//! Command implementations.
//!
//! Each function takes the parsed [`Command`] and produces an [`Outcome`]. Keeping them
//! pure with respect to argument parsing means every command is directly testable without
//! spawning a process.

use serde_json::{Value, json};

use ncui_core::Limits;
use ncui_runtime::Runtime;
use ncui_runtime::describe::{self, MethodDescription};
use ncui_runtime::dispatch::method_table;
use ncui_security::authz::AgentRole;
use ncui_security::token::SecretToken;

use crate::{Command, Format, Outcome};

/// Runs one command against a freshly started local runtime.
///
/// # Errors
///
/// Returns an [`Outcome::Err`] when the command cannot run. The command never panics and
/// never prints: rendering is the caller's job, so a test can assert on structure rather
/// than on captured stdout.
pub fn run(command: &Command, format: Format) -> Outcome {
    let runtime = match Runtime::start(Limits::default()) {
        Ok(runtime) => runtime,
        Err(error) => return Outcome::Err(format!("runtime failed to start: {error}")),
    };
    let outcome = match command {
        Command::Version => Ok((version(), false)),
        Command::Health { deep } => health(&runtime, *deep),
        Command::Describe => describe_command(),
        Command::Capabilities => Ok((capabilities(), false)),
        Command::Roles => Ok((roles(), false)),
        Command::Status { registry } => Ok((status(registry.as_ref()), false)),
        Command::Serve { role, registry } => serve(role.as_deref(), registry.clone()),
        Command::Help => Ok((Value::Null, false)),
    };
    runtime.shutdown();

    match outcome {
        Ok((payload, failed)) => Outcome::Ok {
            payload,
            format,
            failed,
        },
        Err(message) => Outcome::Err(message),
    }
}

type CommandResult = Result<(Value, bool), String>;

fn version() -> Value {
    json!({
            "runtime_version": ncui_runtime::RUNTIME_VERSION,
            "api_version": ncui_runtime::rpc::API_VERSION,
            "jsonrpc": ncui_runtime::rpc::JSONRPC_VERSION,
            "platform": std::env::consts::OS,
        "architecture": std::env::consts::ARCH,
    })
}

fn health(runtime: &Runtime, deep: bool) -> CommandResult {
    let report = ncui_runtime::health::report(runtime, deep);
    let payload = serde_json::to_value(&report)
        .map_err(|e| format!("health report is not serialisable: {e}"))?;
    // A runtime that is not healthy must be able to fail a CI job or trip a probe. The
    // report is still returned, because an operator needs to see *why*.
    Ok((
        payload,
        report.status != ncui_runtime::health::OverallHealth::Healthy,
    ))
}

/// Serves the agent API on loopback.
///
/// The token is generated here rather than read from configuration because this build has
/// no secret store yet (ADR-007 records OS entropy as unavailable without FFI). It is
/// printed once, on stderr, and never logged or stored — a token written to a log file is a
/// token in every backup of that file.
fn serve(role: Option<&str>, registry: Option<Value>) -> CommandResult {
    let limits = Limits::default();
    let role = match role {
        None => AgentRole::Observer,
        Some(name) => AgentRole::ALL
            .iter()
            .copied()
            .find(|candidate| candidate.as_str() == name)
            .ok_or_else(|| {
                let names: Vec<&str> = AgentRole::ALL.iter().map(|r| r.as_str()).collect();
                format!("unknown role `{name}`; known roles: {}", names.join(", "))
            })?,
    };

    let runtime = std::sync::Arc::new(std::sync::Mutex::new(
        Runtime::start(limits).map_err(|error| format!("runtime failed to start: {error}"))?,
    ));
    // `from_os_entropy` reaches the kernel CSPRNG on Unix directly and, on Windows, through
    // the `getrandom` crate — which performs the BCryptGenRandom FFI so this workspace's own
    // code stays free of `unsafe`. A failure here is a real failure and is reported as one
    // rather than being replaced with something weaker.
    let token = SecretToken::from_os_entropy(32).map_err(|error| {
        format!(
            "cannot obtain an operating-system token: {error}. This build has no OS entropy              source on this platform; supply one explicitly rather than falling back to              something predictable."
        )
    })?;
    // Read the fingerprint first: `SecretToken` is moved into the server and is
    // deliberately neither `Clone` nor `Copy`.
    let fingerprint = ncui_runtime::transport::hex_encode(token.expose_secret());
    let server = ncui_runtime::transport::AgentServer::bind_with_registry(
        std::sync::Arc::clone(&runtime),
        token,
        role,
        limits,
        registry,
    )
    .map_err(|error| format!("cannot bind the agent socket: {error}"))?;

    let addr = server.local_addr();
    let stderr = std::io::stderr();
    {
        use std::io::Write as _;
        let mut out = stderr.lock();
        let _ = writeln!(out, "ncui agent API listening on {addr}");
        let _ = writeln!(out, "role: {}", role.as_str());
        let _ = writeln!(
            out,
            "bearer token (shown once, never logged): {fingerprint}"
        );
    }

    let result = server.serve();
    let _ = runtime.lock().map(|guard| guard.shutdown());
    result.map_err(|error| format!("agent server stopped: {error}"))?;

    Ok((json!({ "served_until_shutdown": true }), false))
}

fn describe_command() -> CommandResult {
    let table: Vec<MethodDescription> = method_table();
    let description = describe::describe(ncui_core::CapabilitySet::all(), &table);
    serde_json::to_value(description)
        .map(|value| (value, false))
        .map_err(|e| format!("description is not serialisable: {e}"))
}

