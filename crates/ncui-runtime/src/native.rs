//! The native commands a page may call, registered only for the capabilities it holds.
//!
//! This is the consumer half of the native capability seam. The provider is
//! [`ncui_native::FileSystem`], rooted at one directory; this module registers a command
//! for each operation the page's grant actually covers. A command whose capability the
//! grant does not cover is not registered, so a page cannot call what it was not granted,
//! and `describe` does not advertise a name that would be refused.
//!
//! The commands are registered with the capability they require, which means the call
//! still passes through the runtime's authorization step: registering is not the same as
//! allowing, and a grant narrowed after registration is still enforced at the call.
//!
//! # Why the arguments are checked here and again below
//!
//! The payload is untrusted input from a page. It is checked here — a missing or
//! non-string `path` is refused before the filesystem is reached — and the filesystem
//! checks the path again against its root. The two checks answer different questions:
//! "is this a well-formed request" and "is this path inside the root".

use std::path::Path;
use std::sync::Arc;

use ncui_core::{Capability, CapabilitySet, ErrorKind, NcError, Result};
use ncui_native::{
    Clipboard, ClipboardError, FileSystem, FsError, NotificationError, Notifier, OpenError,
    ProcessError, ProcessRunner, RunRequest, open_with_default,
};
use ncui_webview::SeamError;
use serde_json::Value;

use crate::Runtime;
use crate::commands::CommandRegistration;

/// The capabilities this module provides commands for.
///
/// An application grants these in its manifest, and the commands appear. There is no
/// separate switch: the manifest's grant is the whole story.
pub const CAPABILITIES: &[Capability] = &[
    Capability::FilesystemRead,
    Capability::FilesystemWrite,
    Capability::ProcessSpawn,
    Capability::Clipboard,
    Capability::Notifications,
];

/// Registers the filesystem commands the runtime's page grant covers.
///
/// The root is the application's own directory: a page may read and write the
/// application's files, and nothing else.
///
/// # Errors
///
/// Returns an error when the root cannot be used as a filesystem root, or when a
/// registration is refused — which would mean the command table and the grant disagree,
/// and is reported rather than silently dropping a command.
pub fn register(
    runtime: &Runtime,
    root: &Path,
    identity: Option<&str>,
) -> Result<Vec<CommandRegistration>> {
    let mut registrations = Vec::new();
    let granted = runtime.page_grant();
    if !CAPABILITIES
        .iter()
        .any(|capability| granted.contains(*capability))
    {
        return Ok(registrations);
    }

    let metrics = Arc::clone(&runtime.metrics);

    if granted.contains(Capability::Clipboard) {
        let clipboard = Arc::new(Clipboard::new());
        registrations.push(register_clipboard(runtime, &clipboard, "read", &metrics)?);
        registrations.push(register_clipboard(runtime, &clipboard, "write", &metrics)?);
    }
    if granted.contains(Capability::Notifications) {
        registrations.push(register_notification(runtime, identity, &metrics)?);
    }
    if granted.contains(Capability::ProcessSpawn) {
        registrations.push(register_open(runtime, &metrics)?);
    }
    if granted.contains(Capability::ProcessSpawn) {
        let runner = Arc::new(ProcessRunner::new(root).map_err(|error| {
            NcError::new(ErrorKind::Validation {
                field: "process".to_owned(),
                reason: format!("the application directory cannot be used as a root: {error}"),
            })
            .operation("native::register")
        })?);
        registrations.push(register_process(runtime, &runner, &metrics)?);
    }

    if granted.contains(Capability::FilesystemRead) || granted.contains(Capability::FilesystemWrite)
    {
        let filesystem = Arc::new(FileSystem::new(root).map_err(|error| {
            NcError::new(ErrorKind::Validation {
                field: "filesystem".to_owned(),
                reason: format!("the application directory cannot be used as a root: {error}"),
            })
            .operation("native::register")
        })?);
        register_filesystem(runtime, &filesystem, &metrics, granted, &mut registrations)?;
    }

    Ok(registrations)
}

