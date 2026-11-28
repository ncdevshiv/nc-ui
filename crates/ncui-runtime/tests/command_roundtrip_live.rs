//! Live evidence for the `ipc.editor_grade` latency question the bench cannot
//! answer: the full round trip a page command pays across the real webview
//! boundary — `window.ncui.invoke` in page JS, over the wry IPC transport into
//! the host process, through the seam's parse and capability check and the
//! registered handler, and back over evaluated `__dispatch` into the settling
//! promise. The bench measures the native side in-process (p99 3.6 µs); this
//! measures the transport-inclusive whole the register's ≤ 8 ms budget actually
//! governs, on a real window with a real `WebView2` process.
//!
//! The engine-window identity is deterministic here by construction: this
//! binary creates exactly one window — see `filedrop_live.rs` for why the
//! measurement lives in its own binary.

#![cfg(all(target_os = "windows", feature = "wry-host"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::missing_panics_doc,
    clippy::suboptimal_flops,
    clippy::float_cmp,
    reason = "test assertions must abort loudly on failure; evidence is reported through stdout/stderr; measurement math casts deliberately; test helpers take owned values for call-site clarity; measurement functions are linear sequences that splitting would obscure"
)]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ncui_core::{CapabilitySet, Limits, WindowId};
use ncui_runtime::Runtime;
use ncui_runtime::window::hosted_surface;
use ncui_webview::{Origin, Scheme, WindowRequest};
use serde_json::{Value, json};

static NEXT_COMMAND: AtomicU64 = AtomicU64::new(1);

fn command_id() -> u64 {
    NEXT_COMMAND.fetch_add(1, Ordering::Relaxed)
}

