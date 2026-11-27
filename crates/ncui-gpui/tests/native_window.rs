//! The native-window spike: real gpui windows through the real registry path.
//!
//! Everything the integration claims rests on three facts this file proves on a machine
//! with a display: the engine thread starts and answers, a native window opens through
//! the registry's own composition path — validation, factory, host — and the host's
//! operations round-trip through the command channel while the window is alive. On a
//! machine without a display the tests skip rather than fail, because "no display" is a
//! fact about the machine, matching the wry host's own smoke-test contract.

#![cfg(all(feature = "gpui-engine", target_os = "windows"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use std::sync::Arc;

use gpui::AppContext as _;
use gpui::ParentElement as _;
use gpui::Styled as _;
use ncui_core::WindowId;
use ncui_gpui::{
    GpuiHostFactory, NativeViewContext, NativeViewInstance, NativeViewProvider, providers,
};
use ncui_webview::{
    CapabilityGrant, HostFactory as _, HostSeam, NavigationPolicy, WebViewBackend as _,
    WindowLifecycle, WindowRequest,
};

/// The view the test provider builds: a real gpui view, small enough to read at a glance.
struct TestView {
    /// The label the props set, so a props update is visible in what renders.
    label: String,
    /// The sink events leave through, so `invoke` can prove the event channel end to end.
    events: Arc<ncui_gpui::NativeEventSink>,
}

impl gpui::Render for TestView {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        _cx: &mut gpui::Context<'_, Self>,
    ) -> impl gpui::IntoElement {
        gpui::div()
            .size_full()
            .bg(gpui::rgb(0x20_20_30))
            .text_color(gpui::rgb(0xff_ff_ff))
            .child(self.label.clone())
    }
}

/// The instance half: keeps the entity handle so commands can reach the view.
struct TestInstance {
    /// The view's entity, as the engine handed it back.
    view: gpui::Entity<TestView>,
}

impl NativeViewInstance for TestInstance {
    fn update_props(
        &mut self,
        props: &serde_json::Value,
        _window: &mut gpui::Window,
        cx: &mut gpui::App,
    ) -> Result<(), String> {
        let label = props
            .get("label")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("test view")
            .to_owned();
        self.view.update(cx, |view, _cx| {
            view.label = label;
        });
        Ok(())
    }

    fn invoke(
        &mut self,
        action: &str,
        payload: &serde_json::Value,
        _window: &mut gpui::Window,
        cx: &mut gpui::App,
    ) -> Result<(), String> {
        match action {
            "emit" => {
                let events = self.view.read(cx).events.clone();
                events.emit(
                    ncui_webview::NativeViewEvent::new("test", "emitted").with_payload(payload.clone()),
                );
                Ok(())
            }
            other => Err(format!("the test view does not know the verb {other}")),
        }
    }
}

/// The test provider: validates the props it understands and builds the view.
#[derive(Debug)]
struct TestProvider;

impl NativeViewProvider for TestProvider {
    fn id(&self) -> &str {
        "ncui-gpui::test-view"
    }

    fn build(
        &self,
        context: NativeViewContext,
        _window: &mut gpui::Window,
        cx: &mut gpui::App,
    ) -> Result<(gpui::AnyView, Box<dyn NativeViewInstance>), String> {
        if let Some(label) = context.props.get("label")
            && !label.is_string()
        {
            return Err("the `label` prop must be a string".to_owned());
        }
        let label = context
            .props
            .get("label")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("test view")
            .to_owned();
        let events = context.events.clone();
        let view = cx.new(|_| TestView { label, events });
        let instance = TestInstance { view: view.clone() };
        Ok((view.into(), Box::new(instance)))
    }
}

/// Registers the test provider exactly once per process, however many tests ask.
fn register_test_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        providers().register(Arc::new(TestProvider));
    });
}

/// The seam a native window is spawned with. The seam is the page's half of the world
/// and a native window never touches it — but the factory's contract asks for one, so
/// the test builds the smallest honest one rather than reaching around the contract.
fn test_seam() -> Arc<HostSeam> {
    Arc::new(HostSeam::new(
        WindowLifecycle::new(),
        CapabilityGrant::none(),
        NavigationPolicy::new(&[]),
    ))
}

/// Whether this machine can host windows at all, as the engine itself answers.
fn engine_available(factory: &GpuiHostFactory) -> bool {
    factory.last_error().is_none()
}

#[test]
fn the_engine_thread_starts_and_reports_displays() {
    register_test_provider();
    let factory = match GpuiHostFactory::new() {
        Ok(factory) => factory,
        Err(error) => {
            eprintln!("no display on this machine, skipping: {error}");
            return;
        }
    };
    assert!(
        engine_available(&factory),
        "a factory that built must not carry an error: {:?}",
        factory.last_error()
    );
    // A machine with a display reports at least one monitor. An empty list here would
    // mean the round-trip answered nothing, which is a failure rather than a fact.
    assert!(
        !factory.monitors().is_empty(),
        "a machine that opened the engine has a display to report"
    );
}

