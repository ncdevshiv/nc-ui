//! The page bridge, driven by a real page in a real window.
//!
//! Every other test of the bridge drives it from the Rust side. This one drives it from
//! the page side: a real `wry` window loads a real document, the document calls the
//! bridge the client package ships, and the host answers. Nothing here is a stand-in for
//! the engine — the page is a page, the IPC channel is the platform's, and the script is
//! the one the host injects.
//!
//! The two calls are answered in the opposite order to the one they were made in, because
//! a bridge that only works when answers arrive in order is not a bridge: it is a queue.

#![cfg(feature = "wry-engine")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use core::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ncui_core::WindowId;
use ncui_webview::{
    CapabilityGrant, HostFactory, HostSeam, NavigationPolicy, PageMessage, SeamRequest,
    SeamResponse, WebViewBackend, WindowLifecycle, WindowRequest,
};
use ncui_webview_wry::WryHostFactory;

/// The page under test.
///
/// It calls the bridge twice, waits for both answers, and reports what it received.
const PAGE_SCRIPT: &str = r"
Promise.all([
  window.ncui.invoke('echo', { n: 1 }),
  window.ncui.invoke('echo', { n: 2 })
]).then(function (values) {
  window.ipc.postMessage('RESULT:' + JSON.stringify(values));
}).catch(function (error) {
  window.ipc.postMessage('ERROR:' + String(error && error.message));
});
";

/// The page, as a data url the engine can load.
fn page_url() -> String {
    let mut encoded = String::with_capacity(PAGE_SCRIPT.len() * 3);
    for byte in PAGE_SCRIPT.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'/'
            | b'('
            | b')'
            | b'{'
            | b'}'
            | b'['
            | b']'
            | b':'
            | b','
            | b'.'
            | b'-'
            | b'_'
            | b'!'
            | b'~'
            | b'*'
            | b'\'' => {
                encoded.push(char::from(byte));
            }
            _ => {
                encoded.push('%');
                let _ = write!(encoded, "{byte:02X}");
            }
        }
    }
    format!("data:text/html,<html><body><script>{encoded}</script></body></html>")
}

/// A running seam that declares one read-only action.
fn running_seam() -> Arc<HostSeam> {
    let lifecycle = WindowLifecycle::new();
    lifecycle
        .transition(ncui_webview::HostState::Created)
        .expect("created");
    lifecycle
        .transition(ncui_webview::HostState::Attaching)
        .expect("attaching");
    lifecycle
        .transition(ncui_webview::HostState::Running)
        .expect("running");
    Arc::new(
        HostSeam::new(
            lifecycle,
            CapabilityGrant::none(),
            NavigationPolicy::new(&[]),
        )
        .with_action("echo", ncui_core::CapabilitySet::empty()),
    )
}

/// Reads the next message the page sent, waiting up to `budget` for one to arrive.
fn next_message(host: &dyn WebViewBackend, budget: Duration) -> Option<PageMessage> {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if let Some(message) = host.poll_page_message() {
            return Some(message);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    None
}

#[test]
fn a_real_page_calls_the_bridge_and_receives_its_own_answers() {
    let seam = running_seam();
    let factory = WryHostFactory;
    let host = factory
        .spawn(WindowId::new(), &WindowRequest::new(page_url()), &seam)
        .expect("a window with a page");
    assert!(
        host.is_alive(),
        "the host must come up: {:?}",
        host.last_error()
    );

    // The page makes two calls. Both must arrive as requests with distinct ids.
    let mut requests = Vec::new();
    while requests.len() < 2 {
        let message = next_message(host.as_ref(), Duration::from_secs(20))
            .expect("the page must post its requests");
        let request = SeamRequest::parse(&message.body).expect("a well-formed bridge request");
        assert!(
            matches!(&request.message, ncui_webview::SeamMessage::Perform { name, .. } if name == "echo"),
            "the page must send the action it was given: {:?}",
            request.message
        );
        requests.push(request);
    }
    assert_ne!(
        requests[0].id, requests[1].id,
        "two calls in flight must carry different ids"
    );

    // Answer them in the opposite order to the one they were made in.
    for request in requests.iter().rev() {
        let payload = match &request.message {
            ncui_webview::SeamMessage::Perform { payload, .. } => payload.clone(),
            other => panic!("unexpected message: {other:?}"),
        };
        let response = SeamResponse::ok("performed", Some(payload)).with_id(&request.id);
        let script = format!(
            "window.ncui && window.ncui.__dispatch({});",
            serde_json::to_string(&response).expect("a serialisable response")
        );
        assert!(
            host.evaluate_script(&script),
            "the host must be able to deliver a response to the page"
        );
    }

    // The page reports both answers, in the order it made the calls.
    let mut result = None;
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && result.is_none() {
        if let Some(message) = host.poll_page_message() {
            let body = message.body;
            if let Some(rest) = body.strip_prefix("RESULT:") {
                result = Some(rest.to_owned());
            } else if let Some(rest) = body.strip_prefix("ERROR:") {
                panic!("the page reported a failure: {rest}");
            }
        } else {
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    let result = result.expect("the page must report the answers it received");
    let values: serde_json::Value = serde_json::from_str(&result).expect("valid JSON");
    assert_eq!(
        values[0]["n"], 1,
        "the first call must receive its own answer"
    );
    assert_eq!(
        values[1]["n"], 2,
        "the second call must receive its own answer"
    );
}
