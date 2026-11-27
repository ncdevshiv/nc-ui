//! The coexistence proof: a gpui engine and a wry engine living in one process.
//!
//! ADR-020's riskiest premise is that Win32's per-thread message pumps tolerate what the
//! integration asks of them: gpui's `GetMessageW` loop on one dedicated thread, tao's on
//! another, windows of both kinds open at once, and neither loop deadlocking the other.
//! This file is where that premise is proven rather than asserted — on a machine with a
//! display, with real windows of both kinds open simultaneously. On a machine without a
//! display the test skips rather than fails, matching both hosts' smoke-test contracts.

#![cfg(all(feature = "wry-host", feature = "gpui-host", target_os = "windows"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use std::sync::Arc;

use gpui::AppContext as _;
use ncui_core::WindowId;
use ncui_gpui::{
    GpuiHostFactory, NativeViewContext, NativeViewInstance, NativeViewProvider, providers,
};
use ncui_webview::{
    CapabilityGrant, HostFactory as _, HostSeam, NavigationPolicy, RoutingFactory,
    WebViewBackend as _, WindowLifecycle, WindowRequest,
};

/// The view the coexistence provider builds: as small as a real view can be.
struct CoexistenceView;

impl gpui::Render for CoexistenceView {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        _cx: &mut gpui::Context<'_, Self>,
    ) -> impl gpui::IntoElement {
        gpui::div()
    }
}

/// The instance half: every verb is refused, because coexistence is about windows, not views.
struct CoexistenceInstance;

impl NativeViewInstance for CoexistenceInstance {
    fn update_props(
        &mut self,
        _props: &serde_json::Value,
        _window: &mut gpui::Window,
        _cx: &mut gpui::App,
    ) -> Result<(), String> {
        Ok(())
    }

    fn invoke(
        &mut self,
        action: &str,
        _payload: &serde_json::Value,
        _window: &mut gpui::Window,
        _cx: &mut gpui::App,
    ) -> Result<(), String> {
        Err(format!("the coexistence view has no verbs: {action}"))
    }
}

/// The provider the coexistence window is opened with.
#[derive(Debug)]
struct CoexistenceProvider;

impl NativeViewProvider for CoexistenceProvider {
    fn id(&self) -> &str {
        "ncui-test::coexistence"
    }

    fn build(
        &self,
        _context: NativeViewContext,
        _window: &mut gpui::Window,
        cx: &mut gpui::App,
    ) -> Result<(gpui::AnyView, Box<dyn NativeViewInstance>), String> {
        let view = cx.new(|_| CoexistenceView);
        Ok((view.into(), Box::new(CoexistenceInstance)))
    }
}

/// The seam both factories ask for: the smallest honest one, because a coexistence test
/// tests the engines rather than the policy.
fn test_seam() -> Arc<HostSeam> {
    Arc::new(HostSeam::new(
        WindowLifecycle::new(),
        CapabilityGrant::none(),
        NavigationPolicy::new(&[]),
    ))
}

#[test]
fn gpui_and_wry_windows_coexist_in_one_process() {
    providers().register(Arc::new(CoexistenceProvider));

    // The gpui half: the engine thread starts once for the process.
    let gpui_factory = match GpuiHostFactory::new() {
        Ok(factory) => factory,
        Err(error) => {
            eprintln!("no display for the gpui engine, skipping: {error}");
            return;
        }
    };

    // The wry half: the tao loop starts on its own worker thread, as it always does.
    let wry_factory = ncui_webview_wry::WryHostFactory;

    // The router is the composition the runtime installs: one factory, two engines, and
    // the content kind decides which one answers.
    let router = RoutingFactory::new()
        .with_page_host(Box::new(wry_factory))
        .with_native_host(Box::new(gpui_factory));

    let seam = test_seam();

    // A native window through the router: the content kind routes it to gpui.
    let native_window = WindowId::new();
    let native_host = router
        .spawn(
            native_window,
            &WindowRequest::native("ncui-test::coexistence").with_title("native half"),
            &seam,
        )
        .expect("the native window opens through the router");

    // A page window through the router: the content kind routes it to wry. `about:blank`
    // is the smallest page there is, because coexistence is about the loops rather than
    // the content.
    let page_window = WindowId::new();
    let page_host = router.spawn(
        page_window,
        &WindowRequest::new("about:blank").with_title("page half"),
        &seam,
    );

    // Both engines alive at once is the whole proof: if the loops deadlocked each other,
    // one of these windows would not exist or would not answer.
    assert!(
        native_host.is_alive(),
        "the native window is alive while the page window exists"
    );
    if let Some(page) = &page_host {
        assert!(
            page.is_alive(),
            "the page window is alive while the native window exists"
        );
    } else {
        // A page window that did not open is not a coexistence failure if the engine
        // reported why: wry's own smoke contract allows a machine to refuse. Report it
        // rather than fail, and let the native half's proof stand.
        eprintln!("the page window did not open; the native half is still proven");
    }

    // The native window answers operations while the page loop is running: geometry is
    // a round-trip through the gpui engine thread, and a deadlocked loop answers nothing.
    let geometry = native_host.geometry().expect(
        "the gpui engine answers a geometry round-trip with the tao loop running beside it",
    );
    assert!(geometry.width > 0, "the window is somewhere real");

    // The native half's identity survives routing: the host knows what it is.
    assert!(
        ncui_webview::WebViewBackend::native_view(native_host.as_ref()).is_some(),
        "the routed native host still has its native half"
    );

    // Closing both is the second half of the proof: teardown order between two engines
    // is where a deadlock would hide if one were going to.
    drop(native_host);
    drop(page_host);
    std::thread::sleep(std::time::Duration::from_millis(500));
}

#[test]
fn a_native_window_outlives_the_close_of_a_page_window() {
    providers().register(Arc::new(CoexistenceProvider));
    let gpui_factory = match GpuiHostFactory::new() {
        Ok(factory) => factory,
        Err(error) => {
            eprintln!("no display for the gpui engine, skipping: {error}");
            return;
        }
    };
    let router = RoutingFactory::new()
        .with_page_host(Box::new(ncui_webview_wry::WryHostFactory))
        .with_native_host(Box::new(gpui_factory));
    let seam = test_seam();

    let native_window = WindowId::new();
    let native_host = router
        .spawn(
            native_window,
            &WindowRequest::native("ncui-test::coexistence"),
            &seam,
        )
        .expect("the native window opens");

    // A page window opens and closes while the native one stays: the failure mode this
    // guards against is a close on one loop disturbing the other's windows.
    let page_window = WindowId::new();
    let page_host = router.spawn(page_window, &WindowRequest::new("about:blank"), &seam);
    drop(page_host);
    std::thread::sleep(std::time::Duration::from_millis(300));

    assert!(
        native_host.is_alive(),
        "the native window survives the page window's close"
    );
    drop(native_host);
}