fn capabilities() -> Value {
    use ncui_core::{Capability, CapabilityScope};
    let scopes = [
        CapabilityScope::Runtime,
        CapabilityScope::Workspace,
        CapabilityScope::Agent,
    ];
    let groups: Vec<Value> = scopes
        .iter()
        .map(|scope| {
            let names: Vec<&str> = describe::capability_names(*scope);
            json!({ "scope": scope.as_str(), "capabilities": names })
        })
        .collect();
    json!({ "total": Capability::ALL.len(), "scopes": groups })
}

fn roles() -> Value {
    use ncui_security::authz::AgentRole;
    let described: Vec<Value> = AgentRole::ALL
        .iter()
        .map(|role| {
            json!({
                "role": role.as_str(),
                "capabilities": role.capabilities(),
                "has_destructive": role.has_destructive(),
            })
        })
        .collect();
    json!({ "roles": described })
}

/// Reports the truth layer: what is complete, and what is honestly absent.
///
/// The `scope` field is not decoration. These commands inspect a *local* runtime because
/// the transport that would let them reach a running application is not implemented, and
/// a reader must not mistake one for the other.
fn status(registry: Option<&Value>) -> Value {
    let mut complete: Vec<&str> = Vec::new();
    let mut absent: Vec<&str> = Vec::new();

    if let Some(registry) = registry
        && let Some(features) = registry.get("features").and_then(Value::as_array)
    {
        for feature in features {
            let Some(id) = feature.get("id").and_then(Value::as_str) else {
                continue;
            };
            if feature.get("status").and_then(Value::as_str) == Some("complete") {
                complete.push(id);
            } else {
                absent.push(id);
            }
        }
    }
    complete.sort_unstable();
    absent.sort_unstable();

    json!({
            "runtime_version": ncui_runtime::RUNTIME_VERSION,
            "scope": "local",
            "scope_note": "inspects an in-process runtime; remote attachment is not implemented",
            "features_complete": complete,
            "features_absent": absent,
            "registry_loaded": registry.is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_reports_the_build_and_the_wire_versions() {
        let outcome = run(&Command::Version, Format::Json);
        assert!(outcome.is_ok());
        let Outcome::Ok { payload, .. } = outcome else {
            panic!("expected success");
        };
        assert_eq!(payload["runtime_version"], ncui_runtime::RUNTIME_VERSION);
        assert_eq!(payload["jsonrpc"], "2.0");
        assert!(payload["platform"].is_string());
    }

    #[test]
    fn health_succeeds_on_a_fresh_runtime() {
        let outcome = run(&Command::Health { deep: true }, Format::Json);
        let Outcome::Ok {
            payload, failed, ..
        } = outcome
        else {
            panic!("expected success");
        };
        assert!(!failed, "a fresh runtime must not report failure");
        assert_eq!(payload["status"], "healthy");
        assert_eq!(payload["deep"], true);
    }

    #[test]
    fn describe_lists_every_advertised_method() {
        let outcome = run(&Command::Describe, Format::Json);
        let Outcome::Ok { payload, .. } = outcome else {
            panic!("expected success");
        };
        let methods = payload["methods"].as_array().expect("methods array");
        assert_eq!(methods.len(), method_table().len());
    }

    #[test]
    fn capabilities_cover_every_capability_exactly_once() {
        let outcome = run(&Command::Capabilities, Format::Json);
        let Outcome::Ok { payload, .. } = outcome else {
            panic!("expected success");
        };
        assert_eq!(
            payload["total"].as_u64().expect("total"),
            ncui_core::Capability::ALL.len() as u64
        );
        let scopes = payload["scopes"].as_array().expect("scopes");
        let summed: usize = scopes
            .iter()
            .map(|s| s["capabilities"].as_array().expect("list").len())
            .sum();
        assert_eq!(summed, ncui_core::Capability::ALL.len());
    }

    #[test]
    fn roles_are_listed_from_least_to_most_privileged() {
        let outcome = run(&Command::Roles, Format::Json);
        let Outcome::Ok { payload, .. } = outcome else {
            panic!("expected success");
        };
        let roles = payload["roles"].as_array().expect("roles");
        assert_eq!(roles.len(), AgentRole::ALL.len());
        assert_eq!(roles[0]["role"], "observer");
        assert_eq!(
            roles[0]["has_destructive"].as_bool(),
            Some(false),
            "an observer must not be reported as holding destructive authority"
        );
    }

    #[test]
    fn status_without_a_registry_reports_that_none_was_loaded() {
        let outcome = run(&Command::Status { registry: None }, Format::Json);
        let Outcome::Ok { payload, .. } = outcome else {
            panic!("expected success");
        };
        assert_eq!(payload["registry_loaded"], false);
        assert_eq!(
            payload["features_complete"].as_array().expect("list").len(),
            0
        );
    }

    #[test]
    fn status_separates_complete_from_absent_features() {
        let registry = json!({
            "schema_version": 1,
            "features": [
                { "id": "ipc.framing", "status": "complete" },
                { "id": "gpu.islands", "status": "not_implemented" },
                { "id": "core.errors", "status": "complete" },
            ]
        });
        let outcome = run(
            &Command::Status {
                registry: Some(registry),
            },
            Format::Json,
        );
        let Outcome::Ok { payload, .. } = outcome else {
            panic!("expected success");
        };
        assert_eq!(
            payload["features_complete"],
            json!(["core.errors", "ipc.framing"])
        );
        assert_eq!(payload["features_absent"], json!(["gpu.islands"]));
        assert_eq!(payload["registry_loaded"], true);
    }

    #[test]
    fn status_declares_that_its_scope_is_local() {
        let outcome = run(&Command::Status { registry: None }, Format::Json);
        let Outcome::Ok { payload, .. } = outcome else {
            panic!("expected success");
        };
        assert_eq!(
            payload["scope"], "local",
            "a local inspection must not be presented as a remote one"
        );
    }
}
