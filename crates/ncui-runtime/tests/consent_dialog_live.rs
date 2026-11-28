//! Live evidence for the F2 question the roadmap names: does the consent prompt a
//! page-initiated `component.open` asks for actually surface?
//!
//! The application wiring the CLI installs is reproduced exactly:
//! `NativeConsentPrompter` as the *default* prompter (`consent_prompter_custom`
//! stays false), so `obtain_human_approval` prefers the asking window's own parented
//! surface — `WryWindowConfirmation`, which posts `HostEvent::Confirm` to the engine
//! thread and parks the pump thread on the reply. The engine shows a real
//! `rfd::MessageDialog` parented to the asking window and answers it.
//!
//! Two verdicts are measured, not assumed:
//!
//! 1. Does a top-level window titled `Confirm destructive operation` appear while the
//!    invoke is pending? That is the prompt surfacing — or not.
//! 2. While the prompt is open, does the runtime lock stay free? The prompt is
//!    asked off the runtime lock — the parked call resumes when the answer
//!    lands — so an unrelated `Mutex::lock` from an uninvolved thread must
//!    clear well inside the prompt's life. (Before two-phase consent the pump
//!    held `&mut Runtime` across the human wait and this probe measured the
//!    hostage it parked: ≥1s while the prompt was held.)
//!
//! If the dialog appears the test answers it through the OS (`WM_COMMAND`/`IDYES`),
//! and the invoke must settle with the component summary — the counter-plugin click
//! completing end to end, which is the arc's acceptance. If it never appears the
//! engine-side stall is proven and the teardown settles the call by dropping the
//! engine beneath the waiter.

#![cfg(all(target_os = "windows", feature = "wry-host"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    clippy::missing_panics_doc,
    reason = "test assertions must abort loudly on failure; evidence is reported through stdout/stderr; the measurement is one linear sequence that splitting would obscure"
)]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use ncui_core::{Capability, Limits, WindowId};
use ncui_runtime::consent::NativeConsentPrompter;
use ncui_runtime::native;
use ncui_runtime::window::hosted_surface;
use ncui_runtime::{Runtime, components};
use ncui_video_surface::evidence::{find_toplevel_window, post_window_command};
use ncui_webview::{Origin, Scheme, WindowRequest};
use serde_json::{Value, json};

static NEXT_COMMAND: AtomicU64 = AtomicU64::new(1);
const PROMPT_TITLE: &str = "Confirm destructive operation";
/// `IDYES`, the consent dialog's approve button, as a `WM_COMMAND` id.
const IDYES: usize = 6;

fn command_id() -> u64 {
    NEXT_COMMAND.fetch_add(1, Ordering::Relaxed)
}