/// Registers the commands that read inside the application.
fn register_filesystem(
    runtime: &Runtime,
    filesystem: &Arc<FileSystem>,
    metrics: &Arc<ncui_observability::metrics::MetricsRegistry>,
    granted: CapabilitySet,
    registrations: &mut Vec<CommandRegistration>,
) -> Result<()> {
    if granted.contains(Capability::FilesystemRead) {
        let read_root = Arc::clone(filesystem);
        let read_metrics = Arc::clone(metrics);
        registrations.push(register_command(
            runtime,
            "fs.read_text",
            CapabilitySet::empty().with(Capability::FilesystemRead),
            "Read a text file inside the application directory.",
            move |payload| {
                record_call(&read_metrics, "read_text");
                let path = path_argument(&payload)?;
                read_root
                    .read_text(&path)
                    .map_err(|error| fs_error(&error))
                    .and_then(|content| to_value(&content))
            },
        )?);

        let list_root = Arc::clone(filesystem);
        let list_metrics = Arc::clone(metrics);
        registrations.push(register_command(
            runtime,
            "fs.list",
            CapabilitySet::empty().with(Capability::FilesystemRead),
            "List a directory inside the application directory.",
            move |payload| {
                record_call(&list_metrics, "list");
                let path = path_argument(&payload)?;
                list_root
                    .list(&path)
                    .map_err(|error| fs_error(&error))
                    .and_then(|listing| to_value(&listing))
            },
        )?);

        let stat_root = Arc::clone(filesystem);
        let stat_metrics = Arc::clone(metrics);
        registrations.push(register_command(
            runtime,
            "fs.stat",
            CapabilitySet::empty().with(Capability::FilesystemRead),
            "Describe a file or directory inside the application directory.",
            move |payload| {
                record_call(&stat_metrics, "stat");
                let path = path_argument(&payload)?;
                stat_root
                    .stat(&path)
                    .map_err(|error| fs_error(&error))
                    .and_then(|stat| to_value(&stat))
            },
        )?);
    }

    if granted.contains(Capability::FilesystemWrite) {
        let write_root = Arc::clone(filesystem);
        let write_metrics = Arc::clone(metrics);
        registrations.push(register_command(
            runtime,
            "fs.write_text",
            CapabilitySet::empty().with(Capability::FilesystemWrite),
            "Write a text file inside the application directory.",
            move |payload| {
                record_call(&write_metrics, "write_text");
                let path = path_argument(&payload)?;
                let text = payload
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| SeamError::new("invalid_params", "`text` must be a string"))?;
                write_root
                    .write_text(&path, text)
                    .map_err(|error| fs_error(&error))
                    .and_then(|written| to_value(&written))
            },
        )?);
    }

    Ok(())
}

/// Registers the command that runs a program.
fn register_process(
    runtime: &Runtime,
    runner: &Arc<ProcessRunner>,
    metrics: &Arc<ncui_observability::metrics::MetricsRegistry>,
) -> Result<CommandRegistration> {
    let runner = Arc::clone(runner);
    let metrics = Arc::clone(metrics);
    register_command(
        runtime,
        "process.run",
        CapabilitySet::empty().with(Capability::ProcessSpawn),
        "Run a program the system resolves, with a scrubbed environment and a time limit.",
        move |payload| {
            record_process_call(&metrics);
            let request = run_request(&payload)?;
            runner
                .run(&request)
                .map_err(|error| process_error(&error))
                .and_then(|output| to_value(&output))
        },
    )
}

/// Registers one clipboard command.
fn register_clipboard(
    runtime: &Runtime,
    clipboard: &Arc<Clipboard>,
    direction: &'static str,
    metrics: &Arc<ncui_observability::metrics::MetricsRegistry>,
) -> Result<CommandRegistration> {
    let clipboard = Arc::clone(clipboard);
    let metrics = Arc::clone(metrics);
    match direction {
        "read" => register_command(
            runtime,
            "clipboard.read_text",
            CapabilitySet::empty().with(Capability::Clipboard),
            "Read the clipboard's text.",
            move |_payload| {
                record_clipboard_call(&metrics, "read_text");
                clipboard
                    .read_text()
                    .map_err(|error| clipboard_error(&error))
                    .and_then(|text| to_value(&text))
            },
        ),
        _ => register_command(
            runtime,
            "clipboard.write_text",
            CapabilitySet::empty().with(Capability::Clipboard),
            "Write text to the clipboard.",
            move |payload| {
                record_clipboard_call(&metrics, "write_text");
                let text = payload
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| SeamError::new("invalid_params", "`text` must be a string"))?;
                clipboard
                    .write_text(text)
                    .map_err(|error| clipboard_error(&error))
                    .and_then(|written| to_value(&written))
            },
        ),
    }
}

