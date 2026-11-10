//! Rendering an [`Outcome`] as text or JSON.
//!
//! Rendering returns a `String` rather than writing to a stream, so the exact bytes a
//! caller receives can be asserted in a test.
//!
//! Every command has a real text rendering. Falling back to JSON for commands that have no
//! text form would mean `ncui version` silently printing a JSON document when the user
//! asked for text, which reads as a bug even when the content is right.

use core::fmt::Write as _;

use serde_json::Value;

use ncui_core::{Capability, CapabilitySet};
use ncui_runtime::health::HealthReport;
use ncui_runtime::rpc::Response;

use crate::{Command, Format, Outcome};

/// Renders an outcome for `command` in the requested format.
///
/// Returns a string that always ends in a newline.
#[must_use]
pub fn render(outcome: &Outcome, command: &Command) -> String {
    match outcome {
        Outcome::Ok {
            payload,
            format,
            failed: _,
        } => match format {
            Format::Json => to_json(payload),
            Format::Text => to_text(payload, command),
        },
        // An error is always rendered as text: it is a message for whoever ran the
        // command, and a JSON wrapper would only make a failure harder to read.
        Outcome::Err(message) => format!("ncui: {message}\n"),
    }
}

fn to_json(payload: &Value) -> String {
    let mut text = serde_json::to_string_pretty(payload)
        .unwrap_or_else(|_| "{\"error\":\"unserialisable\"}".to_owned());
    text.push('\n');
    text
}

fn to_text(payload: &Value, command: &Command) -> String {
    match command {
        Command::Version => version_text(payload),
        Command::Health { .. } => health_payload_text(payload),
        Command::Describe => describe_text(payload),
        Command::Capabilities => capabilities_text(payload),
        Command::Roles => roles_text(payload),
        Command::Status { .. } => status_text(payload),
        Command::Serve { .. } => to_json(payload),
        // `launch` runs an application, so its output is the run's: one shape for one event,
        // whether the application was named by directory or by the name it was installed under.
        Command::Run { .. } | Command::Launch { .. } => run_text(payload),
        Command::Build { check, .. } => build_text(payload, *check),
        Command::Update { .. } => update_text(payload),
        Command::Install { .. } => install_text(payload),
        Command::Uninstall { .. } => uninstall_text(payload),
        Command::Installed { .. } => installed_text(payload),
        Command::Help => crate::usage::text(),
    }
}

/// The text form of an install.
fn install_text(payload: &Value) -> String {
    let mut out = String::new();
    let installed = payload.get("installed").cloned().unwrap_or(Value::Null);
    let _ = writeln!(
        out,
        "{} {}",
        field(&installed, "name"),
        field(&installed, "version")
    );
    let _ = writeln!(out, "into   {}", field(&installed, "directory"));
    let _ = writeln!(out, "run    {}", field(&installed, "program"));
    for registration in installed
        .get("registrations")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
    {
        let _ = writeln!(out, "       registered {registration}");
    }
    out
}

/// The text form of an uninstall.
fn uninstall_text(payload: &Value) -> String {
    let removed = payload.get("removed").cloned().unwrap_or(Value::Null);
    format!(
        "removed {} from {}\n",
        field(&removed, "name"),
        field(&removed, "directory")
    )
}

/// The text form of the list of installed applications.
fn installed_text(payload: &Value) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}", field(payload, "root"));
    let empty = Vec::new();
    let applications = payload
        .get("applications")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    if applications.is_empty() {
        let _ = writeln!(out, "nothing is installed here");
        return out;
    }
    for application in applications {
        let _ = writeln!(
            out,
            "{} {}  {}",
            field(application, "name"),
            field(application, "version"),
            field(application, "directory")
        );
    }
    out
}

/// The text form of a finished application run.
fn run_text(payload: &Value) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} {}",
        field(payload, "application"),
        field(payload, "version")
    );
    let _ = writeln!(out, "url    {}", field(payload, "url"));
    let _ = writeln!(out, "window {}", field(payload, "window"));
    let _ = writeln!(out, "reloads {}", field(payload, "reloads"));
    let _ = writeln!(
        out,
        "served {} page calls",
        field(payload, "page_calls_served")
    );
    out
}

