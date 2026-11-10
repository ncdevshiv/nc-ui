//! Command implementations.
//!
//! Each function takes the parsed [`Command`] and produces an [`Outcome`]. Keeping them
//! pure with respect to argument parsing means every command is directly testable without
//! spawning a process.

use std::path::PathBuf;

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
    // `run` owns its own runtime for the whole life of the application, so it is handled
    // before the shared runtime is started. The runtime the other commands use is torn
    // down when they finish, which is exactly wrong for a window that is meant to stay
    // open.
    if let Command::Run {
        directory,
        watch,
        url,
    } = command
    {
        return match run_app(directory, *watch, url.as_deref()) {
            Ok(payload) => Outcome::Ok {
                payload,
                format,
                failed: false,
            },
            Err(message) => Outcome::Err(message),
        };
    }

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
        Command::Build {
            directory,
            out,
            binary,
            check,
            installer,
        } => build(
            directory,
            out.as_deref(),
            binary.as_deref(),
            *check,
            *installer,
        ),
        Command::Install { bundle, root } => install(bundle, root.as_deref()),
        Command::Update { bundle, from, root } => {
            update(bundle.as_deref(), from.as_deref(), root.as_deref())
        }
        Command::Uninstall { name, root } => uninstall(name, root.as_deref()),
        Command::Installed { root } => installed(root.as_deref()),
        Command::Launch { name, root, link } => launch(name, root.as_deref(), link.as_deref()),
        // `run` returned above; this arm exists so the match is total.
        Command::Run { .. } => Err("the application was not run".to_owned()),
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

/// Runs an application: serves its assets and opens its window.
///
/// The application is a directory with a manifest in it. Its assets are served over
/// loopback HTTP so the page has a real origin, the window is opened at its entry page,
/// and the page's calls are served until the window closes.
///
/// Without the `wry-host` feature there is no engine to open a window with, and the
/// command says so rather than serving assets nobody will look at.
#[cfg(feature = "wry-host")]
fn run_app(directory: &str, watch: bool, url: Option<&str>) -> Result<Value, String> {
    use std::time::Duration;

    use ncui_runtime::application::Application;

    let directory_path = std::path::Path::new(directory);
    let mut application = Application::launch(directory_path, Limits::default())
        .map_err(|error| format!("cannot run the application: {error}"))?
        .with_link(url.map(str::to_owned));
    if watch {
        application
            .watch()
            .map_err(|error| format!("cannot watch the application's files: {error}"))?;
    }

    // One instance per application. A second `ncui run` of the same application hands its
    // arguments to the instance that is already serving and exits, rather than opening a second
    // window onto the same page. The name is the application's own, so two applications are two
    // instances and two runs of one application are not.
    match ncui_native::InstanceGuard::acquire(&instance_state_dir(), application.name())
        .map_err(|error| format!("cannot establish the application instance: {error}"))?
    {
        ncui_native::Acquired::AlreadyRunning { port, token } => {
            let arguments: Vec<String> = std::env::args().skip(1).collect();
            ncui_native::hand_over(port, &token, &arguments)
                .map_err(|error| format!("cannot reach the running instance: {error}"))?;
            return Ok(json!({
                "application": application.name(),
                "instance": "already_running",
                "handed_over_to": port,
                "arguments": arguments,
                "closed": true,
            }));
        }
        ncui_native::Acquired::First(guard) => {
            application = application.with_instance(*guard);
        }
    }

    // The URL is written to stderr before the window opens: it is diagnostic output for a
    // long-running command, and the payload the command returns at the end says the same
    // thing.
    {
        use std::io::Write as _;
        let stderr = std::io::stderr();
        let mut out = stderr.lock();
        let _ = writeln!(out, "ncui serving {}", application.url());
    }

    // The page loop. The runtime answers the page's calls from here, and the loop ends
    // when the window does: the engine reports that it is gone, and the window stops
    // serving.
    let mut served = 0usize;
    while application.is_running() {
        // The wait is the bridge's idle path: a page's call is answered when the engine
        // delivers it, not a frame later. The timeout is what lets the loop notice a window
        // that has closed.
        served += application.pump_blocking(Duration::from_millis(50));
    }
    // One last pump, so a message that arrived while the window was closing is still
    // answered.
    served += application.pump();

    if let Some(link) = url {
        // The first instance's own link. A second launch's link arrives through the hand-off
        // instead, and both are reported the same way: a link is something the user asked for,
        // and a launch that silently ignored it would leave them looking at a window with no
        // explanation. It goes to stderr with the serving line, because that is where a
        // long-running command's diagnostics already are.
        use std::io::Write as _;
        let stderr = std::io::stderr();
        let mut out = stderr.lock();
        let _ = writeln!(out, "ncui opened by {link}");
    }
    let url = application.url().to_owned();
    let name = application.name().to_owned();
    let version = application.version().to_owned();
    let window = application.window().to_string();
    let reloads = application.reloads();
    let handoffs = application.handoffs();
    application.shutdown();

    Ok(json!({
        "application": name,
        "version": version,
        "url": url,
        "window": window,
        "page_calls_served": served,
        "reloads": reloads,
        "handoffs": handoffs,
        "instance": "first",
        "link": url,
        "closed": true,
    }))
}

