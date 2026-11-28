//! F5: embedded child webviews driven against a real parent window, proven in pixels.
//!
//! An agent-driven session creates a child webview inside a live wry window, moves it,
//! lists it and closes it — and each step is verified against a screen capture of the
//! parent window's client area, because a child webview is a separate platform surface
//! whose presence no amount of API success answers on its own. The captures are kept as
//! artifacts under `target/f5-evidence/` so the claim can be inspected rather than trusted.

#![cfg(all(target_os = "windows", feature = "wry-host"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ncui_core::{Limits, WindowId};
use ncui_runtime::Runtime;
use ncui_runtime::agent::AgentSession;
use ncui_runtime::dispatch::dispatch;
use ncui_runtime::rpc::Request;
use ncui_runtime::window::hosted_surface;
use ncui_security::authz::AgentRole;
use ncui_security::token::SecretToken;
use ncui_webview::{Origin, Scheme, WindowRequest};
use serde_json::{Value, json};

static NEXT_COMMAND: AtomicU64 = AtomicU64::new(1);

const PARENT_BLUE: (i64, i64, i64) = (0, 0, 255);
const CHILD_RED: (i64, i64, i64) = (255, 0, 0);

fn command_id() -> u64 {
    NEXT_COMMAND.fetch_add(1, Ordering::Relaxed)
}

fn page(body_color: &str, title: &str) -> String {
    format!(
        "<!doctype html><title>{title}</title><body style=\"margin:0;background:{body_color};width:100vw;height:100vh\"></body>"
    )
}