/// Renders a build or a check.
///
/// A check that passed says so rather than printing the bundle again: the caller asked whether the
/// bundle is current, and the answer is the point.
fn build_text(payload: &Value, check: bool) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} {}",
        field(payload, "name"),
        field(payload, "version")
    );
    let _ = writeln!(out, "identity {}", field(payload, "identity"));
    if !check {
        let _ = writeln!(out, "runtime  {}", field(payload, "runtime"));
    }
    let _ = writeln!(out, "out      {}", field(payload, "out"));
    let _ = writeln!(
        out,
        "{} {} files",
        if check { "matches " } else { "wrote   " },
        field(payload, "files")
    );
    // The installer is only there when `--installer` asked for one, and a build that names it is
    // telling the person what to hand to the user.
    if let Some(installer) = payload.get("installer").and_then(Value::as_str) {
        let _ = writeln!(out, "installer {installer}");
    }
    out
}

fn update_text(payload: &Value) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} {} -> {}",
        field(payload, "name"),
        field(payload, "from_version"),
        field(payload, "to_version")
    );
    let _ = writeln!(out, "installed {}", field(payload, "directory"));
    if let Some(registrations) = payload.get("registrations").and_then(Value::as_array) {
        for registration in registrations {
            if let Some(text) = registration.as_str() {
                let _ = writeln!(out, "  {text}");
            }
        }
    }
    out
}

fn version_text(payload: &Value) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "ncui {}", field(payload, "runtime_version"));
    let _ = writeln!(out, "agent api   {}", field(payload, "api_version"));
    let _ = writeln!(out, "jsonrpc     {}", field(payload, "jsonrpc"));
    let _ = writeln!(
        out,
        "platform    {} ({})",
        field(payload, "platform"),
        field(payload, "architecture")
    );
    out
}

fn describe_text(payload: &Value) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "ncui {} on {} ({})",
        field(payload, "runtime_version"),
        field(payload, "platform"),
        field(payload, "architecture")
    );
    let methods = payload
        .get("methods")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let roles = payload
        .get("roles")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let _ = writeln!(out, "\n{methods} methods, {roles} agent roles:");
    if let Some(methods) = payload.get("methods").and_then(Value::as_array) {
        for method in methods {
            let requires = method
                .get("requires")
                .and_then(Value::as_array)
                .map_or_else(String::new, |names| {
                    let joined: Vec<&str> = names.iter().filter_map(Value::as_str).collect();
                    if joined.is_empty() {
                        "any authenticated agent".to_owned()
                    } else {
                        joined.join(", ")
                    }
                });
            let destructive = if method.get("destructive").and_then(Value::as_bool) == Some(true) {
                "  [destructive]"
            } else {
                ""
            };
            let _ = writeln!(
                out,
                "  {:<26} requires: {requires}{destructive}",
                field(method, "name")
            );
        }
    }
    out
}

fn capabilities_text(payload: &Value) -> String {
    let mut out = String::new();
    let total = payload.get("total").and_then(Value::as_u64).unwrap_or(0);
    let _ = writeln!(out, "{total} capabilities:");
    if let Some(scopes) = payload.get("scopes").and_then(Value::as_array) {
        for scope in scopes {
            let _ = writeln!(out, "\n  [{}]", field(scope, "scope"));
            if let Some(names) = scope.get("capabilities").and_then(Value::as_array) {
                for name in names.iter().filter_map(Value::as_str) {
                    let _ = writeln!(out, "    {name}");
                }
            }
        }
    }
    out
}

fn roles_text(payload: &Value) -> String {
    let mut out = String::new();
    if let Some(roles) = payload.get("roles").and_then(Value::as_array) {
        for role in roles {
            let destructive = if role.get("has_destructive").and_then(Value::as_bool) == Some(true)
            {
                " (includes destructive authority)"
            } else {
                ""
            };
            let _ = writeln!(out, "{}{destructive}:", field(role, "role"));
            let names: Vec<&str> = role
                .get("capabilities")
                .and_then(Value::as_array)
                .map_or_else(Vec::new, |items| {
                    items.iter().filter_map(Value::as_str).collect()
                });
            for name in names {
                let _ = writeln!(out, "    {name}");
            }
            out.push('\n');
        }
    }
    out
}