/// Registers the notification command.
///
/// The title and the body are the whole payload: a notification is a message to the user, so
/// there is nothing else for a page to say and nothing else for the host to interpret.
fn register_notification(
    runtime: &Runtime,
    identity: Option<&str>,
    metrics: &Arc<ncui_observability::metrics::MetricsRegistry>,
) -> Result<CommandRegistration> {
    let metrics = Arc::clone(metrics);
    // The application's own identity, when it names one: a notification delivered under the
    // application's name is one the user can act on, and the identity is the same value a bundle
    // registers with the platform, so the two cannot drift.
    let notifier = identity.map_or_else(Notifier::new, Notifier::with_app_id);
    register_command(
        runtime,
        "notifications.show",
        CapabilitySet::empty().with(Capability::Notifications),
        "Raise a notification with a title and a body.",
        move |payload| {
            record_notification_call(&metrics);
            let title = payload
                .get("title")
                .and_then(Value::as_str)
                .ok_or_else(|| SeamError::new("invalid_params", "`title` must be a string"))?;
            let body = payload
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or_default();
            notifier
                .show(title, body)
                .map_err(|error| notification_error(&error))
                .and_then(|()| {
                    to_value(&serde_json::json!({
                        "shown": true,
                        "title": title,
                        "bytes": title.len() + body.len(),
                    }))
                })
        },
    )
}

/// Registers the command that hands a target to whatever the platform uses for it.
///
/// It asks for the same capability as spawning a program, and for the same reason: opening a
/// target starts whatever program the platform associates with it, which is a program the page
/// did not name. `process.spawn` is destructive, so the consent gate applies to every call —
/// which is right, because a page that can open a target can start a program the user did not
/// choose.
fn register_open(
    runtime: &Runtime,
    metrics: &Arc<ncui_observability::metrics::MetricsRegistry>,
) -> Result<CommandRegistration> {
    let metrics = Arc::clone(metrics);
    register_command(
        runtime,
        "shell.open",
        CapabilitySet::empty().with(Capability::ProcessSpawn),
        "Open a file or a link with whatever the system uses for it.",
        move |payload| {
            record_open_call(&metrics);
            let target = payload
                .get("target")
                .and_then(Value::as_str)
                .ok_or_else(|| SeamError::new("invalid_params", "`target` must be a string"))?;
            open_with_default(target)
                .map_err(|error| open_error(&error))
                .and_then(|()| {
                    to_value(&serde_json::json!({
                        "opened": true,
                        "target": target,
                    }))
                })
        },
    )
}

/// Counts one open, so a page that opens a stream of targets is visible.
fn record_open_call(metrics: &ncui_observability::metrics::MetricsRegistry) {
    let _ = metrics.counter_inc(
        "native.opens",
        &ncui_observability::metrics::Labels::new(),
        1,
    );
}

/// Maps an open failure onto the bridge's error shape.
fn open_error(error: &OpenError) -> SeamError {
    SeamError {
        code: error.code().to_owned(),
        message: error.to_string(),
        retryable: false,
    }
}

/// Counts one notification, so a page that raises a stream of them is visible.
fn record_notification_call(metrics: &ncui_observability::metrics::MetricsRegistry) {
    let _ = metrics.counter_inc(
        "native.notifications",
        &ncui_observability::metrics::Labels::new(),
        1,
    );
}

/// Maps a notification failure onto the bridge's error shape.
fn notification_error(error: &NotificationError) -> SeamError {
    SeamError {
        code: error.code().to_owned(),
        message: error.to_string(),
        retryable: false,
    }
}

/// Counts one clipboard call, labelled by the operation.
fn record_clipboard_call(metrics: &ncui_observability::metrics::MetricsRegistry, operation: &str) {
    let Ok(labels) = ncui_observability::metrics::Labels::from_pairs([("op", operation)]) else {
        return;
    };
    let _ = metrics.counter_inc("native.clipboard_calls", &labels, 1);
}