/// Where an application's instance lock lives.
///
/// The system's scratch directory rather than the application's own: a bundled application may sit in a
/// directory its user cannot write, and a lock is per-user state rather than part of the
/// application. The token in the lock is what stops another local process from using the port,
/// so a directory that is shared between users is not a shared authority.
#[cfg(feature = "wry-host")]
fn instance_state_dir() -> PathBuf {
    std::env::temp_dir().join("ncui-instances")
}

/// Builds a shippable bundle from an application directory.
///
/// The bundle is written where `--out` says, or under `dist/<name>` beside the workspace. `--check`
/// writes nothing: it builds into a private directory and compares the result with the bundle
/// already in `--out`, which is how a stale bundle is caught rather than shipped.
pub(crate) fn build(
    directory: &str,
    out: Option<&str>,
    binary: Option<&str>,
    check: bool,
    installer: bool,
) -> Result<(Value, bool), String> {
    let app = PathBuf::from(directory);
    let binary = match binary {
        Some(named) => PathBuf::from(named),
        None => crate::bundle::default_binary(&workspace_root()?),
    };
    let out = match out {
        Some(named) => PathBuf::from(named),
        None => {
            let manifest = ncui_app::AppManifest::load(&app)
                .map_err(|error| format!("{directory} is not an application: {error}"))?;
            workspace_root()?.join("dist").join(&manifest.name)
        }
    };

    if check {
        let bundle =
            crate::bundle::check(&app, &binary, &out).map_err(|error| error.to_string())?;
        // A stale installer is as wrong as a stale bundle: the check compares the one in `--out`
        // with what a fresh build would write, byte for byte.
        if installer {
            let expected = crate::installer::pack(&out).map_err(|error| error.to_string())?;
            let path = crate::installer::installer_path(&out, &bundle.name);
            let bytes = std::fs::read(&path).map_err(|error| {
                format!("the installer {} cannot be read: {error}", path.display())
            })?;
            let carried = crate::installer::read(&bytes)
                .ok_or_else(|| {
                    format!(
                        "{} is not an installer: it carries no bundle",
                        path.display()
                    )
                })?
                .map_err(|error| error.to_string())?;
            if carried != expected {
                return Err(format!(
                    "{} carries a bundle that is not this tree's: rebuild it",
                    path.display()
                ));
            }
        }
        return Ok((
            json!({
                "name": bundle.name,
                "version": bundle.version,
                "identity": bundle.identity,
                "out": out.to_string_lossy(),
                "files": bundle.files.len(),
                "checked": true,
                "installer": installer.then(|| {
                    crate::installer::installer_path(&out, &bundle.name)
                        .to_string_lossy()
                        .into_owned()
                }),
            }),
            false,
        ));
    }

    let bundle = crate::bundle::build(&app, &binary, &out).map_err(|error| error.to_string())?;
    // The installer is written after the bundle, from the bundle: one file a user can run, made of
    // this runtime with the bundle appended.
    let written = if installer {
        Some(
            crate::installer::write(&out, &binary, &out)
                .map_err(|error| error.to_string())?
                .to_string_lossy()
                .into_owned(),
        )
    } else {
        None
    };
    Ok((
        json!({
            "name": bundle.name,
            "version": bundle.version,
            "identity": bundle.identity,
            "scheme": bundle.scheme,
            "runtime": bundle.runtime,
            "out": out.to_string_lossy(),
            "files": bundle.files.len(),
            "checked": false,
            "installer": written,
        }),
        false,
    ))
}

/// The directory the workspace's own release build lands under.
///
/// Derived from the executable's location rather than from the working directory: `ncui build` is
/// run from wherever the caller happens to be, and a bundle that depended on that would be a bundle
/// that could not be built from a script.
fn workspace_root() -> Result<PathBuf, String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    for ancestor in executable.ancestors() {
        if ancestor.join("Cargo.toml").is_file() {
            return Ok(ancestor.to_path_buf());
        }
    }
    std::env::current_dir().map_err(|error| error.to_string())
}

