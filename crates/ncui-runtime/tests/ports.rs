//! The comparison ports, end to end, on a real platform.
//!
//! These tests run the applications under `comparison/ports/` — the NC-UI ports of the cloned
//! Electron and Tauri applications — exactly as a user would: a manifest is read, the assets
//! are served over loopback HTTP, a real window loads the page, and the page calls the runtime
//! through the bridge. The benchmark configuration file is the trigger: the ports measure their
//! own IPC round trips when it is present, which exercises the same page code a user's click
//! does and leaves an observation point the test can assert on.
//!
//! The ports are copied to a scratch directory first, because a run writes its benchmark
//! configuration and result beside the application. Nothing is written into the repository.

#![cfg(feature = "wry-host")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ncui_core::{Capability, CapabilitySet, Limits};
use ncui_runtime::application::Application;
use ncui_runtime::consent::{ConsentPrompter, ConsentRequest};

/// The harness is the host for these runs and explicitly approves only setup and result writes.
/// The measured rounds are `list_windows` calls and never pass through this policy.
#[derive(Debug)]
struct PortApprovalPrompter;

impl ConsentPrompter for PortApprovalPrompter {
    fn confirm(&self, request: &ConsentRequest) -> ncui_core::Result<bool> {
        let params = request.params().as_object();
        let allowed = match request.method() {
            "fs.write_text" => {
                request.capabilities() == CapabilitySet::empty().with(Capability::FilesystemWrite)
                    && params.is_some_and(|params| {
                        params.len() == 2
                            && params.get("path").and_then(serde_json::Value::as_str)
                                == Some("bench-result.json")
                            && params
                                .get("text")
                                .and_then(serde_json::Value::as_str)
                                .is_some()
                    })
            }
            "component.open" => {
                request.capabilities() == CapabilitySet::empty().with(Capability::RunComponents)
                    && params.is_some_and(|params| {
                        params.len() == 2
                            && params.get("name").and_then(serde_json::Value::as_str)
                                == Some("word_signal")
                            && params.get("artifact").and_then(serde_json::Value::as_str)
                                == Some("plugins/word_signal.wat")
                    })
            }
            _ => false,
        };
        Ok(request.subject().starts_with("page:") && allowed)
    }
}

fn launch_port(path: &Path) -> Application {
    Application::launch_with(path, Limits::default(), None, None, |runtime| {
        runtime.set_consent_prompter(Arc::new(PortApprovalPrompter));
        Ok(())
    })
    .expect("the port launches")
}

/// Skips a test when this machine cannot open a real window.
macro_rules! require_real_window {
    () => {
        if std::env::consts::OS == "macos"
            || (std::env::consts::OS != "windows"
                && std::env::var("DISPLAY").is_err()
                && std::env::var("WAYLAND_DISPLAY").is_err())
        {
            eprintln!("skipping: this machine cannot open a real window");
            return;
        }
    };
}

/// The repository root, from this crate's manifest directory.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the crate is two levels below the repository root")
        .to_path_buf()
}

/// A copy of a port directory, removed when the test ends.
struct PortCopy {
    path: PathBuf,
}

impl PortCopy {
    /// Copies `comparison/ports/<name>` into a scratch directory.
    fn new(name: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "ncui-port-{}-{}-{}",
            name,
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        let source = repo_root().join("comparison").join("ports").join(name);
        copy_dir(&source, &path);
        Self { path }
    }

    /// The copied application directory.
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PortCopy {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Recursively copies a directory.
fn copy_dir(source: &Path, target: &Path) {
    std::fs::create_dir_all(target).expect("the scratch directory is created");
    for entry in std::fs::read_dir(source).expect("the source directory is readable") {
        let entry = entry.expect("the directory entry is readable");
        let file_type = entry.file_type().expect("the file type is readable");
        let destination = target.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir(&entry.path(), &destination);
        } else {
            std::fs::copy(entry.path(), &destination).expect("the file is copied");
        }
    }
}

/// Writes the benchmark configuration that makes a port measure its own IPC.
fn write_bench_config(port: &Path, rounds: u32) {
    std::fs::write(
        port.join("bench-config.json"),
        format!(r#"{{"rounds": {rounds}, "output": "bench-result.json"}}"#),
    )
    .expect("the benchmark configuration is written");
}

/// Pumps the application until the benchmark result appears, or the budget runs out.
fn pump_until_result(application: &mut Application, result_path: &Path) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        application.pump();
        if result_path.is_file()
            && let Ok(text) = std::fs::read_to_string(result_path)
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(&text)
        {
            return value;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "the port did not write its benchmark result at {}",
        result_path.display()
    );
}

#[test]
fn the_electron_quick_start_port_runs_its_page_and_measures_ipc() {
    require_real_window!();
    let port = PortCopy::new("electron-quick-start");
    write_bench_config(port.path(), 20);

    let mut application = launch_port(port.path());
    assert!(application.is_running(), "the window is open");
    assert!(
        application.url().starts_with("http://127.0.0.1:"),
        "the page is served from loopback: {}",
        application.url()
    );

    let result = pump_until_result(&mut application, &port.path().join("bench-result.json"));
    assert_eq!(result["framework"], "ncui", "{result}");
    assert_eq!(result["rounds"], 20, "{result}");
    assert_eq!(
        result["completed"], 20,
        "every round trip must complete: {result}"
    );
    assert!(
        result["per_round_ms"].as_f64().expect("a number") > 0.0,
        "the measurement must be a duration: {result}"
    );

    application.shutdown();
}

#[test]
fn the_tauri_word_signal_port_loads_its_component_and_measures_ipc() {
    require_real_window!();
    let port = PortCopy::new("tauri-word-signal");
    write_bench_config(port.path(), 20);

    let mut application = launch_port(port.path());
    assert!(application.is_running(), "the window is open");

    let result = pump_until_result(&mut application, &port.path().join("bench-result.json"));
    assert_eq!(result["framework"], "ncui", "{result}");
    assert_eq!(result["rounds"], 20, "{result}");
    assert_eq!(
        result["completed"], 20,
        "the component must answer every round trip: {result}"
    );

    application.shutdown();
}
