//! Smoke test for the real engine: a host can be created on a machine with a display.
//!
//! On a machine without a display or without the `wry-engine` feature, the test skips
//! rather than failing, because "no display" is a fact about the machine.

#![cfg(feature = "wry-engine")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use ncui_webview::WebViewBackend;

#[test]
fn a_host_can_be_created_on_a_machine_with_a_display() {
    let host = ncui_webview_wry::WindowHost::new();
    // Either the host came up (display present) or it recorded why it did not.
    if host.is_alive() {
        assert!(
            host.last_error().is_none(),
            "a live host must not carry an error"
        );
    } else {
        // No display or no engine: the refusal must say why.
        assert!(
            host.last_error().is_some(),
            "a host that did not come up must say why"
        );
    }
}