fn serve_page() -> (String, Origin) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the local page server");
    let port = listener.local_addr().expect("server address").port();
    std::thread::spawn(move || {
        for _ in 0..8 {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(stream.try_clone().expect("clone the stream"));
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let body = r#"<!doctype html><title>consent</title><body>consent</body>
<script>
window.__settled = null;
window.__fire = function () {
  // Idempotent: CDP evaluates are at-least-once, and a retried trigger must not
  // open a second component over the first.
  if (window.__fired) { return; }
  window.__fired = true;
  window.ncui.invoke("component.open", { name: "greeter", artifact: "greeter.wat" })
    .then(function (r) {
      window.__stage = "opened:" + r.name;
      return window.ncui.invoke("component.invoke", { name: "greeter", method: "greet" });
    })
    .then(function (r) {
      window.__settled = { ok: true, stage: window.__stage, result: r };
    }, function (e) {
      window.__settled = { ok: false, stage: window.__stage || "open", code: e && e.code, message: e && e.message };
    });
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

/// A directory the test removes when it is done.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let unique = format!(
            "ncui-consent-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("the clock is sane")
                .as_nanos()
        );
        let path = std::env::temp_dir().join(format!("{unique}-{tag}"));
        std::fs::create_dir_all(&path).expect("the scratch directory is writable");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_pages_component_open_asks_consent_and_the_prompt_surfaces() {
    let (url, origin) = serve_page();

    // The artifact is the example's own: the counter plugin is the click the
    // acceptance names, so the artifact it loads is the artifact this test loads.
    let scratch = Scratch::new("app");
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/counter");
    std::fs::copy(
        example.join("plugins/greeter.wat"),
        scratch.path().join("greeter.wat"),
    )
    .expect("the example artifact copies into the application directory");
    // The component reads this file itself, through `fs.read_text` — the service
    // call is the negotiation's proof, so the file it asks for has to exist.
    std::fs::copy(example.join("notes.txt"), scratch.path().join("notes.txt"))
        .expect("the notes file copies into the application directory");

    let mut runtime = Runtime::start(Limits::default()).expect("runtime starts");
    // The manifest's own grant: the counter example declares `run.components`, and
    // without it the command table would not register the surface at all.
    runtime.grant_page_capabilities(&[Capability::RunComponents, Capability::FilesystemRead]);
    // The CLI's exact shape (commands.rs): the native prompt is the *default*
    // surface, so the asking window's parented dialog is what approval asks.
    runtime.set_default_consent_prompter(Arc::new(NativeConsentPrompter::new()));
    let component_commands =
        components::register(&runtime, scratch.path()).expect("the component surface registers");
    assert!(
        !component_commands.is_empty(),
        "the component commands registered — an empty surface means the grant did not cover run.components"
    );
    // `fs.read_text`, the service the artifact's manifest declares. The native
    // surface is rooted at the same directory the components are, matching the
    // application's own registration.
    let _native_commands = native::register(&runtime, scratch.path(), None, &[])
        .expect("the native surface registers");

    let actions = runtime.page_commands.actions();
    let grant = runtime.page_grant();
    runtime.windows = hosted_surface(&[origin], &actions, grant);
    let window = runtime
        .windows
        .open(
            &WindowRequest::new(&url).with_title("ncui-consent-proof"),
            None,
        )
        .expect("the window opens");

    let runtime = Arc::new(Mutex::new(runtime));
    let stop = Arc::new(AtomicBool::new(false));
    let pump = {
        let runtime = Arc::clone(&runtime);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                {
                    let mut runtime = runtime
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let _ = runtime.windows.wait_page_message(Duration::from_millis(5));
                    let now = runtime.elapsed();
                    runtime.pump_pages(now);
                }
                std::thread::yield_now();
            }
        })
    };

    // The window must be evaluable before the trigger means anything — the
    // startup dam (the diagnose in `what to add.md` §7) is retried through.
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
    // The page's own script must have landed: evaluable is transport, `__fire`
    // defined is the page loaded.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match evaluate(&runtime, window, "typeof window.__fire") {
            Ok(value) if value.as_str() == Some("function") => break,
            _ if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(250)),
            other => panic!("the page harness never landed: {other:?}"),
        }
    }

    let baseline = find_toplevel_window(PROMPT_TITLE);
    assert!(
        baseline.is_none(),
        "a consent prompt was already up before the call — the measurement would attribute another call's dialog"
    );

    evaluate(&runtime, window, "(window.__fire(), 'fired')").expect("the invoke trigger evaluates");

    // Verdict one: the prompt. A dialog-class top-level window with the consent
    // title either appears or it does not.
    let prompt_deadline = Instant::now() + Duration::from_secs(12);
    let mut prompt: Option<isize> = None;
    while Instant::now() < prompt_deadline {
        if let Some(hwnd) = find_toplevel_window(PROMPT_TITLE) {
            prompt = Some(hwnd);
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let Some(prompt) = prompt else {
        // The stall half: the invoke is still pending (parked at the consent
        // gate, its answer waiting on a prompt that never surfaced). The probe
        // reports whether the runtime itself is also stuck — a lock that cannot
        // be taken while nothing is being answered is a different, deeper
        // defect than a prompt nobody can see.
        let (probe_tx, probe_rx) = mpsc::channel();
        {
            let runtime = Arc::clone(&runtime);
            std::thread::spawn(move || {
                {
                    let _runtime = runtime
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                let _ = probe_tx.send(());
            });
        }
        let hostage = probe_rx.recv_timeout(Duration::from_secs(3)).is_err();
        eprintln!(
            "F2 REPRODUCED: no consent dialog surfaced in 12s; the invoke is still \
             pending; an unrelated lock acquisition still blocked: {hostage}"
        );
        stop.store(true, Ordering::Release);
        drop(runtime);
        let _ = pump.join();
        panic!("the consent prompt never surfaced — the Confirm event never produced a dialog");
    };

    // The prompt is real. The lock probe now measures the hostage honestly: it
    // starts inside the prompt's life, and a deliberate hold before answering
    // gives it something to accumulate.
    let (probe_tx, probe_rx) = mpsc::channel();
    {
        let runtime = Arc::clone(&runtime);
        std::thread::spawn(move || {
            let waited = Instant::now();
            {
                let _runtime = runtime
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            let _ = probe_tx.send(waited.elapsed());
        });
    }
    eprintln!("the consent prompt surfaced; holding 2s, then answering yes");
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        post_window_command(prompt, IDYES),
        "the answer was queued to the dialog"
    );

    // The lock probe reports how long an unrelated lock acquisition waited:
    // the prompt is asked off the runtime lock, so it must clear well inside
    // the prompt's held life — a wait covering the prompt would be the
    // lock-hostage this test used to measure.
    // The click's chain consents again: `component.invoke` is the same destructive
    // capability, asked per call — so answering once is not enough. A settle-poller
    // watches `__settled` on its own thread while this thread keeps answering
    // whatever prompts appear until the chain ends. The lock probe is collected
    // inside the same loop: the lock is free during the prompts, so its report
    // lands early rather than after the pump releases the runtime.
    let (settle_tx, settle_rx) = mpsc::channel();
    {
        let runtime = Arc::clone(&runtime);
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                match evaluate(&runtime, window, "window.__settled") {
                    Ok(value) if !value.is_null() => {
                        let _ = settle_tx.send(value);
                        return;
                    }
                    _ if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(150));
                    }
                    _ => return,
                }
            }
        });
    }

    let mut answers = 1usize;
    let mut lock_waited = None;
    let deadline = Instant::now() + Duration::from_secs(60);
    let settled = loop {
        if let Some(hwnd) = find_toplevel_window(PROMPT_TITLE) {
            assert!(
                post_window_command(hwnd, IDYES),
                "the answer was queued to the dialog"
            );
            answers += 1;
            // The closed dialog is still enumerable for a moment; a breath
            // between answers keeps one prompt from being counted twice.
            std::thread::sleep(Duration::from_millis(250));
            continue;
        }
        if lock_waited.is_none()
            && let Ok(waited) = probe_rx.try_recv()
        {
            lock_waited = Some(waited);
        }
        match settle_rx.try_recv() {
            Ok(value) => break value,
            Err(mpsc::TryRecvError::Empty) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(150));
            }
            Err(mpsc::TryRecvError::Empty) => {
                panic!("the invoke never settled: still pending at the deadline")
            }
            Err(mpsc::TryRecvError::Disconnected) => panic!("the settle poller died"),
        }
    };
    let lock_waited = match lock_waited.or_else(|| probe_rx.try_recv().ok()) {
        Some(waited) => waited,
        None => probe_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the lock probe reports once the pump releases the runtime"),
    };

    stop.store(true, Ordering::Release);
    let _ = pump.join();

    eprintln!("answered {answers} consent prompt(s); lock probe cleared in {lock_waited:?}");
    assert_eq!(
        settled.get("ok").and_then(Value::as_bool),
        Some(true),
        "the click settled but not as a success: {settled}"
    );
    assert_eq!(
        settled.get("stage").and_then(Value::as_str),
        Some("opened:greeter"),
        "the opened component is the artifact the page named: {settled}"
    );
    // The prompt was held for two seconds; a probe that clears inside one
    // proves the runtime lock was not the person's hostage. The defect this
    // measurement used to prove waited out the whole prompt.
    assert!(
        lock_waited < Duration::from_secs(1),
        "an unrelated lock acquisition parked behind the prompt: {lock_waited:?}"
    );
}
