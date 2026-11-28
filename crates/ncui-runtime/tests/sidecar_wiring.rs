//! The runtime-integration proof for supervised sidecars: the supervisor the
//! runtime owns spawns, supervises, and terminates real children — and the
//! runtime's own drop is the app-exit path the spec demands.
//!
//! The children are this test binary re-invoked on itself with a child-mode
//! variable set — the self-spawn pattern, because a simulated child proves nothing
//! and a cross-package binary is not guaranteed to exist.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr,
    missing_docs,
    clippy::missing_panics_doc,
    reason = "test assertions must abort loudly on failure"
)]

use std::io::{BufRead, BufReader, Write};
use std::time::{Duration, Instant};

use ncui_core::Limits;
use ncui_runtime::Runtime;
use ncui_sidecar::{ExitReason, RestartPolicy, SidecarSpec, SidecarStatus};

/// Child modes: when the harness re-invokes this binary, the variable picks
/// the behavior the named test runs.
const CHILD_ENV: &str = "NCUI_SIDECAR_CHILD_MODE";

fn in_child_mode() -> Option<String> {
    std::env::var(CHILD_ENV).ok()
}

fn spec(mode: &str, restart: RestartPolicy) -> SidecarSpec {
    SidecarSpec {
        name: format!("station-stunt-{mode}"),
        program: std::env::current_exe().expect("the test binary path"),
        args: vec![
            "sidecar_child_behavior".to_owned(),
            "--exact".to_owned(),
            "--nocapture".to_owned(),
            "--test-threads".to_owned(),
            "1".to_owned(),
        ],
        env: vec![(CHILD_ENV.to_owned(), mode.to_owned())],
        env_clear: false,
        working_dir: None,
        restart,
    }
}

fn wait_for(duration: Duration, mut condition: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {what}");
}

/// The child behavior, in every mode the harness needs. Run as a test in the
/// normal suite it returns immediately — the behavior only exists when the
/// binary was re-invoked as a child.
#[test]
fn sidecar_child_behavior() {
    let Some(mode) = in_child_mode() else {
        return;
    };
    match mode.as_str() {
        "echo" => {
            let stdin = std::io::stdin();
            let stdout = std::io::stdout();
            for line in stdin.lock().lines() {
                let Ok(line) = line else {
                    return;
                };
                let mut out = stdout.lock();
                if out.write_all(line.as_bytes()).is_err() || out.write_all(b"\n").is_err() {
                    return;
                }
                let _ = out.flush();
            }
        }
        "echo-then-die" => {
            let stdin = std::io::stdin();
            let stdout = std::io::stdout();
            let mut count = 0u32;
            for line in stdin.lock().lines() {
                let Ok(line) = line else {
                    return;
                };
                count += 1;
                {
                    let mut out = stdout.lock();
                    if out.write_all(line.as_bytes()).is_err() || out.write_all(b"\n").is_err() {
                        return;
                    }
                    let _ = out.flush();
                }
                if count >= 2 {
                    std::process::exit(7);
                }
            }
        }
        "ndjson-then-die" => {
            // The wire-protocol stunt double: answer each request line with
            // the call's own id, then exit nonzero after the second — a crash
            // a supervisor is expected to repair.
            let stdin = std::io::stdin();
            let stdout = std::io::stdout();
            let mut count = 0u32;
            for line in stdin.lock().lines() {
                let Ok(line) = line else {
                    return;
                };
                let parsed: serde_json::Value = match serde_json::from_str(&line) {
                    Ok(parsed) => parsed,
                    Err(_) => continue,
                };
                let Some(id) = parsed.get("id").and_then(serde_json::Value::as_u64) else {
                    continue;
                };
                let answer = serde_json::json!({"id": id, "result": "pong"});
                {
                    let mut out = stdout.lock();
                    // One atomic write, newline FIRST: the harness's own
                    // progress output writes partial lines, and the wire parses
                    // whole lines as JSON — a leading \n ends whatever noise
                    // line is open so the answer owns its own complete line.
                    let framed = format!("\n{}\n", serde_json::to_string(&answer).expect("json"));
                    if out.write_all(framed.as_bytes()).is_err() {
                        return;
                    }
                    let _ = out.flush();
                }
                count += 1;
                if count >= 2 {
                    std::process::exit(7);
                }
            }
        }
        "exit0" => std::process::exit(0),
        // `silent`: read forever, say nothing — the kill-and-restart child.
        _ => {
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                if line.is_err() {
                    return;
                }
            }
        }
    }
}

