//! The nested-build methods: `ncui.test.run` and `ncui.benchmark.run`.
//!
//! These two methods shell out to cargo, which is why they live apart from the dispatch
//! pipeline they are reached through. The pipeline calls them like any other handler; the
//! guard, the command construction and the timeout live here, so the dispatch module keeps
//! its focus on the request path rather than on process spawning.
//!
//! The guard is a runtime property ([`Runtime::bench_allowed`](crate::Runtime::bench_allowed)),
//! read from `NCUI_RUN_BENCH` at startup. A nested cargo build is expensive and
//! environment-mutating, so an embedder opts in explicitly; a caller without the opt-in gets
//! a refusal rather than a surprise build.

use core::time::Duration;
use std::time::Instant;

use ncui_core::error::validation;
use ncui_core::{ErrorKind, NcError, Result};
use serde_json::{Value, json};

use crate::Runtime;
use crate::dispatch::json_error;

/// Runs the project's test suite.
///
/// Shells out to `cargo test` with an optional package and filter, capturing stdout and
/// stderr. The result is structured so an agent can read pass/fail counts without parsing
/// prose. A timeout bounds the run; exceeding it kills the process and reports a
/// timeout rather than hanging the dispatch pipeline.
pub(crate) fn invoke_test_run(runtime: &Runtime, params: &Value) -> Result<Value> {
    bench_guard(runtime)?;
    let package = params.get("package").and_then(Value::as_str);
    let filter = params.get("filter").and_then(Value::as_str);
    let mut command = std::process::Command::new("cargo");
    command.arg("test");
    if let Some(pkg) = package {
        command.args(["-p", pkg]);
    }
    command.arg("--");
    if let Some(f) = filter {
        command.arg(f);
    }
    command.arg("--nocapture");
    run_cargo(command, "test")
}

/// Runs the project's benchmark suite. See [`invoke_test_run`] for the guard.
pub(crate) fn invoke_benchmark_run(runtime: &Runtime, params: &Value) -> Result<Value> {
    bench_guard(runtime)?;
    let package = params.get("package").and_then(Value::as_str);
    let filter = params.get("filter").and_then(Value::as_str);
    let mut command = std::process::Command::new("cargo");
    command.arg("bench");
    if let Some(pkg) = package {
        command.args(["-p", pkg]);
    }
    command.arg("--");
    if let Some(f) = filter {
        command.arg(f);
    }
    run_cargo(command, "bench")
}

/// Refuses the nested-build methods unless the runtime was started with `NCUI_RUN_BENCH=1`.
///
/// # Errors
///
/// Returns [`ErrorKind::Validation`] when the runtime did not opt in.
fn bench_guard(runtime: &Runtime) -> Result<()> {
    if runtime.bench_allowed {
        return Ok(());
    }
    Err(validation(
        "NCUI_RUN_BENCH",
        "must be set to 1 to run cargo test/bench through dispatch; refusing a nested build",
    )
    .operation("dispatch::bench_guard"))
}

/// Runs a cargo command with a timeout, capturing output.
fn run_cargo(mut command: std::process::Command, kind: &str) -> Result<Value> {
    // The budget covers a *cold release build*, not only the run: `cargo bench` compiles the
    // benchmark binaries first, and the release profile links with thin LTO. A budget sized for
    // the run alone reports a working benchmark suite as a timeout on the first run after any
    // change to the crates it measures — which is exactly when the gate runs it.
    const TIMEOUT_MS: u64 = 900_000;
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    // Run from the workspace root so `-p` resolves. Walk up from the current
    // directory looking for a workspace manifest; fall back to the current dir.
    let mut dir = std::env::current_dir().unwrap_or_default();
    for _ in 0..5 {
        if dir.join("Cargo.toml").exists()
            && std::fs::read_to_string(dir.join("Cargo.toml"))
                .is_ok_and(|s| s.contains("[workspace]"))
        {
            command.current_dir(&dir);
            break;
        }
        if let Some(parent) = dir.parent() {
            dir = parent.to_path_buf();
        } else {
            break;
        }
    }
    let start = Instant::now();
    let mut child = command.spawn().map_err(|e| {
        NcError::new(ErrorKind::Io {
            operation: format!("dispatch::{kind}_run::spawn"),
            message: e.to_string(),
            io_kind: e.kind(),
        })
        .operation("dispatch::run_cargo")
    })?;
    // Wait with timeout.
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok::<std::process::ExitStatus, NcError>(status),
            Ok(None) => {
                if start.elapsed().as_millis() as u64 > TIMEOUT_MS {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(NcError::new(ErrorKind::Timeout {
                        elapsed_ms: TIMEOUT_MS,
                    })
                    .operation(format!("dispatch::{kind}_run")));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                let _ = child.kill();
                return Err(NcError::new(ErrorKind::Io {
                    operation: format!("dispatch::{kind}_run::wait"),
                    message: e.to_string(),
                    io_kind: e.kind(),
                })
                .operation("dispatch::run_cargo"));
            }
        }
    }?;
    let stdout = child
        .stdout
        .take()
        .map(|mut s| {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(&mut s, &mut buf);
            buf
        })
        .unwrap_or_default();
    let stderr = child
        .stderr
        .take()
        .map(|mut s| {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(&mut s, &mut buf);
            buf
        })
        .unwrap_or_default();
    let stdout = String::from_utf8_lossy(&stdout).to_string();
    let stderr = String::from_utf8_lossy(&stderr).to_string();
    let exit_code = status.code().unwrap_or(-1);
    let elapsed_ms = start.elapsed().as_millis() as u64;
    // Truncate output to avoid huge responses.
    let stdout = if stdout.len() > 64 * 1024 {
        format!("{}... (truncated)", &stdout[..64 * 1024])
    } else {
        stdout
    };
    let stderr = if stderr.len() > 64 * 1024 {
        format!("{}... (truncated)", &stderr[..64 * 1024])
    } else {
        stderr
    };
    serde_json::to_value(json!({
        "kind": kind,
        "exit_code": exit_code,
        "success": status.success(),
        "elapsed_ms": elapsed_ms,
        "stdout": stdout,
        "stderr": stderr,
    }))
    .map_err(|e| json_error(&e))
}