/// Installs a bundle for this user.
///
/// The root is the user's own applications directory unless one is named, which is what makes an
/// install something anybody can do: an install that needed an administrator to write one
/// directory would be an install most people cannot perform.
pub(crate) fn install(bundle: &str, root: Option<&str>) -> Result<(Value, bool), String> {
    let root = root.map_or_else(ncui_native::default_root, PathBuf::from);
    let installer = ncui_native::Installer::new(&root)
        .map_err(|error| format!("the applications directory is unusable: {error}"))?;
    let installed = installer
        .install(std::path::Path::new(bundle))
        .map_err(|error| error.to_string())?;
    let payload = serde_json::json!({
        "installed": installed,
        "root": installer.root().display().to_string(),
    });
    Ok((payload, false))
}

/// Replaces an installed application with another version of itself.
///
/// The name comes from the bundle rather than from the caller: an update that took a name and a
/// directory could replace one application with another, and the name a bundle carries is the only
/// thing that says which application it is.
pub(crate) fn update(
    bundle: Option<&str>,
    from: Option<&str>,
    root: Option<&str>,
) -> Result<(Value, bool), String> {
    match (bundle, from) {
        (Some(_), Some(_)) => Err(
            "name either the directory of the bundle to update from or a URL, not both".to_owned(),
        ),
        (None, None) => Err(
            "an update needs the directory of the bundle to update from, or `--from <url>`"
                .to_owned(),
        ),
        (Some(bundle), None) => update_from_directory(bundle, root),
        (None, Some(from)) => update_from_url(from, root),
    }
}

/// Replaces an installed application with a bundle that is on disk.
fn update_from_directory(bundle: &str, root: Option<&str>) -> Result<(Value, bool), String> {
    let root = root.map_or_else(ncui_native::default_root, PathBuf::from);
    let installer = ncui_native::Installer::new(&root)
        .map_err(|error| format!("the applications directory is unusable: {error}"))?;
    let updated = installer
        .update(std::path::Path::new(bundle))
        .map_err(|error| error.to_string())?;
    let payload = serde_json::json!({
        "name": updated.installed.name,
        "from_version": updated.previous.version,
        "to_version": updated.installed.version,
        "directory": updated.installed.directory,
        "registrations": updated.installed.registrations,
        "root": installer.root().display().to_string(),
    });
    Ok((payload, false))
}

/// Replaces an installed application with the release a URL describes.
///
/// The document is fetched first, so the application knows which version it is being offered and
/// which application the release is for before anything large is downloaded; then the bundle, which
/// is checked against the digest the document named; then the installer checks the bundle's own
/// records, file by file. A release for another application, or one whose download is not what was
/// promised, is refused with the reason.
fn update_from_url(url: &str, root: Option<&str>) -> Result<(Value, bool), String> {
    let root = root.map_or_else(ncui_native::default_root, PathBuf::from);
    let installer = ncui_native::Installer::new(&root)
        .map_err(|error| format!("the applications directory is unusable: {error}"))?;
    let scratch = crate::installer::private_directory().map_err(|error| error.to_string())?;

    // The document, bounded by what a document may be.
    let document_path = scratch.path().join("release.json");
    crate::release::fetch(url, crate::release::MAX_RELEASE_BYTES, &document_path)
        .map_err(|error| error.to_string())?;
    let document_bytes = std::fs::read(&document_path)
        .map_err(|error| format!("{}: {error}", document_path.display()))?;
    let document = crate::release::release(&document_bytes).map_err(|error| error.to_string())?;

    // The application this update is for is the one the release names: an update is not a way to
    // install something else over what the user has.
    let installed = installer
        .installed()
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|installed| installed.name == document.name)
        .ok_or_else(|| {
            format!(
                "`{}` is not installed here, and an update replaces what is",
                document.name
            )
        })?;

    // The bundle, bounded by what a bundle may be.
    let archive_path = scratch.path().join("bundle");
    crate::release::fetch(
        &document.url,
        crate::release::MAX_BUNDLE_BYTES,
        &archive_path,
    )
    .map_err(|error| error.to_string())?;
    let updated = crate::release::apply(&root, &installed.name, &document, &archive_path)
        .map_err(|error| error.to_string())?;

    let payload = serde_json::json!({
        "name": updated.installed.name,
        "from_version": updated.previous.version,
        "to_version": updated.installed.version,
        "directory": updated.installed.directory,
        "registrations": updated.installed.registrations,
        "release": url,
        "root": installer.root().display().to_string(),
    });
    Ok((payload, false))
}