#[test]
fn the_runtime_supervises_a_sidecar_through_its_whole_lifecycle() {
    if in_child_mode().is_some() {
        return;
    }
    let runtime = Runtime::start(Limits::default()).expect("the runtime starts");

    // Spawn through the runtime's own supervisor — the shape NC Studio's
    // station process takes: the host owns the byte path outright.
    let mut handle = runtime
        .sidecars
        .spawn(spec(
            "echo-then-die",
            RestartPolicy::OnFailure {
                max_restarts: 3,
                base_backoff: Duration::from_millis(100),
            },
        ))
        .expect("the sidecar spawns");
    assert!(matches!(handle.status(), SidecarStatus::Running { .. }));
    assert!(matches!(
        runtime.sidecars.status(handle.id()),
        Some(SidecarStatus::Running { .. })
    ));

    // The transport roundtrip through the runtime-owned child. Two lines
    // kill the first incarnation — the restart is the runtime's supervision,
    // not anything the test does.
    handle.stdin.write_all(b"station up\n").expect("write");
    handle.stdin.write_all(b"second line\n").expect("write");
    handle.stdin.flush().expect("flush");

    wait_for(
        Duration::from_secs(10),
        || handle.restart_count() >= 1 && matches!(handle.status(), SidecarStatus::Running { .. }),
        "the runtime-supervised restart",
    );
    eprintln!(
        "restart evidence: count {} — the runtime restarted the crash",
        handle.restart_count()
    );

    // The same handle reaches the new incarnation.
    handle.stdin.write_all(b"after restart\n").expect("write");
    handle.stdin.flush().expect("flush");
    let mut reader = BufReader::new(&mut handle.stdout);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut heard = Vec::new();
    // ends_with, not equality: the self-spawned child is a libtest binary, and
    // the harness's own progress output glues onto the echo lines — the echo is
    // always the line's tail because it carries the newline.
    while !heard
        .iter()
        .any(|line: &String| line.ends_with("after restart"))
        && Instant::now() < deadline
    {
        let mut line = String::new();
        if reader.read_line(&mut line).expect("the stream reads") > 0 {
            heard.push(line.trim_end().to_owned());
        }
    }
    assert!(
        heard.iter().any(|line| line.ends_with("after restart")),
        "the restarted child answers through the same pipe: {heard:?}"
    );

    // Terminate through the runtime: the runtime-level inspect agrees.
    assert!(runtime.sidecars.terminate(handle.id()));
    wait_for(
        Duration::from_secs(5),
        || {
            matches!(
                runtime.sidecars.status(handle.id()),
                Some(SidecarStatus::Terminated)
            )
        },
        "the runtime-visible termination",
    );
}

#[test]
fn the_runtime_drop_is_the_app_exit_the_spec_means() {
    if in_child_mode().is_some() {
        return;
    }
    let mut reader;
    {
        let runtime = Runtime::start(Limits::default()).expect("the runtime starts");
        let handle = runtime
            .sidecars
            .spawn(spec("silent", RestartPolicy::Never))
            .expect("the sidecar spawns");
        reader = BufReader::new(handle.stdout);
        // The runtime ends here — the application-exit shape.
    }
    // The child is dead because the job object says so: EOF is the truth
    // observable from outside.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut line = String::new();
    loop {
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        assert!(
            Instant::now() < deadline,
            "the sidecar outlived the runtime — the never-orphan guarantee failed"
        );
    }
    eprintln!("drop evidence: the sidecar died with its runtime");
}

#[test]
fn a_clean_exit_is_reported_through_the_runtime() {
    if in_child_mode().is_some() {
        return;
    }
    let runtime = Runtime::start(Limits::default()).expect("the runtime starts");
    let handle = runtime
        .sidecars
        .spawn(spec(
            "exit0",
            RestartPolicy::OnFailure {
                max_restarts: 2,
                base_backoff: Duration::from_millis(50),
            },
        ))
        .expect("the sidecar spawns");
    wait_for(
        Duration::from_secs(10),
        || {
            matches!(
                runtime.sidecars.status(handle.id()),
                Some(SidecarStatus::Exited {
                    code: Some(0),
                    reason: ExitReason::CleanExit
                })
            )
        },
        "the clean-exit verdict",
    );
}

/// The composition the manifest's `stream` sidecars now take: the supervisor
/// owns the lifecycle, `StreamingSidecar` owns the wire on the stable pipes.
/// The evidence is the recovery — a crashed backend answers the NEXT call
/// through the same wire, because the pipes never noticed the incarnation die.
#[test]
fn a_supervised_sidecar_survives_a_crash_through_the_wire() {
    if in_child_mode().is_some() {
        return;
    }
    let supervisor = ncui_sidecar::Supervisor::new();
    let handle = supervisor
        .spawn(spec(
            "ndjson-then-die",
            RestartPolicy::OnFailure {
                max_restarts: 4,
                base_backoff: Duration::from_millis(50),
            },
        ))
        .expect("the supervised sidecar spawns");
    let sidecar_id = handle.id();
    let (stdin, stdout) = handle.into_transport();
    let wire =
        ncui_native::StreamingSidecar::from_pipes(stdin, stdout, "ndjson-then-die".to_owned())
            .expect("the wire wraps the stable pipes");
    let timeout = Duration::from_secs(10);
    assert_eq!(
        wire.call(&serde_json::json!({"n": 1}), timeout)
            .expect("the first call answers"),
        serde_json::json!("pong")
    );
    // The second call's answer is the crash trigger: the stunt double exits
    // nonzero right after it.
    assert_eq!(
        wire.call(&serde_json::json!({"n": 2}), timeout)
            .expect("the second call answers — the crash comes after it"),
        serde_json::json!("pong")
    );
    wait_for(
        Duration::from_secs(10),
        || {
            matches!(
                supervisor.status(sidecar_id),
                Some(SidecarStatus::Running { .. })
            )
        },
        "the supervisor's restart",
    );
    // The third call lands on the NEW incarnation through the SAME wire — the
    // whole point of composing the layers: the protocol never noticed the
    // process it was talking to die. A call that lands inside the death window
    // is refused honestly; the next attempt answers, so the bounded retry is
    // what a real caller's timeout budget is.
    let deadline = Instant::now() + timeout;
    let mut third = wire.call(&serde_json::json!({"n": 3}), Duration::from_secs(2));
    while third.is_err() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        third = wire.call(&serde_json::json!({"n": 3}), Duration::from_secs(2));
    }
    assert_eq!(
        third.expect("the restarted backend answers through the same wire"),
        serde_json::json!("pong")
    );
    eprintln!("composition evidence: crash, restart, and the third call answered 'pong'");
}
