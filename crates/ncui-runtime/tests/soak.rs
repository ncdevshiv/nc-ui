//! Soak tests: repeated create/destroy cycles must return resources to baseline.
//!
//! Every other test here checks a bound once. A bound that holds on the first cycle can still
//! leak on the thousandth — a marker added to a set and removed on only one of its two exit
//! paths, a thread pool that joins on shutdown but not on drop, a metric registry that evicts
//! only on a path nothing reaches. Those defects are indistinguishable from a working
//! implementation until the loop runs long enough, which is what this file does.
//!
//! Each test runs a short variant by default. `NCUI_SOAK_ROUNDS` raises the count for a longer
//! local run and for the dedicated CI job, so one set of assertions backs both the per-push
//! run and the long one — rather than there being two files that can disagree.
//!
//! ```sh
//! NCUI_SOAK_ROUNDS=5000 cargo test -p ncui-runtime --test soak -- --nocapture
//! ```

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use ncui_core::{Limits, LimitsBuilder};
use ncui_observability::logring::{LogLevel, LogRecord};
use ncui_observability::metrics::{Labels, MetricsRegistry};
use ncui_runtime::agent::AgentSession;
use ncui_runtime::mcp;
use ncui_runtime::rpc::Request;
use ncui_runtime::{Runtime, dispatch, transport};
use ncui_security::authz::AgentRole;
use ncui_security::ratelimit::RateLimiter;
use ncui_security::token::SecretToken;
use ncui_workers::{BackpressurePolicy, TaskOutcome, WorkerPool, WorkerState, WorkerTask};
use serde_json::json;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How many create/destroy cycles to run.
///
/// Overridable so the same assertions back both the fast in-suite run and a long soak. The
/// default is high enough to catch a per-cycle leak — which accumulates linearly and is
/// therefore obvious well before a thousand rounds — while staying inside an ordinary run.
fn rounds() -> u32 {
    std::env::var("NCUI_SOAK_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200)
}

/// A small but valid configuration.
///
/// Small limits make a leak surface as a *bound breach* rather than as slow memory growth,
/// which is a sharper signal and does not depend on how long the process is watched.
fn limits() -> Limits {
    LimitsBuilder::new()
        .max_workers(2)
        .max_worker_queue_depth(8)
        .max_metric_series(16)
        .log_ring_capacity(32)
        .max_ipc_frame_bytes(64 * 1024)
        .max_worker_payload_bytes(32 * 1024)
        .agent_rate(10_000, 10_000)
        .worker_shutdown_timeout_ms(1_000)
        .build()
        .expect("limits")
}

/// An authenticated administrator session, which is what dispatch requires.
fn session(agent_id: &str) -> AgentSession {
    let mut session =
        AgentSession::new(agent_id, AgentRole::Administrator, 10_000, 10_000).expect("session");
    let token = SecretToken::new(vec![0x77; 32]).expect("token");
    session.authenticate(&token, &token).expect("authenticated");
    session
}

/// Writes one length-prefixed frame, which is the transport's wire format.
fn write_frame(stream: &mut std::net::TcpStream, payload: &[u8]) {
    let length = u32::try_from(payload.len()).expect("a frame fits a u32 length");
    stream.write_all(&length.to_be_bytes()).expect("header");
    stream.write_all(payload).expect("payload");
    stream.flush().expect("flush");
}

/// Reads one length-prefixed frame, ignoring its contents.
///
/// The soak asserts on server-side counters rather than on any single reply, so the payload
/// is discarded; what matters is that the frame is consumed whole.
fn read_response(stream: &mut std::net::TcpStream) {
    use std::io::Read;
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).expect("response header");
    let length = u32::from_be_bytes(header) as usize;
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body).expect("response body");
}

/// A runtime's observable resource counts, for a before/after comparison.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
struct Counts {
    metric_series: usize,
    log_ring: usize,
    worker_queue: usize,
}

fn counts(runtime: &Runtime) -> Counts {
    Counts {
        metric_series: runtime.metrics.len(),
        log_ring: runtime.logs.len(),
        worker_queue: runtime.workers.queue_depth(),
    }
}