/// Removes an installed application, with the registrations it made.
pub(crate) fn uninstall(name: &str, root: Option<&str>) -> Result<(Value, bool), String> {
    let root = root.map_or_else(ncui_native::default_root, PathBuf::from);
    let installer = ncui_native::Installer::new(&root)
        .map_err(|error| format!("the applications directory is unusable: {error}"))?;
    let removed = installer
        .uninstall(name)
        .map_err(|error| error.to_string())?;
    let payload = serde_json::json!({
        "removed": removed,
        "root": installer.root().display().to_string(),
    });
    Ok((payload, false))
}

/// Runs an installed application by name.
///
/// The name is resolved to the directory the install recorded, and the application is then run
/// exactly as `ncui run` would run it — the same single-instance claim, the same remembered
/// window, the same link handling. A second way of starting an application would be a second set
/// of behaviours to keep in step.
fn launch(name: &str, root: Option<&str>, link: Option<&str>) -> Result<(Value, bool), String> {
    let root = root.map_or_else(ncui_native::default_root, PathBuf::from);
    let installed = resolve_installed(name, &root)?;
    run_app(&installed.directory, false, link).map(|payload| (payload, false))
}

/// Finds the installed application called `name` under `root`.
pub(crate) fn resolve_installed(
    name: &str,
    root: &std::path::Path,
) -> Result<ncui_native::Installed, String> {
    let installer = ncui_native::Installer::new(root)
        .map_err(|error| format!("the applications directory is unusable: {error}"))?;
    let applications = installer
        .installed()
        .map_err(|error| format!("the installed applications could not be listed: {error}"))?;
    applications
        .into_iter()
        .find(|application| application.name == name)
        .ok_or_else(|| {
            format!(
                "`{name}` is not installed under {}; `ncui installed` lists what is",
                root.display()
            )
        })
}

/// Lists what is installed for this user.
pub(crate) fn installed(root: Option<&str>) -> Result<(Value, bool), String> {
    let root = root.map_or_else(ncui_native::default_root, PathBuf::from);
    let installer = ncui_native::Installer::new(&root)
        .map_err(|error| format!("the applications directory is unusable: {error}"))?;
    let applications = installer.installed().map_err(|error| error.to_string())?;
    let payload = serde_json::json!({
        "root": installer.root().display().to_string(),
        "count": applications.len(),
        "applications": applications,
    });
    Ok((payload, false))
}

/// Runs an application in a build with no window host.
///
/// The manifest is still read, so a user whose application is broken hears about the
/// manifest rather than about the build. The refusal itself is the build's.
#[cfg(not(feature = "wry-host"))]
fn run_app(directory: &str, _watch: bool, _url: Option<&str>) -> Result<Value, String> {
    use ncui_app::AppManifest;

    let directory_path = std::path::Path::new(directory);
    AppManifest::load(directory_path).map_err(|error| error.to_string())?;
    Err(
        "this build has no window host, so an application cannot be opened; rebuild ncui \
         with the `wry-host` feature"
            .to_owned(),
    )
}

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

    // The window host. A runtime with a host can open windows; one without refuses every
    // window operation rather than inventing windows. The feature is off by default, so a
    // build without it says so rather than pretending.
    let runtime = {
        // The `mut` is only needed when the window host is installed, because installing
        // it consumes the runtime and returns a new one. The attribute carries a reason
        // because the workspace requires one.
        #[cfg_attr(
            not(feature = "wry-host"),
            allow(
                unused_mut,
                reason = "the runtime is only replaced when a host is installed"
            )
        )]
        let mut runtime =
            Runtime::start(limits).map_err(|error| format!("runtime failed to start: {error}"))?;
        #[cfg(feature = "wry-host")]
        {
            runtime = runtime
                .with_host(&ncui_runtime::window::hosted_origins())
                .map_err(|error| format!("cannot install the window host: {error}"))?;
        }
        #[cfg(not(feature = "wry-host"))]
        {
            // Without the feature there is no engine to install. The refusal is the
            // runtime's own, and the command says so below.
        }
        std::sync::Arc::new(std::sync::Mutex::new(runtime))
    };
    // `from_os_entropy` reaches the kernel CSPRNG on Unix directly and, on Windows, through
    // the `getrandom` crate — which performs the BCryptGenRandom FFI so this workspace's own
    // code stays free of `unsafe`. A failure here is a real failure and is reported as one
    // rather than being replaced with something weaker.
    let token = SecretToken::from_os_entropy(32).map_err(|error| {
        format!(
            "cannot obtain an operating-system token: {error}. This build has no OS entropy \
             source on this platform; supply one explicitly rather than falling back to \
             something predictable."
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
/// The `scope` field is not decoration. These commands inspect a *local* runtime, and a
/// reader must not mistake one for a runtime reached over the socket — `ncui serve` is
/// the command that serves one, and it prints the address and the token it bound.
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
            "scope_note": "inspects an in-process runtime; `ncui serve` serves one over the loopback socket",
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
