//! Headless gpui tests for the concrete provider set.
//!
//! These tests use gpui's own test platform rather than a mock: views are built inside a
//! test window, rendered until parked, updated and invoked through the same trait methods
//! the runtime engine uses.
#![recursion_limit = "2048"]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use std::cell::RefCell;
use std::ops::Deref;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    point, px, AnyView, AnyWeakView, AnyWindowHandle, Context, IntoElement, Modifiers, Render,
    TestAppContext, VisualTestContext, Window,
};
use ncui_core::WindowId;
use ncui_gpui::{providers, NativeEventSink, NativeViewContext, NativeViewInstance};
use ncui_kit_providers::{apply_theme, register_all, PROVIDER_IDS};
use serde_json::{json, Value};

/// Generic host: renders the provider root the way the runtime's native root does.
struct Host(AnyView);

impl Render for Host {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.0.clone()
    }
}

type BuiltParts = (AnyView, Box<dyn NativeViewInstance>);

struct BuiltProvider {
    visual: VisualTestContext,
    instance: Box<dyn NativeViewInstance>,
    weak: AnyWeakView,
    events: Arc<NativeEventSink>,
}

fn build_provider(cx: &mut TestAppContext, id: &str, props: Value) -> BuiltProvider {
    let provider = providers().get(id).expect("the provider is registered");
    let events = Arc::new(NativeEventSink::new());
    let window_id = WindowId::new();
    let slot: Rc<RefCell<Option<BuiltParts>>> = Rc::new(RefCell::new(None));
    let closure_slot = Rc::clone(&slot);
    let closure_events = Arc::clone(&events);

    let window_handle = cx.add_window(move |window, cx| {
        let context = NativeViewContext {
            window: window_id,
            props: props.clone(),
            events: closure_events,
        };
        let (view, instance) = provider
            .build(context, window, cx)
            .expect("valid example props build the provider view");
        *closure_slot.borrow_mut() = Some((view.clone(), instance));
        Host(view)
    });

    let visual = VisualTestContext::from_window(*window_handle.deref(), cx);
    let (view, instance) = slot
        .borrow_mut()
        .take()
        .expect("the window construction captured the provider output");
    let weak = view.downgrade();
    drop(view);
    BuiltProvider { visual, instance, weak, events }
}

fn build_registered(cx: &mut TestAppContext, id: &str, props: Value) -> BuiltProvider {
    assert_eq!(register_all(), PROVIDER_IDS.len());
    build_provider(cx, id, props)
}

#[gpui::test]
fn every_provider_builds_updates_and_invokes_headlessly(cx: &mut TestAppContext) {
    let cases: [(&str, Value, &str, Value); 6] = [
        ("nc-ui-kit::button", json!({"label": "Save", "variant": "primary"}), "click", json!({})),
        ("nc-ui-kit::input", json!({"value": "hello", "placeholder": "Name"}), "clear", json!({})),
        (
            "nc-ui-kit::dropdown",
            json!({"options": ["Alpha", "Beta"], "selected": 1}),
            "select",
            json!({"index": 0}),
        ),
        (
            "nc-ui-kit::checkbox",
            json!({"label": "Enable sync", "checked": true}),
            "toggle",
            json!({}),
        ),
        (
            "nc-ui-kit::dialog",
            json!({"title": "Delete file", "description": "This cannot be undone.", "open": true}),
            "close",
            json!({}),
        ),
        (
            "nc-ui-kit::settings-tabs",
            json!({"active": "general", "panels": {"advanced": "Advanced settings copy."}}),
            "select",
            json!({"tab": "advanced"}),
        ),
    ];

    for (id, props, action, payload) in cases {
        let mut built = build_registered(cx, id, props.clone());
        built.visual.run_until_parked();
        assert!(built.weak.upgrade().is_some(), "{id} did not keep a live root entity");

        let updated = built.visual.update(|window, cx| {
            built.instance.update_props(&props, window, cx)
        });
        assert!(updated.is_ok(), "{id} refused valid replacement props: {}", updated.unwrap_err());
        built.visual.run_until_parked();

        let invoked = built.visual.update(|window, cx| {
            built.instance.invoke(action, &payload, window, cx)
        });
        assert!(invoked.is_ok(), "{id} refused `{action}`: {}", invoked.unwrap_err());
        built.visual.run_until_parked();
        assert!(built.weak.upgrade().is_some(), "{id} lost its root entity across commands");
    }
}

#[gpui::test]
fn a_button_click_emits_the_real_native_event(cx: &mut TestAppContext) {
    let mut built = build_registered(
        cx,
        "nc-ui-kit::button",
        json!({"label": "Save", "variant": "primary"}),
    );
    built.visual.run_until_parked();

    // The shared root pads the child by 20px; a medium button is 36px high.
    built.visual.simulate_click(point(px(30.), px(38.)), Modifiers::default());
    built.visual.run_until_parked();

    let event = built.events.poll().expect("the click handler emits an event");
    assert_eq!(event.source, "button");
    assert_eq!(event.action, "clicked");
    assert_eq!(event.payload, json!({"label": "Save", "origin": "interaction"}));
}

#[gpui::test]
fn typing_in_the_input_emits_the_real_change_event(cx: &mut TestAppContext) {
    let mut built = build_registered(
        cx,
        "nc-ui-kit::input",
        json!({"placeholder": "Name"}),
    );
    built.visual.run_until_parked();

    built.visual.simulate_click(point(px(30.), px(37.)), Modifiers::default());
    built.visual.run_until_parked();
    built.visual.simulate_keystrokes("a");
    built.visual.run_until_parked();

    let event = built.events.poll().expect("typing emits a change event");
    assert_eq!(event.source, "input");
    assert_eq!(event.action, "changed");
    assert_eq!(event.payload, json!({"value": "a", "origin": "interaction"}));
}

#[gpui::test]
fn applying_the_runtime_theme_sets_the_kit_global(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let applied = apply_theme(ncui_webview::Theme::Dark, cx);
        assert_eq!(applied.mode, nc_ui_kit::theme::ThemeMode::Dark);
        assert_eq!(nc_ui_kit::theme::Theme::get(cx).mode, nc_ui_kit::theme::ThemeMode::Dark);
    });
}