fn status_text(payload: &Value) -> String {
    let complete = payload
        .get("features_complete")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let absent = payload
        .get("features_absent")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let loaded = payload
        .get("registry_loaded")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut out = String::new();
    let _ = writeln!(out, "ncui {}", field(payload, "runtime_version"));
    let _ = writeln!(out, "scope: {}", field(payload, "scope"));
    let _ = writeln!(out, "  {}", field(payload, "scope_note"));
    if loaded {
        let _ = writeln!(out, "\n{complete} complete, {absent} not implemented:");
        for id in payload
            .get("features_complete")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let _ = writeln!(out, "  [x] {id}");
        }
        for id in payload
            .get("features_absent")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let _ = writeln!(out, "  [ ] {id}");
        }
    } else {
        let _ = writeln!(
            out,
            "\nno feature registry was loaded; pass --registry <path> for a real report"
        );
    }
    out
}

/// Reads a field, rendering a string field directly and any other shape as JSON.
fn field(value: &Value, name: &str) -> String {
    match value.get(name) {
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None => "(absent)".to_owned(),
    }
}

/// Renders a health report as indented text.
#[must_use]
pub fn health_text(report: &HealthReport) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        "status: {}\nlive: {}\nready: {}\n",
        report.status.as_str(),
        report.live,
        report.ready
    );
    for check in &report.checks {
        let _ = writeln!(
            out,
            "  {:<10} {:<7} {}",
            check.name,
            format!("{:?}", check.status).to_lowercase(),
            check.detail
        );
    }
    out
}

fn health_payload_text(payload: &Value) -> String {
    match serde_json::from_value::<HealthReport>(payload.clone()) {
        Ok(report) => health_text(&report),
        // A payload that is not a health report should not be silently reshaped; showing
        // the raw value makes the mismatch obvious instead of hiding it.
        Err(_) => to_json(payload),
    }
}

/// Renders a capability set as a comma-separated list, or `(none)`.
#[must_use]
pub fn capability_list(label: &str, capabilities: CapabilitySet) -> String {
    if capabilities.is_empty() {
        return format!("{label}: (none)\n");
    }
    let names: Vec<&str> = capabilities.iter().map(Capability::as_str).collect();
    format!("{label}: {}\n", names.join(", "))
}