/// Serves the solid-color parent and child pages on one loopback server.
fn serve_pages(title: &str) -> (String, String, Origin) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the local page server");
    let port = listener.local_addr().expect("server address").port();
    let parent = page("rgb(0,0,255)", title);
    let child = page("rgb(255,0,0)", "child");
    std::thread::spawn(move || {
        for _ in 0..32 {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("bound the request read");
            let mut reader = BufReader::new(stream.try_clone().expect("clone the request stream"));
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }
            loop {
                let mut line = String::new();
                let read = reader.read_line(&mut line);
                if matches!(read, Ok(0) | Err(_)) || line == "\r\n" {
                    break;
                }
            }
            let body = if request_line.contains("/child") {
                child.as_str()
            } else {
                parent.as_str()
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (
        format!("http://127.0.0.1:{port}/"),
        format!("http://127.0.0.1:{port}/child"),
        Origin::new(Scheme::Http, "127.0.0.1", Some(port)),
    )
}

fn cdp(runtime: &Runtime, window: WindowId, method: &str, params: &Value) -> Result<Value, String> {
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

fn evaluate(runtime: &Runtime, window: WindowId, expression: &str) -> Result<Value, String> {
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

fn wait_until_evaluable(runtime: &Runtime, window: WindowId) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match evaluate(runtime, window, "true") {
            Ok(Value::Bool(true)) => return,
            Err(error) if Instant::now() < deadline => {
                assert!(
                    !error.contains("cdp_target_bind_failed"),
                    "window {window} has no page-target binding: {error}"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
            other => panic!("window {window} never became evaluable: {other:?}"),
        }
    }
}

fn admin_session() -> AgentSession {
    let token = SecretToken::from_os_entropy(32).expect("token");
    let mut session =
        AgentSession::new("f5-agent", AgentRole::Administrator, 100, 100).expect("session");
    session.authenticate(&token, &token).expect("auth");
    session
}

fn agent_call(
    runtime: &mut Runtime,
    session: &mut AgentSession,
    method: &str,
    params: Value,
) -> Value {
    let request = Request::new(json!(command_id()), method, params);
    let response = dispatch(runtime, session, &request, Duration::from_secs(30), None)
        .unwrap_or_else(|| panic!("{method} produced no response"));
    if let Some(error) = response.error {
        panic!("{method} was refused: {error:?}");
    }
    response
        .result
        .unwrap_or_else(|| panic!("{method} produced no result"))
}

/// The workspace root: tests run with the crate directory as their working directory,
/// while the capture helper and the evidence directory live at the workspace's top.
fn workspace_root() -> PathBuf {
    // Not canonicalized: the canonical form carries a `\?\` prefix the .NET imaging
    // APIs inside the capture helper refuse, and the helper is where the path must work.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

struct Capture {
    samples: Vec<(i64, i64, i64, i64, i64)>,
}

impl Capture {
    /// The color of the sample nearest to the requested client-relative physical point.
    fn color_at(&self, x: i64, y: i64) -> Option<(i64, i64, i64)> {
        self.samples
            .iter()
            .find(|(sx, sy, _, _, _)| *sx == x && *sy == y)
            .map(|(_, _, r, g, b)| (*r, *g, *b))
    }
}

fn color_matches(actual: (i64, i64, i64), expected: (i64, i64, i64)) -> bool {
    let tolerance = 40;
    (actual.0 - expected.0).abs() <= tolerance
        && (actual.1 - expected.1).abs() <= tolerance
        && (actual.2 - expected.2).abs() <= tolerance
}

/// Captures the window's client area through the PowerShell helper: the PNG lands at
/// `png_path` as the inspectable artifact and the samples come back for assertions.
fn capture(
    title_fragment: &str,
    png_path: &Path,
    samples: &[(i64, i64)],
) -> Result<Capture, String> {
    let samples_file = png_path.with_extension("samples.json");
    let samples_json = serde_json::to_string(
        &samples
            .iter()
            .map(|(x, y)| json!({ "x": x, "y": y }))
            .collect::<Vec<_>>(),
    )
    .map_err(|error| error.to_string())?;
    std::fs::write(&samples_file, samples_json).map_err(|error| error.to_string())?;
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            &workspace_root()
                .join("tools/capture_window_client.ps1")
                .to_string_lossy(),
            "-WindowTitle",
            title_fragment,
            "-OutPng",
            &png_path.to_string_lossy(),
            "-SamplesFile",
            &samples_file.to_string_lossy(),
        ])
        .output()
        .map_err(|error| format!("the capture helper could not start: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "the capture helper failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let stdout: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("capture output was not JSON: {error}"))?;
    let mut parsed = Vec::new();
    for sample in stdout["samples"].as_array().cloned().unwrap_or_default() {
        parsed.push((
            sample["x"].as_i64().unwrap_or(-1),
            sample["y"].as_i64().unwrap_or(-1),
            sample["r"].as_i64().unwrap_or(-1),
            sample["g"].as_i64().unwrap_or(-1),
            sample["b"].as_i64().unwrap_or(-1),
        ));
    }
    Ok(Capture { samples: parsed })
}

/// Logical client coordinates scaled into the capture's physical pixels.
fn physical(x: i64, y: i64, scale: f64) -> (i64, i64) {
    (
        (x as f64 * scale).round() as i64,
        (y as f64 * scale).round() as i64,
    )
}

/// Polls the window capture until `watch` (a client-relative physical point) shows
/// `expected`, returning the capture that saw it. `extra` points ride along in every
/// capture so the caller can assert on what the rest of the window showed at that instant.
fn poll_until_color(
    title: &str,
    png: &Path,
    watch: (i64, i64),
    expected: (i64, i64, i64),
    extra: &[(i64, i64)],
    timeout: Duration,
    failure: &str,
) -> Capture {
    let deadline = Instant::now() + timeout;
    loop {
        let mut points = vec![watch];
        points.extend_from_slice(extra);
        match capture(title, png, &points) {
            Ok(found) => {
                if found
                    .color_at(watch.0, watch.1)
                    .is_some_and(|color| color_matches(color, expected))
                {
                    return found;
                }
                assert!(Instant::now() < deadline, "{failure}");
            }
            Err(error) if Instant::now() < deadline => {
                let _ = error;
            }
            Err(error) => panic!("the window could not be captured: {error}"),
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn evidence_dir() -> PathBuf {
    let dir = workspace_root().join("target/f5-evidence");
    std::fs::create_dir_all(&dir).expect("the evidence directory can be created");
    dir
}

struct LiveParent {
    runtime: Runtime,
    agent: AgentSession,
    window_key: String,
    title: String,
    run_tag: String,
    scale: f64,
}

impl LiveParent {
    fn open() -> (Self, String) {
        let run_tag = format!("f5-{}", command_id());
        let title = format!("ncui-f5-child-proof-{run_tag}");
        let (parent_url, child_url, origin) = serve_pages(&title);

        let mut runtime = Runtime::start(Limits::default()).expect("runtime starts");
        let actions = runtime.page_commands.actions();
        let grant = runtime.page_grant();
        runtime.windows = hosted_surface(&[origin], &actions, grant);

        let window = runtime
            .windows
            .open(&WindowRequest::new(&parent_url).with_title(&title), None)
            .expect("the parent window opens");
        wait_until_evaluable(&runtime, window);
        let scale = evaluate(&runtime, window, "window.devicePixelRatio")
            .ok()
            .and_then(|value| value.as_f64())
            .unwrap_or(1.0);

        let parent = Self {
            runtime,
            agent: admin_session(),
            window_key: window.to_string(),
            title,
            run_tag,
            scale,
        };
        (parent, child_url)
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        agent_call(&mut self.runtime, &mut self.agent, method, params)
    }

    fn png(&self, phase: &str) -> PathBuf {
        evidence_dir().join(format!("{}-{phase}.png", self.run_tag))
    }
}

#[test]
fn an_agent_driven_child_webview_is_visible_at_its_commanded_rectangle() {
    let (mut parent, child_url) = LiveParent::open();

    // Create: the child is commanded to (40, 40) at 220x150 logical pixels, and the
    // capture must show the child's page inside that rectangle and the parent's outside.
    let (cx, cy, cw, ch) = (40_i64, 40_i64, 220_i64, 150_i64);
    let created = parent.call(
        "ncui.window.child.create",
        json!({
            "window": parent.window_key,
            "url": child_url,
            "x": cx,
            "y": cy,
            "width": cw,
            "height": ch,
        }),
    );
    let child_id = created["child"]
        .as_str()
        .expect("child.create answers the child id")
        .to_owned();

    let inside = physical(cx + cw / 2, cy + ch / 2, parent.scale);
    let outside = physical(cx + cw + 60, cy + ch + 60, parent.scale);
    let first_capture = poll_until_color(
        &parent.title,
        &parent.png("created"),
        inside,
        CHILD_RED,
        &[outside],
        Duration::from_secs(30),
        "the commanded child rectangle never showed the child's page",
    );
    let outside_color = first_capture
        .color_at(outside.0, outside.1)
        .expect("the outside point was sampled");
    assert!(
        color_matches(outside_color, PARENT_BLUE),
        "outside the child rect the parent's page must show, got {outside_color:?}"
    );

    // List: exactly one child, with the parent and rectangle the agent commanded.
    let listed = parent.call("ncui.window.child.list", json!({}));
    let children = listed["children"].as_array().expect("children list");
    assert_eq!(children.len(), 1, "exactly one child webview must be live");
    assert_eq!(children[0]["child"].as_str(), Some(child_id.as_str()));
    assert_eq!(
        children[0]["parent"].as_str(),
        Some(parent.window_key.as_str())
    );
    assert_eq!(children[0]["x"].as_i64(), Some(cx));
    assert_eq!(children[0]["y"].as_i64(), Some(cy));
    assert_eq!(children[0]["width"].as_i64(), Some(cw));
    assert_eq!(children[0]["height"].as_i64(), Some(ch));

    // Bounds: the child moves to (300, 200); the vacated rectangle reverts to the parent.
    let (nx, ny) = (300_i64, 200_i64);
    parent.call(
        "ncui.window.child.bounds",
        json!({
            "window": parent.window_key,
            "child": child_id,
            "x": nx,
            "y": ny,
            "width": cw,
            "height": ch,
        }),
    );
    let moved_inside = physical(nx + cw / 2, ny + ch / 2, parent.scale);
    let moved_capture = poll_until_color(
        &parent.title,
        &parent.png("moved"),
        moved_inside,
        CHILD_RED,
        &[inside],
        Duration::from_secs(20),
        "the child never appeared at its moved rectangle",
    );
    let old_color = moved_capture
        .color_at(inside.0, inside.1)
        .expect("the old point was sampled");
    assert!(
        color_matches(old_color, PARENT_BLUE),
        "the vacated rectangle must revert to the parent's page, got {old_color:?}"
    );

    // Close: the child leaves the list and its pixels revert to the parent's page.
    parent.call(
        "ncui.window.child.close",
        json!({ "window": parent.window_key, "child": child_id }),
    );
    let listed = parent.call("ncui.window.child.list", json!({}));
    assert_eq!(
        listed["children"].as_array().map(Vec::len),
        Some(0),
        "a closed child must not stay listed"
    );
    poll_until_color(
        &parent.title,
        &parent.png("closed"),
        moved_inside,
        PARENT_BLUE,
        &[],
        Duration::from_secs(20),
        "the closed child's pixels never reverted to the parent's page",
    );
}