fn serve_page() -> (String, Origin) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the local page server");
    let port = listener.local_addr().expect("server address").port();
    std::thread::spawn(move || {
        for _ in 0..16 {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(stream.try_clone().expect("clone the stream"));
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            // The measurement harness lives in the page itself: CDP evaluate is
            // the trigger and the readback, not the transport for a kilobyte of
            // JavaScript — the devtools endpoint in this harness has proven
            // intolerant of large evaluated scripts, and small expressions are
            // the shape the other live evidence uses.
            let body = r#"<!doctype html><title>roundtrip</title><body>roundtrip</body>
<script>
window.__result = null;
window.__measure = function () {
  // Idempotent: the host's trigger is at-least-once — a CDP evaluate can execute
  // and still lose its response to a flaky transport, and a retried trigger must
  // not start a second measurement over the first.
  if (window.__measureStarted) { return; }
  window.__measureStarted = true;
  var times = [];
  var i = 0;
  function step() {
    if (i >= 200) {
      times.sort(function (a, b) { return a - b; });
      function pick(q) { return times[Math.min(times.length - 1, Math.floor(q * times.length))]; }
      window.__result = { count: times.length, p50: pick(0.5), p90: pick(0.9), p99: pick(0.99), max: times[times.length - 1] };
      return;
    }
    var start = performance.now();
    window.__settleLog = window.__settleLog || [];
    window.__settleLog.push(JSON.stringify({ invokeAt: Math.round(start), i: i }));
    window.ncui.invoke("echo", { kind: "playhead_move", position_ms: i * 33 }).then(function (reply) {
      var end = performance.now();
      if (!reply || !reply.echoed) { window.__result = { error: "a reply was not the echo", at: i }; return; }
      times.push(end - start);
      i += 1;
      step();
    }, function (error) {
      window.__result = { error: "invoke rejected: " + (error && error.message), at: i };
    });
  }
  if (!window.ncui) { window.__result = { error: "the bridge is absent" }; return; }
  step();
};
</script>"#;
            let response = format!(
                "HTTP/1.1 200 OK
content-type: text/html
content-length: {}

{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (
        format!("http://127.0.0.1:{port}/"),
        Origin::new(Scheme::Http, "127.0.0.1", Some(port)),
    )
}

fn cdp(
    runtime: &Arc<Mutex<Runtime>>,
    window: WindowId,
    method: &str,
    params: &Value,
) -> Result<Value, String> {
    let runtime = runtime
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    runtime
        .windows
        .cdp_send(
            window,
            &json!({
                "id": command_id(),
                "method": method,
                "params": params.clone(),
            }),
        )
        .map_err(|rejection| format!("window {window} rejected the CDP request: {rejection:?}"))?
        .map_err(|error| error.to_string())
}

fn evaluate(
    runtime: &Arc<Mutex<Runtime>>,
    window: WindowId,
    expression: &str,
) -> Result<Value, String> {
    let answer = cdp(
        runtime,
        window,
        "Runtime.evaluate",
        &json!({ "expression": expression, "returnByValue": true }),
    )?;
    if answer.get("exceptionDetails").is_some() {
        return Err(format!("Runtime.evaluate failed: {answer}"));
    }
    answer
        .pointer("/result/result/value")
        .cloned()
        .ok_or_else(|| format!("Runtime.evaluate returned no value: {answer}"))
}

#[test]
fn a_page_command_round_trip_stays_inside_the_interaction_budget() {
    let (url, origin) = serve_page();
    let mut runtime = Runtime::start(Limits::default()).expect("runtime starts");
    // The scrub-shaped echo: registered before the window opens, in the shared
    // action table the seam reads, so the success path is what is measured —
    // a rejection would measure the refusal path instead.
    let _registration = runtime
        .page_commands
        .register(
            "echo",
            CapabilitySet::empty(),
            "echoes the payload",
            |payload| Ok(json!({ "echoed": payload })),
        )
        .expect("the echo command registers");
    let actions = runtime.page_commands.actions();
    let grant = runtime.page_grant();
    runtime.windows = hosted_surface(&[origin], &actions, grant);
    let window = runtime
        .windows
        .open(
            &WindowRequest::new(&url).with_title("ncui-roundtrip-proof"),
            None,
        )
        .expect("the window opens");

    // The production shape, because nothing less is truthful: the pump is a
    // dedicated thread running continuously from before the page can speak,
    // exactly as transport.rs runs it for a serving runtime. Every interleaved
    // design — pump inside the evaluability wait, pump between polls — was
    // tried here first and each one starved the same way: a blocking CDP call
    // (the devtools endpoint's connects can hang for seconds under load) froze
    // the only thread that could serve the page, the invoke outlived the
    // bridge's own 30 s timeout, and the response settled nothing. The
    // diagnosis cost a day; the architecture is the lesson.
    let runtime = Arc::new(Mutex::new(runtime));
    let pump = Arc::clone(&runtime);
    let pump_thread = std::thread::spawn(move || {
        loop {
            {
                let mut runtime = pump
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                // The blocking wait is the pacing: it wakes the moment the engine
                // delivers, so the cadence the measurement includes is the
                // pipeline's own latency rather than a poll's — the same reason
                // the production bridge documents the wait as the idle path.
                let _ = runtime.windows.wait_page_message(Duration::from_millis(5));
                let now = runtime.elapsed();
                runtime.pump_pages(now);
            }
            std::thread::yield_now();
        }
    });

    // Transient transport failures are retried throughout: the devtools endpoint
    // is one small server inside the browser, a connect to it can hang under
    // load, and none of that says anything about the pipeline. The strictness
    // lives in the measurement assertions, not in any single connect succeeding.
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        match evaluate(&runtime, window, "true") {
            Ok(_) => break,
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(error) => panic!("window never became evaluable: {error}"),
        }
    }

    // The startup dam, measured rather than assumed: at window birth the engine
    // can take tens of seconds before evaluated scripts reach the page (the
    // diagnosis in `futureplan/what to add.md` §7 — events and responses dam
    // behind window creation and flush together). The interaction budget the
    // register sets is about steady-state editing, so the measurement starts
    // when script delivery is provably live — and the dam's length is evidence
    // in its own right, printed for the register rather than hidden.
    let dam_start = Instant::now();
    let dam = loop {
        let posted = Instant::now();
        runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .windows
            .post_to_page(window, "window.__damProbe = (window.__damProbe || 0) + 1;")
            .unwrap_or(false);
        std::thread::sleep(Duration::from_millis(300));
        let landed = evaluate(&runtime, window, "window.__damProbe || 0")
            .ok()
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if landed > 0 {
            break posted - dam_start;
        }
        assert!(
            dam_start.elapsed() < Duration::from_secs(120),
            "script delivery never came live — the dam is the whole story"
        );
        std::thread::sleep(Duration::from_millis(700));
    };
    println!("startup dam before script delivery came live: {dam:.1?}");

    // The measurement starts only now, when the pipeline is live and the pump
    // thread is already serving: an invoke must never outrun the pump, because
    // what is measured is the round trip, not the harness's startup latency.
    // The trigger is idempotent in the page — a CDP evaluate can execute and
    // still lose its response to the flaky transport, so it is at-least-once.
    let trigger_deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match evaluate(&runtime, window, "window.__measure(); true") {
            Ok(_) => break,
            Err(_) if Instant::now() < trigger_deadline => {
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(error) => panic!("the measurement never triggered: {error}"),
        }
    }

    let deadline = Instant::now() + Duration::from_secs(60);
    let measurement: Value = loop {
        // A poll that fails in transport is retried, not asserted — the deadline
        // is what bounds the loop honestly.
        let raw = evaluate(
            &runtime,
            window,
            "window.__result ? JSON.stringify(window.__result) : null",
        );
        if let Ok(raw) = raw
            && let Some(text) = raw.as_str()
        {
            let parsed: Value = serde_json::from_str(text).expect("the measurement is JSON");
            if parsed.get("error").is_some() {
                let log = evaluate(&runtime, window, "JSON.stringify(window.__settleLog || [])")
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_else(|| "unreadable".to_owned());
                panic!("the measurement itself failed in the page: {parsed} settle log: {log}");
            }
            break parsed;
        }
        assert!(
            Instant::now() < deadline,
            "the measurement never completed — the round trips it measures never settled"
        );
        std::thread::sleep(Duration::from_millis(50));
    };

    let p50 = measurement["p50"].as_f64().expect("p50");
    let p90 = measurement["p90"].as_f64().expect("p90");
    let p99 = measurement["p99"].as_f64().expect("p99");
    let max = measurement["max"].as_f64().expect("max");
    let count = measurement["count"].as_u64().unwrap_or(0);
    assert_eq!(count, 200, "every invoke settled: {measurement}");
    // The register's budget: command round-trip p99 ≤ 8 ms on Windows dev
    // hardware, transport included. The numbers are printed rather than only
    // asserted because the evidence is the distribution, not the pass.
    println!(
        "round-trip ms over {count} sequential invokes: p50={p50:.3} p90={p90:.3} p99={p99:.3} max={max:.3}"
    );
    assert!(
        p99 <= 8.0,
        "the interaction budget is 8 ms; measured p99 {p99:.3} ms — the transport-inclusive round trip is over budget"
    );
    drop(pump_thread);
}