/// Maps a clipboard failure onto the bridge's error shape.
fn clipboard_error(error: &ClipboardError) -> SeamError {
    SeamError {
        code: error.code().to_owned(),
        message: error.to_string(),
        retryable: error.retryable(),
    }
}

/// Registers one command, mapping a refusal to a validation error.
fn register_command<F>(
    runtime: &Runtime,
    name: &str,
    requires: CapabilitySet,
    summary: &str,
    handler: F,
) -> Result<CommandRegistration>
where
    F: Fn(Value) -> std::result::Result<Value, SeamError> + Send + Sync + 'static,
{
    runtime
        .register_page_command(name, requires, summary, handler)
        .map_err(|error| {
            NcError::new(ErrorKind::Validation {
                field: "native_command".to_owned(),
                reason: format!("`{name}` could not be registered: {error}"),
            })
            .operation("native::register")
        })
}

/// Counts one filesystem call, labelled by the operation.
///
/// A page's filesystem use is the part of an application an operator most needs to see:
/// a call rate that climbs is either a feature or a page that is doing something it should
/// not. The label is the operation's own name, from this module, never the page's text.
fn record_call(metrics: &ncui_observability::metrics::MetricsRegistry, operation: &str) {
    let Ok(labels) = ncui_observability::metrics::Labels::from_pairs([("op", operation)]) else {
        return;
    };
    let _ = metrics.counter_inc("native.fs_calls", &labels, 1);
}

/// Reads the arguments of a `process.run` command.
fn run_request(payload: &Value) -> std::result::Result<RunRequest, SeamError> {
    let program = payload
        .get("program")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| SeamError::new("invalid_params", "`program` must be a string"))?;
    let args = match payload.get("args") {
        None => Vec::new(),
        Some(value) => value
            .as_array()
            .ok_or_else(|| SeamError::new("invalid_params", "`args` must be an array of strings"))?
            .iter()
            .map(|argument| {
                argument.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                    SeamError::new("invalid_params", "each element of `args` must be a string")
                })
            })
            .collect::<std::result::Result<Vec<_>, _>>()?,
    };
    let stdin = match payload.get("stdin") {
        None => None,
        Some(value) => Some(
            value
                .as_str()
                .ok_or_else(|| SeamError::new("invalid_params", "`stdin` must be a string"))?
                .to_owned(),
        ),
    };
    let timeout_ms =
        match payload.get("timeout_ms") {
            None => None,
            Some(value) => Some(value.as_u64().ok_or_else(|| {
                SeamError::new("invalid_params", "`timeout_ms` must be a number")
            })?),
        };
    Ok(RunRequest {
        program,
        args,
        stdin,
        timeout_ms,
    })
}

/// Counts one program run.
fn record_process_call(metrics: &ncui_observability::metrics::MetricsRegistry) {
    let _ = metrics.counter_inc(
        "native.process_runs",
        &ncui_observability::metrics::Labels::new(),
        1,
    );
}

/// Maps a process failure onto the bridge's error shape.
fn process_error(error: &ProcessError) -> SeamError {
    SeamError {
        code: error.code().to_owned(),
        message: error.to_string(),
        retryable: error.retryable(),
    }
}

/// Reads the `path` argument of a filesystem command.
fn path_argument(payload: &Value) -> std::result::Result<String, SeamError> {
    payload
        .get("path")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| SeamError::new("invalid_params", "`path` must be a string"))
}

/// Serialises a result for the page.
fn to_value<T: serde::Serialize>(value: &T) -> std::result::Result<Value, SeamError> {
    serde_json::to_value(value).map_err(|error| {
        SeamError::new(
            "internal",
            format!("the result is not serialisable: {error}"),
        )
    })
}

/// Maps a filesystem failure onto the bridge's error shape.
fn fs_error(error: &FsError) -> SeamError {
    SeamError {
        code: error.code().to_owned(),
        message: error.to_string(),
        retryable: error.retryable(),
    }
}

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]
#[cfg(test)]
mod tests {
    use super::*;
    use ncui_core::Limits;
    use ncui_webview::{ActionTable, SeamMessage};