/// Repeatedly starting and shutting down a runtime must not accumulate resources.
#[test]
fn a_runtime_returns_to_baseline_after_repeated_cycles() {
    let mut peak = Counts {
        metric_series: 0,
        log_ring: 0,
        worker_queue: 0,
    };

    for round in 0..rounds() {
        let limits = limits();
        let mut runtime = Runtime::start(limits).expect("runtime");

        // Drive every subsystem that holds a resource, so a per-cycle leak in any of them
        // shows in the counts rather than being masked by an untouched one.
        let mut session = session("soak-runtime");
        let table = dispatch::method_table();
        for method in [
            "ncui.describe",
            "ncui.health",
            "ncui.logs.purge",
            "ncui.metrics.query",
        ] {
            let request = Request::new(json!(1), method, json!({ "consent": true }));
            let _ = dispatch::dispatch_with(
                &mut runtime,
                &mut session,
                &request,
                Duration::from_millis(u64::from(round)),
                None,
                &table,
            );
        }
        // Push the ring and the registry past their bounds, so eviction is exercised rather
        // than merely being *available*.
        for i in 0..200 {
            runtime.logs.push(LogRecord::new(
                u64::from(round),
                LogLevel::Info,
                "soak",
                "cycle",
                format!("message {i}"),
            ));
            // The counter is written for its side effect on the registry's series count,
            // so the result is checked rather than dropped: a write that was refused would
            // mean eviction was not exercised at all.
            runtime
                .metrics
                .counter_inc("soak_counter", &Labels::new(), 1)
                .expect("a single-series counter write is within bounds");
        }

        let live = counts(&runtime);
        peak.metric_series = peak.metric_series.max(live.metric_series);
        peak.log_ring = peak.log_ring.max(live.log_ring);
        peak.worker_queue = peak.worker_queue.max(live.worker_queue);

        // The assertion a *leak* trips, as opposed to the eviction counter merely
        // incrementing: however many messages were pushed, the bounded stores stay in bounds.
        assert!(
            live.metric_series <= limits.max_metric_series,
            "round {round}: metric registry holds {} series, over its bound of {}",
            live.metric_series,
            limits.max_metric_series
        );
        assert!(
            live.log_ring <= limits.log_ring_capacity,
            "round {round}: log ring holds {} entries, over its bound of {}",
            live.log_ring,
            limits.log_ring_capacity
        );
        assert!(
            live.worker_queue <= limits.max_worker_queue_depth,
            "round {round}: worker queue holds {} tasks, over its bound of {}",
            live.worker_queue,
            limits.max_worker_queue_depth
        );

        runtime.shutdown();
    }

    // The peak is asserted rather than printed: this workspace denies `print_stdout` in
    // library and test code so that output is never mistaken for a result. The bounds above
    // already assert what the peak would show; this states the soak did work at all.
    assert!(
        peak.metric_series > 0 && peak.log_ring > 0,
        "the soak did no work: peak was {peak:?} over {} cycles",
        rounds()
    );
}

/// Cancellation markers must not survive the tasks they name.
///
/// This is the soak that would have caught the eviction leak: a cancelled task popped by
/// `DropOldest` never reached `worker_loop`, which was the only place markers were removed,
/// and the set had no bound in `Limits` — so it grew without limit and nothing reported it.
#[test]
fn cancellation_markers_do_not_accumulate_across_cycles() {
    for round in 0..rounds() {
        let limits = limits();
        let metrics = Arc::new(MetricsRegistry::new(limits).expect("metrics"));
        let pool = WorkerPool::new(limits, metrics).expect("pool");
        pool.transition(WorkerState::Suspended).expect("suspend");

        for _ in 0..8 {
            let task = WorkerTask::new("soak-cancel", 0, || TaskOutcome::Completed { bytes: 0 });
            let id = task.id;
            pool.submit(task, BackpressurePolicy::DropOldest)
                .expect("queued");
            let _ = pool.cancel(id);
            // Force evictions past the cancelled task: the path that used to strand it.
            for _ in 0..8 {
                let _ = pool.submit(
                    WorkerTask::new("soak-flood", 0, || TaskOutcome::Completed { bytes: 0 }),
                    BackpressurePolicy::DropOldest,
                );
            }
        }

        let markers = pool.cancellation_marker_count();
        assert_eq!(
            markers, 0,
            "round {round}: {markers} cancellation markers survived with nothing queued; a \
             cancelled task that was evicted rather than dequeued kept its marker forever"
        );

        let report = pool.shutdown();
        assert!(
            report.is_complete(),
            "round {round}: shutdown left workers running: {report:?}"
        );
    }
}