/// Renders a JSON-RPC response as indented text.
#[must_use]
pub fn response_text(response: &Response) -> String {
    if let Some(error) = &response.error {
        let mut out = String::new();
        let _ = write!(
            out,
            "error {}: {}\n  operation: {}\n  retryable: {}\n",
            error.code, error.message, error.data.operation, error.data.retryable
        );
        return out;
    }
    match &response.result {
        Some(result) => to_json(result),
        None => "(no result)\n".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ncui_runtime::agent::AgentSession;
    use ncui_runtime::rpc::Request;
    use ncui_security::authz::AgentRole;
    use serde_json::json;

    fn ok(payload: Value) -> Outcome {
        Outcome::Ok {
            payload,
            format: Format::Text,
            failed: false,
        }
    }

    #[test]
    fn json_output_is_parseable_and_newline_terminated() {
        let outcome = Outcome::Ok {
            payload: json!({"a": 1}),
            format: Format::Json,
            failed: false,
        };
        let text = render(&outcome, &Command::Version);
        assert!(text.ends_with('\n'), "output must end in a newline");
        let parsed: Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(parsed["a"], 1);
    }

    #[test]
    fn text_mode_does_not_fall_back_to_json_for_version() {
        let outcome = ok(json!({
            "runtime_version": "0.1.0",
            "api_version": "0.1.0",
            "jsonrpc": "2.0",
            "platform": "linux",
            "architecture": "x86_64",
        }));
        let text = render(&outcome, &Command::Version);
        assert!(
            !text.trim_start().starts_with('{'),
            "text mode must not emit a JSON document: {text}"
        );
        assert!(text.contains("ncui 0.1.0"), "got: {text}");
        assert!(text.contains("linux (x86_64)"), "got: {text}");
    }

    #[test]
    fn every_command_renders_as_prose_in_text_mode() {
        let cases: Vec<(Command, Value)> = vec![
            (
                Command::Capabilities,
                json!({"total": 1, "scopes": [{"scope": "runtime", "capabilities": ["gpu"]}]}),
            ),
            (
                Command::Roles,
                json!({"roles": [{"role": "observer", "capabilities": ["read.logs"], "has_destructive": false}]}),
            ),
            (
                Command::Status { registry: None },
                json!({
                    "runtime_version": "0.1.0",
                    "scope": "local",
                    "scope_note": "in-process",
                    "features_complete": ["a"],
                    "features_absent": ["b"],
                    "registry_loaded": true,
                }),
            ),
        ];
        for (command, payload) in cases {
            let text = render(&ok(payload), &command);
            assert!(
                !text.trim_start().starts_with('{'),
                "{command:?} fell back to JSON in text mode: {text}"
            );
        }
    }

    #[test]
    fn describe_text_lists_every_method_with_its_requirement() {
        let outcome = ok(json!({
            "runtime_version": "0.1.0",
            "platform": "linux",
            "architecture": "x86_64",
            "methods": [
                {"name": "ncui.health", "requires": [], "destructive": false},
                {"name": "ncui.data.purge", "requires": ["delete.data"], "destructive": true},
            ],
            "roles": [],
        }));
        let text = render(&outcome, &Command::Describe);
        assert!(text.contains("ncui.health"), "got: {text}");
        assert!(text.contains("any authenticated agent"), "got: {text}");
        assert!(text.contains("delete.data"), "got: {text}");
        assert!(text.contains("[destructive]"), "got: {text}");
    }

    #[test]
    fn status_text_says_when_no_registry_was_loaded() {
        let outcome = ok(json!({
            "runtime_version": "0.1.0",
            "scope": "local",
            "scope_note": "in-process",
            "features_complete": [],
            "features_absent": [],
            "registry_loaded": false,
        }));
        let text = render(&outcome, &Command::Status { registry: None });
        assert!(text.contains("--registry"), "got: {text}");
        assert!(text.contains("scope: local"), "got: {text}");
    }

    #[test]
    fn an_error_is_rendered_as_a_readable_message() {
        let text = render(
            &Outcome::Err("something broke".to_owned()),
            &Command::Version,
        );
        assert!(text.contains("something broke"), "got: {text}");
        assert!(text.starts_with("ncui: "), "got: {text}");
    }

    #[test]
    fn a_health_payload_renders_as_text() {
        let runtime = ncui_runtime::Runtime::start(ncui_core::Limits::default()).expect("runtime");
        let payload = serde_json::to_value(ncui_runtime::health::report(&runtime, true))
            .expect("serialisable");
        let text = render(&ok(payload), &Command::Health { deep: true });
        assert!(text.contains("status: healthy"), "got: {text}");
        assert!(text.contains("workers"), "got: {text}");
    }

    #[test]
    fn an_empty_capability_set_renders_explicitly() {
        assert_eq!(
            capability_list("granted", CapabilitySet::empty()),
            "granted: (none)\n"
        );
    }

    #[test]
    fn a_populated_capability_set_lists_its_members() {
        let text = capability_list(
            "granted",
            CapabilitySet::empty()
                .with(Capability::ReadLogs)
                .with(Capability::Gpu),
        );
        assert!(text.contains("read.logs"), "got: {text}");
        assert!(text.contains("gpu"), "got: {text}");
    }

    #[test]
    fn a_response_error_renders_with_its_stable_fields() {
        let mut runtime =
            ncui_runtime::Runtime::start(ncui_core::Limits::default()).expect("runtime");
        let token = ncui_security::token::SecretToken::new(vec![0x11; 32]).expect("token");
        let mut session =
            AgentSession::new("agent", AgentRole::Administrator, 10, 10).expect("session");
        session
            .authenticate(&token, &token)
            .expect("test session authenticates");
        let request = Request::new(json!(1), "ncui.nope", json!({}));
        let response = ncui_runtime::dispatch::dispatch(
            &mut runtime,
            &mut session,
            &request,
            core::time::Duration::from_secs(0),
            None,
        )
        .expect("response");

        let text = response_text(&response);
        assert!(text.contains("error"), "got: {text}");
        assert!(text.contains("operation"), "got: {text}");
    }
}