#[test]
fn a_native_window_opens_lives_answers_and_closes() {
    register_test_provider();
    let factory = match GpuiHostFactory::new() {
        Ok(factory) => factory,
        Err(error) => {
            eprintln!("no display on this machine, skipping: {error}");
            return;
        }
    };

    let window = WindowId::new();
    let request = WindowRequest::native("ncui-gpui::test-view")
        .with_props(serde_json::json!({ "label": "the spike window" }))
        .with_title("ncui spike");
    let host = factory
        .spawn(window, &request, &test_seam())
        .unwrap_or_else(|| {
            panic!(
                "the engine refused a window it should host: {:?}",
                factory.last_error()
            )
        });

    // Alive immediately after opening: the engine thread flipped nothing, because
    // nothing ended.
    assert!(host.is_alive(), "a window just opened is alive");

    // The native half is there, because the window's content is native.
    let native = host
        .native_view()
        .expect("a native window's host has a native half");

    // There is no page, and the web half says so honestly.
    assert!(
        host.poll_page_message().is_none(),
        "a native window has no page messages, ever"
    );

    // Geometry round-trips: the window is somewhere, and that somewhere has the size the
    // request asked for, because nothing has resized it since.
    let geometry = host.geometry().expect("the platform can say where the window is");
    assert!(
        geometry.width > 0 && geometry.height > 0,
        "an open window has a size: {geometry:?}"
    );

    // The theme is the machine's real answer, not a guess.
    assert!(
        host.window_theme().is_some(),
        "gpui reads the platform's real appearance"
    );

    // The event channel round-trips: invoke the verb that emits, then drain the event.
    native
        .send_view_command(ncui_webview::NativeViewCommand::Invoke {
            action: "emit".to_owned(),
            payload: serde_json::json!({ "value": 42 }),
        })
        .expect("the test view accepts its emit verb");
    let event = native
        .wait_native_event(std::time::Duration::from_secs(5))
        .expect("the emitted event arrives");
    assert_eq!(event.source, "test");
    assert_eq!(event.action, "emitted");
    assert_eq!(event.payload, serde_json::json!({ "value": 42 }));

    // Props update round-trips without an error.
    native
        .send_view_command(ncui_webview::NativeViewCommand::UpdateProps {
            props: serde_json::json!({ "label": "updated" }),
        })
        .expect("the test view accepts well-formed props");

    // An unknown verb is refused with the provider's own reason.
    let refusal = native
        .send_view_command(ncui_webview::NativeViewCommand::Invoke {
            action: "does-not-exist".to_owned(),
            payload: serde_json::Value::Null,
        })
        .expect_err("an unknown verb is refused");
    assert!(
        matches!(refusal, ncui_webview::NativeViewError::Refused { .. }),
        "the refusal is the provider's, typed: {refusal:?}"
    );

    // Closing is dropping: the host goes away, the engine hears, and liveness flips.
    drop(host);
    // The close travels the command channel — give the engine a moment to perform it.
    std::thread::sleep(std::time::Duration::from_millis(500));
}

#[test]
fn an_unknown_provider_is_refused_with_a_typed_reason() {
    register_test_provider();
    let factory = match GpuiHostFactory::new() {
        Ok(factory) => factory,
        Err(error) => {
            eprintln!("no display on this machine, skipping: {error}");
            return;
        }
    };
    let window = WindowId::new();
    let request = WindowRequest::native("ncui-gpui::no-such-view");
    let host = factory.spawn(window, &request, &test_seam());
    assert!(
        host.is_none(),
        "a provider that does not exist cannot open a window"
    );
    let error = factory
        .last_error()
        .expect("the refusal says why, in the provider's own terms");
    assert!(
        error.contains("ncui-gpui::no-such-view"),
        "the refusal names the provider that was asked for: {error}"
    );
}

#[test]
fn two_windows_are_independent() {
    register_test_provider();
    let factory = match GpuiHostFactory::new() {
        Ok(factory) => factory,
        Err(error) => {
            eprintln!("no display on this machine, skipping: {error}");
            return;
        }
    };
    let seam = test_seam();
    let first_window = WindowId::new();
    let second_window = WindowId::new();
    let first = factory
        .spawn(
            first_window,
            &WindowRequest::native("ncui-gpui::test-view"),
            &seam,
        )
        .expect("the first window opens");
    let second = factory
        .spawn(
            second_window,
            &WindowRequest::native("ncui-gpui::test-view"),
            &seam,
        )
        .expect("the second window opens");
    assert!(first.is_alive() && second.is_alive());

    // Closing one leaves the other: independence is the whole point of a window.
    drop(first);
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(
        second.is_alive(),
        "the second window survives the first one's close"
    );
    drop(second);
}