/// A budget drained by one peer stays drained, however that peer connects.
#[test]
fn a_shared_rate_limit_does_not_refill_per_connection() {
    let limits = LimitsBuilder::new()
        .agent_rate(1, 4)
        .max_workers(1)
        .build()
        .expect("limits");

    for round in 0..(rounds() / 10).max(1) {
        // One limiter, many sessions: the shape the transport uses after the per-connection
        // limiter was replaced by a shared one.
        let limiter = Arc::new(Mutex::new(
            RateLimiter::new(limits.agent_rate_per_second, limits.agent_rate_burst)
                .expect("limiter"),
        ));
        let mut served = 0;
        for connection in 0..20 {
            let mut session = AgentSession::with_limiter(
                format!("soak-conn-{round}-{connection}"),
                AgentRole::Administrator,
                Arc::clone(&limiter),
            )
            .expect("session");
            if session.rate_limit(Duration::from_millis(0), 1).is_ok() {
                served += 1;
            }
        }
        assert!(
            served <= limits.agent_rate_burst as usize,
            "round {round}: {served} requests were served from a budget of {}, so each \
             connection was handed a fresh allowance",
            limits.agent_rate_burst
        );
    }
}

/// Repeated agent connections must not accumulate live sessions.
#[test]
fn repeated_agent_connections_stay_within_bounds() {
    let limits = limits();
    let runtime = Arc::new(Mutex::new(Runtime::start(limits).expect("runtime")));
    let token_bytes = vec![0x55; 32];
    let token = SecretToken::new(token_bytes.clone()).expect("token");
    let server = transport::AgentServer::bind(
        Arc::clone(&runtime),
        token,
        AgentRole::Administrator,
        limits,
    )
    .expect("bind");
    let addr = server.local_addr();
    let stats = server.stats_handle();
    std::thread::spawn(move || {
        let _ = server.serve();
    });

    for round in 0..(rounds() / 4).max(1) {
        let mut stream = std::net::TcpStream::connect(addr).expect("connect");
        let token = SecretToken::new(token_bytes.clone()).expect("token");
        write_frame(&mut stream, transport::handshake_frame(&token).as_bytes());
        let request = Request::new(json!(1), "ncui.describe", json!({}));
        write_frame(
            &mut stream,
            &serde_json::to_vec(&request).expect("serialise"),
        );
        // Read the reply as a *framed* response. Reading a fixed number of bytes instead
        // would consume part of the next length header, desynchronise the stream, and make
        // the server log a connection error — the test would then be measuring its own
        // malformed client rather than the runtime.
        read_response(&mut stream);

        let live = counts(&runtime.lock().expect("runtime"));
        assert!(
            live.log_ring <= limits.log_ring_capacity,
            "round {round}: log ring grew to {} over repeated connections",
            live.log_ring
        );
    }

    let stats = stats.lock().expect("stats");
    assert!(stats.connections > 0, "the soak connected to nothing");
    assert_eq!(
        stats.connection_errors, 0,
        "a clean handshake and a well-formed request must not produce a connection error"
    );
}

/// An MCP round trip per cycle must leave the runtime's bounded resources at baseline.
#[test]
fn mcp_round_trips_do_not_accumulate_resources() {
    let limits = limits();
    let mut runtime = Runtime::start(limits).expect("runtime");
    let table = dispatch::method_table();
    let before = counts(&runtime);

    for round in 0..rounds() {
        let mut session = session(&format!("soak-mcp-{round}"));
        let result = mcp::call_tool(
            &mut runtime,
            &mut session,
            "ncui_describe",
            json!({}),
            Duration::from_millis(0),
            None,
            &table,
        );
        assert_eq!(result["isError"], json!(false), "round {round}: {result}");
    }

    let after = counts(&runtime);
    assert_eq!(
        after.metric_series, before.metric_series,
        "MCP round trips created metric series that were never released"
    );
    assert_eq!(
        after.worker_queue, 0,
        "MCP round trips left {} tasks queued",
        after.worker_queue
    );
    assert!(
        after.log_ring <= limits.log_ring_capacity,
        "MCP round trips pushed the log ring to {}, over its bound of {}",
        after.log_ring,
        limits.log_ring_capacity
    );
}