    fn runtime_with(capabilities: &[Capability]) -> Runtime {
        let mut runtime = Runtime::start(Limits::default()).expect("the runtime starts");
        runtime.grant_page_capabilities(capabilities);
        runtime
    }

    fn scratch() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "ncui-native-register-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("the scratch directory is created");
        path
    }

    #[test]
    fn a_page_without_the_capability_gets_no_filesystem_commands() {
        let root = scratch();
        let runtime = runtime_with(&[]);
        let registrations = register(&runtime, &root, None).expect("registration succeeds");
        assert!(registrations.is_empty());
        assert!(
            runtime.page_commands.get("fs.read_text").is_none(),
            "a page that was not granted the capability must not be offered the command"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_read_capability_registers_the_read_commands_only() {
        let root = scratch();
        let runtime = runtime_with(&[Capability::FilesystemRead]);
        let _registrations = register(&runtime, &root, None).expect("registration succeeds");
        assert!(runtime.page_commands.get("fs.read_text").is_some());
        assert!(runtime.page_commands.get("fs.list").is_some());
        assert!(runtime.page_commands.get("fs.stat").is_some());
        assert!(
            runtime.page_commands.get("fs.write_text").is_none(),
            "read must not bring write with it"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_spawn_capability_registers_the_run_command() {
        let root = scratch();
        let runtime = runtime_with(&[Capability::ProcessSpawn]);
        let _registrations = register(&runtime, &root, None).expect("registration succeeds");
        assert!(runtime.page_commands.get("process.run").is_some());
        assert!(
            runtime.page_commands.get("fs.read_text").is_none(),
            "spawn must not bring the filesystem with it"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_run_command_executes_a_real_program() {
        let root = scratch();
        let runtime = runtime_with(&[Capability::ProcessSpawn]);
        let _registrations = register(&runtime, &root, None).expect("registration succeeds");
        let command = runtime
            .page_commands
            .get("process.run")
            .expect("registered");
        let handler = match command.target {
            crate::commands::CommandTarget::Native(handler) => handler,
            crate::commands::CommandTarget::RuntimeMethod(_) => panic!("expected a native command"),
        };
        let output = if cfg!(windows) {
            handler(serde_json::json!({
                "program": "cmd",
                "args": ["/C", "echo from the page"]
            }))
        } else {
            handler(serde_json::json!({
                "program": "sh",
                "args": ["-c", "echo from the page"]
            }))
        }
        .expect("the program runs");
        assert_eq!(output["status"], 0);
        assert_eq!(
            output["stdout"].as_str().map(str::trim),
            Some("from the page")
        );
        assert_eq!(output["timed_out"], false);

        let labels = ncui_observability::metrics::Labels::new();
        assert_eq!(
            runtime
                .metrics
                .counter_value("native.process_runs", &labels),
            Some(1),
            "the run must be counted"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_program_name_with_a_path_outside_the_application_is_refused_at_the_command() {
        let root = scratch();
        let runtime = runtime_with(&[Capability::ProcessSpawn]);
        let _registrations = register(&runtime, &root, None).expect("registration succeeds");
        let command = runtime
            .page_commands
            .get("process.run")
            .expect("registered");
        let handler = match command.target {
            crate::commands::CommandTarget::Native(handler) => handler,
            crate::commands::CommandTarget::RuntimeMethod(_) => panic!("expected a native command"),
        };
        let program = if cfg!(windows) {
            "C:\\Windows\\System32\\cmd.exe"
        } else {
            "/bin/sh"
        };
        let error = handler(serde_json::json!({ "program": program }))
            .expect_err("an absolute program path is refused");
        assert_eq!(error.code, "capability_denied");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_run_request_with_a_bad_argument_is_refused_before_anything_spawns() {
        let root = scratch();
        let runtime = runtime_with(&[Capability::ProcessSpawn]);
        let _registrations = register(&runtime, &root, None).expect("registration succeeds");
        let command = runtime
            .page_commands
            .get("process.run")
            .expect("registered");
        let handler = match command.target {
            crate::commands::CommandTarget::Native(handler) => handler,
            crate::commands::CommandTarget::RuntimeMethod(_) => panic!("expected a native command"),
        };
        for payload in [
            serde_json::json!({}),
            serde_json::json!({ "program": "x", "args": "not an array" }),
            serde_json::json!({ "program": "x", "args": [1] }),
            serde_json::json!({ "program": "x", "stdin": 7 }),
            serde_json::json!({ "program": "x", "timeout_ms": "soon" }),
        ] {
            let error = handler(payload.clone()).expect_err("the request is refused");
            assert_eq!(error.code, "invalid_params", "for {payload}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_clipboard_capability_registers_the_clipboard_commands() {
        let root = scratch();
        let runtime = runtime_with(&[Capability::Clipboard]);
        let _registrations = register(&runtime, &root, None).expect("registration succeeds");
        assert!(runtime.page_commands.get("clipboard.read_text").is_some());
        assert!(runtime.page_commands.get("clipboard.write_text").is_some());
        assert!(
            runtime.page_commands.get("fs.read_text").is_none(),
            "the clipboard must not bring the filesystem with it"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_notification_command_raises_a_real_notification() {
        // The end of the chain the host can verify: the platform's own notification API accepted
        // the message. Whether the user then sees it depends on do-not-disturb and on the
        // identity the toast is delivered under, which is why the module documents both.
        let root = scratch();
        let runtime = runtime_with(&[Capability::Notifications]);
        let _registrations = register(&runtime, &root, None).expect("registration succeeds");
        let show = runtime
            .page_commands
            .get("notifications.show")
            .expect("registered");
        let crate::commands::CommandTarget::Native(show) = show.target.clone() else {
            panic!("notifications.show is a native command");
        };
        let answer = show(serde_json::json!({
            "title": "NC-UI",
            "body": "a page asked for this notification"
        }));
        match answer {
            Ok(answer) => {
                assert_eq!(answer["shown"], true);
                assert_eq!(answer["title"], "NC-UI");
            }
            // A machine with no notifier is a fact about the machine — a container, a bare server, a
            // desktop without `notify-send`. What is skipped is the part only a real platform can
            // answer; the refusals below are checked either way.
            Err(error) if error.code == "notification_unavailable" => {}
            Err(error) => panic!("the platform accepts the notification: {error:?}"),
        }

        // A title over the bound is refused before anything is started.
        let refused = show(serde_json::json!({
            "title": "t".repeat(ncui_native::MAX_TITLE_BYTES + 1)
        }))
        .expect_err("an over-long title is refused");
        assert_eq!(refused.code, "notification_too_long");
        // A payload that is not a string is a caller's mistake, reported as one.
        let refused = show(serde_json::json!({ "title": 7 })).expect_err("a number is not a title");
        assert_eq!(refused.code, "invalid_params");
    }

    #[test]
    fn the_open_command_asks_the_platform_and_reports_its_refusal() {
        // The command really runs: a target no handler is registered for is refused by the
        // platform, and the refusal reaches the page with the code that says which rule produced
        // it. What it does not assert is that a handler runs, for the reason the module gives:
        // Windows resolves a scheme through a cache the shell refreshes on its own schedule.
        let root = scratch();
        let runtime = runtime_with(&[Capability::ProcessSpawn]);
        let _registrations = register(&runtime, &root, None).expect("registration succeeds");
        let open = runtime.page_commands.get("shell.open").expect("registered");
        let crate::commands::CommandTarget::Native(open) = open.target.clone() else {
            panic!("shell.open is a native command");
        };

        let refused = open(serde_json::json!({ "target": "javascript:alert(1)" }))
            .expect_err("a browser's scheme is refused");
        assert_eq!(refused.code, "open_scheme_refused");

        let refused = open(serde_json::json!({ "target": "https://example.test/\" & calc" }))
            .expect_err("a target that cannot survive the tool is refused");
        assert_eq!(refused.code, "open_target_refused");

        let refused =
            open(serde_json::json!({ "target": 7 })).expect_err("a number is not a target");
        assert_eq!(refused.code, "invalid_params");
    }

    #[test]
    fn a_page_without_the_capability_gets_no_open_command() {
        let root = scratch();
        let runtime = runtime_with(&[Capability::FilesystemRead]);
        let _registrations = register(&runtime, &root, None).expect("registration succeeds");
        assert!(
            runtime.page_commands.get("shell.open").is_none(),
            "reading a file is not permission to start whatever opens a target"
        );
    }

    #[test]
    fn a_page_without_the_notification_capability_gets_no_notification_command() {
        let root = scratch();
        let runtime = runtime_with(&[Capability::FilesystemRead]);
        let _registrations = register(&runtime, &root, None).expect("registration succeeds");
        assert!(
            runtime.page_commands.get("notifications.show").is_none(),
            "read.logs is not permission to interrupt the user"
        );
    }

    #[test]
    fn the_clipboard_commands_round_trip_real_text() {
        let root = scratch();
        let runtime = runtime_with(&[Capability::Clipboard]);
        let _registrations = register(&runtime, &root, None).expect("registration succeeds");
        let write = runtime
            .page_commands
            .get("clipboard.write_text")
            .expect("registered");
        let read = runtime
            .page_commands
            .get("clipboard.read_text")
            .expect("registered");
        let (
            crate::commands::CommandTarget::Native(write),
            crate::commands::CommandTarget::Native(read),
        ) = (write.target.clone(), read.target.clone())
        else {
            panic!("the clipboard commands are native");
        };

        let text = format!("ncui native clipboard {}", std::process::id());
        match write(serde_json::json!({ "text": text.clone() })) {
            Ok(written) => {
                assert_eq!(written["bytes"], text.len());
                let read_back = read(serde_json::json!({})).expect("the read succeeds");
                assert_eq!(read_back["text"], text);
            }
            // A machine with no clipboard tool is a fact about the machine; what is skipped is the
            // round trip only a real platform can answer, and the refusal path is covered by the
            // test below.
            Err(error) if error.code == "unavailable" => {
                let _ = std::fs::remove_dir_all(&root);
                return;
            }
            Err(error) => panic!("the write succeeds: {error:?}"),
        }

        let labels = ncui_observability::metrics::Labels::from_pairs([("op", "read_text")])
            .expect("the label is valid");
        assert_eq!(
            runtime
                .metrics
                .counter_value("native.clipboard_calls", &labels),
            Some(1),
            "the call must be counted"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_clipboard_write_without_text_is_refused_before_the_tool_is_reached() {
        let root = scratch();
        let runtime = runtime_with(&[Capability::Clipboard]);
        let _registrations = register(&runtime, &root, None).expect("registration succeeds");
        let write = runtime
            .page_commands
            .get("clipboard.write_text")
            .expect("registered");
        let crate::commands::CommandTarget::Native(write) = write.target.clone() else {
            panic!("the clipboard command is native");
        };
        let error = write(serde_json::json!({})).expect_err("a missing text is refused");
        assert_eq!(error.code, "invalid_params");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_write_capability_registers_the_write_command() {
        let root = scratch();
        let runtime = runtime_with(&[Capability::FilesystemWrite]);
        let _registrations = register(&runtime, &root, None).expect("registration succeeds");
        assert!(runtime.page_commands.get("fs.write_text").is_some());
        assert!(runtime.page_commands.get("fs.read_text").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_registered_command_is_advertised_to_the_page() {
        let root = scratch();
        let runtime = runtime_with(&[Capability::FilesystemRead]);
        let _registrations = register(&runtime, &root, None).expect("registration succeeds");
        let actions: ActionTable = runtime.page_commands.actions();
        assert!(
            actions.requires("fs.read_text").is_some(),
            "the page must be able to discover the command it may call"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_registered_command_reads_a_real_file() {
        let root = scratch();
        std::fs::write(root.join("note.txt"), "from disk").expect("the fixture is written");
        let runtime = runtime_with(&[Capability::FilesystemRead]);
        let _registrations = register(&runtime, &root, None).expect("registration succeeds");

        let command = runtime
            .page_commands
            .get("fs.read_text")
            .expect("registered");
        let handler = match command.target {
            crate::commands::CommandTarget::Native(handler) => handler,
            crate::commands::CommandTarget::RuntimeMethod(method) => {
                panic!("expected a native command, got `{method}`")
            }
        };
        let value = handler(serde_json::json!({ "path": "note.txt" })).expect("the call succeeds");
        assert_eq!(value["text"], "from disk");
        assert_eq!(value["bytes"], 9);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_filesystem_call_is_counted_in_the_metrics_registry() {
        // The observability claim, tested rather than asserted in prose: a call to a
        // native command is visible to whoever reads the runtime's metrics.
        let root = scratch();
        std::fs::write(root.join("note.txt"), "counted").expect("the fixture is written");
        let runtime = runtime_with(&[Capability::FilesystemRead]);
        let _registrations = register(&runtime, &root, None).expect("registration succeeds");
        let command = runtime
            .page_commands
            .get("fs.read_text")
            .expect("registered");
        let handler = match command.target {
            crate::commands::CommandTarget::Native(handler) => handler,
            crate::commands::CommandTarget::RuntimeMethod(_) => panic!("expected a native command"),
        };
        handler(serde_json::json!({ "path": "note.txt" })).expect("the read succeeds");
        let labels = ncui_observability::metrics::Labels::from_pairs([("op", "read_text")])
            .expect("the label is valid");
        assert_eq!(
            runtime.metrics.counter_value("native.fs_calls", &labels),
            Some(1),
            "the call must be counted under its operation"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_path_argument_is_refused_before_the_filesystem_is_reached() {
        let root = scratch();
        let runtime = runtime_with(&[Capability::FilesystemRead]);
        let _registrations = register(&runtime, &root, None).expect("registration succeeds");
        let command = runtime
            .page_commands
            .get("fs.read_text")
            .expect("registered");
        let handler = match command.target {
            crate::commands::CommandTarget::Native(handler) => handler,
            crate::commands::CommandTarget::RuntimeMethod(_) => panic!("expected a native command"),
        };
        let error = handler(serde_json::json!({})).expect_err("a missing path is refused");
        assert_eq!(error.code, "invalid_params");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_escape_is_refused_at_the_command_boundary() {
        let root = scratch();
        let runtime = runtime_with(&[Capability::FilesystemRead]);
        let _registrations = register(&runtime, &root, None).expect("registration succeeds");
        let command = runtime
            .page_commands
            .get("fs.read_text")
            .expect("registered");
        let handler = match command.target {
            crate::commands::CommandTarget::Native(handler) => handler,
            crate::commands::CommandTarget::RuntimeMethod(_) => panic!("expected a native command"),
        };
        let error = handler(serde_json::json!({ "path": "../../etc/passwd" }))
            .expect_err("an escape is refused");
        assert_eq!(error.code, "capability_denied");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_seam_answers_a_page_with_the_read_result() {
        // The whole path a page's call takes: the seam checks the action against the
        // grant, and the registered handler answers with the file's contents.
        let root = scratch();
        std::fs::write(root.join("note.txt"), "through the seam").expect("the fixture is written");
        let runtime = runtime_with(&[Capability::FilesystemRead]);
        let _registrations = register(&runtime, &root, None).expect("registration succeeds");
        let actions = runtime.page_commands.actions();

        let lifecycle = ncui_webview::WindowLifecycle::new();
        lifecycle
            .transition(ncui_webview::HostState::Created)
            .expect("created");
        lifecycle
            .transition(ncui_webview::HostState::Attaching)
            .expect("attaching");
        lifecycle
            .transition(ncui_webview::HostState::Running)
            .expect("running");
        let seam = ncui_webview::HostSeam::new(
            lifecycle,
            ncui_webview::CapabilityGrant::of(&[Capability::FilesystemRead]),
            ncui_webview::NavigationPolicy::new(&[]),
        )
        .with_action_table(actions);

        let response = seam.handle(
            &SeamMessage::Perform {
                name: "fs.read_text".to_owned(),
                payload: serde_json::json!({ "path": "note.txt" }),
            },
            &mut |request: &ncui_webview::CapabilityRequest, payload| {
                let _ = request;
                let command = runtime
                    .page_commands
                    .get("fs.read_text")
                    .expect("registered");
                match command.target {
                    crate::commands::CommandTarget::Native(handler) => handler(payload),
                    crate::commands::CommandTarget::RuntimeMethod(_) => {
                        panic!("expected a native command")
                    }
                }
            },
        );
        assert!(response.ok, "the seam refused the call: {response:?}");
        assert_eq!(
            response.payload.expect("a payload")["text"],
            "through the seam"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
