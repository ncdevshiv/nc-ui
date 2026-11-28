//! Supervision evidence: real children, real pipes, real exits.
//!
//! The child is the crate's own `test-child` binary — behavior under external
//! control — because supervision evidence with a simulated child proves
//! nothing. Every test carries a deadline: a supervision bug that hangs must
//! fail, never freeze the suite.

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

use ncui_sidecar::{ExitReason, RestartPolicy, SidecarSpec, SidecarStatus, Supervisor};

fn child(program_args: &[&str]) -> SidecarSpec {
    SidecarSpec {
        name: "test-child".to_owned(),
        program: std::path::PathBuf::from(env!("CARGO_BIN_EXE_test-child")),
        args: program_args.iter().map(|arg| (*arg).to_owned()).collect(),
        env: Vec::new(),
        env_clear: false,
        working_dir: None,
        restart: RestartPolicy::Never,
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

#[test]
fn a_spawned_sidecar_roundtrips_bytes_in_both_directions() {
    let supervisor = Supervisor::new();
    let mut handle = supervisor
        .spawn(child(&["echo"]))
        .expect("the child spawns");
    assert!(matches!(handle.status(), SidecarStatus::Running { .. }));

    handle
        .stdin
        .write_all(b"hello sidecar\n")
        .expect("the write lands");
    handle.stdin.flush().expect("the flush lands");
    let mut reader = BufReader::new(&mut handle.stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("the echo returns");
    assert_eq!(line.trim_end(), "hello sidecar");

    assert!(supervisor.terminate(handle.id()));
    wait_for(
        Duration::from_secs(5),
        || matches!(handle.status(), SidecarStatus::Terminated),
        "the terminated status",
    );
}

#[test]
fn a_crash_restarts_with_the_count_and_the_transport_survives() {
    let supervisor = Supervisor::new();
    let mut spec = child(&["echo-then-die", "2"]);
    spec.restart = RestartPolicy::OnFailure {
        max_restarts: 3,
        base_backoff: Duration::from_millis(100),
    };
    let mut handle = supervisor.spawn(spec).expect("the child spawns");
    let first_pid = match handle.status() {
        SidecarStatus::Running { pid, .. } => pid,
        other => panic!("the child must start running, got {other:?}"),
    };

    // Two lines kill the first incarnation — the child exits 7 after two.
    handle.stdin.write_all(b"one\n").expect("first line");
    handle.stdin.write_all(b"two\n").expect("second line");
    handle.stdin.flush().expect("flush");

    wait_for(
        Duration::from_secs(10),
        || handle.restart_count() >= 1 && matches!(handle.status(), SidecarStatus::Running { .. }),
        "the restart",
    );
    let second_pid = match handle.status() {
        SidecarStatus::Running { pid, .. } => pid,
        other => panic!("the restart must be running, got {other:?}"),
    };
    assert_ne!(
        first_pid, second_pid,
        "a restart is a new process, not a zombie"
    );

    // The transport truth: the SAME handle writes and reads the new child.
    handle
        .stdin
        .write_all(b"still here\n")
        .expect("write to the new incarnation");
    handle.stdin.flush().expect("flush");
    let mut reader = BufReader::new(&mut handle.stdout);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut lines = Vec::new();
    while lines.len() < 3 && Instant::now() < deadline {
        let mut line = String::new();
        if reader.read_line(&mut line).expect("the stream reads") > 0 {
            lines.push(line.trim_end().to_owned());
        }
    }
    assert!(
        lines.iter().any(|line| line == "still here"),
        "the new incarnation must echo through the same pipe, got {lines:?}"
    );
    supervisor.terminate(handle.id());
}

#[test]
fn a_clean_exit_stays_down_because_finished_work_is_not_restarted() {
    let supervisor = Supervisor::new();
    let mut spec = child(&["exit", "0"]);
    spec.restart = RestartPolicy::OnFailure {
        max_restarts: 3,
        base_backoff: Duration::from_millis(50),
    };
    let handle = supervisor.spawn(spec).expect("the child spawns");
    wait_for(
        Duration::from_secs(10),
        || {
            matches!(
                handle.status(),
                SidecarStatus::Exited {
                    code: Some(0),
                    reason: ExitReason::CleanExit
                }
            )
        },
        "the clean-exit verdict",
    );
    assert_eq!(
        handle.restart_count(),
        0,
        "finished work is never restarted"
    );
}

#[test]
fn the_never_policy_stays_down_on_any_exit() {
    let supervisor = Supervisor::new();
    let spec = child(&["exit", "3"]);
    let handle = supervisor.spawn(spec).expect("the child spawns");
    wait_for(
        Duration::from_secs(10),
        || {
            matches!(
                handle.status(),
                SidecarStatus::Exited {
                    code: Some(3),
                    reason: ExitReason::PolicyNever
                }
            )
        },
        "the policy-never verdict",
    );
}

#[test]
fn restarts_exhausted_is_a_fault_surfaced_not_a_loop_run() {
    let supervisor = Supervisor::new();
    let mut spec = child(&["exit", "1"]);
    spec.restart = RestartPolicy::OnFailure {
        max_restarts: 1,
        base_backoff: Duration::from_millis(50),
    };
    let handle = supervisor.spawn(spec).expect("the child spawns");
    wait_for(
        Duration::from_secs(15),
        || {
            matches!(
                handle.status(),
                SidecarStatus::Exited {
                    reason: ExitReason::RestartsExhausted,
                    ..
                }
            )
        },
        "the exhausted verdict",
    );
    assert_eq!(
        handle.restart_count(),
        1,
        "exactly one restart was attempted"
    );
}

#[test]
fn terminate_ends_the_stream_with_an_honest_eof() {
    let supervisor = Supervisor::new();
    let mut handle = supervisor
        .spawn(child(&["echo"]))
        .expect("the child spawns");
    handle.stdin.write_all(b"before the end\n").expect("write");
    handle.stdin.flush().expect("flush");
    assert!(supervisor.terminate(handle.id()));
    wait_for(
        Duration::from_secs(5),
        || matches!(handle.status(), SidecarStatus::Terminated),
        "termination",
    );
    // Writes after terminate fail rather than pretending.
    assert!(handle.stdin.write_all(b"too late\n").is_err());
    // The stream ends: EOF — possibly after the already-written echo drains.
    let mut reader = BufReader::new(&mut handle.stdout);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) => panic!("the stream errors rather than ending: {error}"),
        }
        assert!(Instant::now() < deadline, "the stream never ended");
    }
}

#[test]
fn dropping_the_supervisor_takes_the_children_with_it() {
    let supervisor = Supervisor::new();
    let handle = supervisor
        .spawn(child(&["silent"]))
        .expect("the child spawns");
    let pid = match handle.status() {
        SidecarStatus::Running { pid, .. } => pid,
        other => panic!("running expected, got {other:?}"),
    };
    let mut stdout = handle.stdout;
    drop(supervisor);
    // The child is dead because the job says so — EOF is the observable truth
    // from outside the process.
    let mut reader = BufReader::new(&mut stdout);
    let mut line = String::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        assert!(
            Instant::now() < deadline,
            "pid {pid} outlived its supervisor"
        );
    }
}
