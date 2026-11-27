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
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyView, AnyWeakView, Context, IntoElement, Modifiers, Render, TestAppContext,
    VisualTestContext, Window, point, px,
};
use ncui_core::WindowId;
use ncui_gpui::{NativeEventSink, NativeViewContext, NativeViewInstance, providers};
use ncui_kit_providers::{PROVIDER_IDS, apply_theme, register_all};
use serde_json::{Value, json};

/// Generic host: renders the provider root the way the runtime's native root does.
struct Host(AnyView);

impl Render for Host {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        self.0.clone()
    }
}

struct Empty;

impl Render for Empty {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        gpui::div()
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

    let visual = VisualTestContext::from_window(window_handle.into(), cx);
    let (view, instance) = slot
        .borrow_mut()
        .take()
        .expect("the window construction captured the provider output");
    let weak = view.downgrade();
    drop(view);
    BuiltProvider {
        visual,
        instance,
        weak,
        events,
    }
}

fn build_registered(cx: &mut TestAppContext, id: &str, props: Value) -> BuiltProvider {
    assert_eq!(register_all(), PROVIDER_IDS.len());
    build_provider(cx, id, props)
}

#[gpui::test]
fn a_build_refusal_preserves_its_machine_readable_code(cx: &mut TestAppContext) {
    assert_eq!(register_all(), PROVIDER_IDS.len());
    let provider = providers()
        .get("nc-ui-kit::button")
        .expect("the button provider is registered");
    let events = Arc::new(NativeEventSink::new());
    let slot: Rc<RefCell<Option<Result<(), String>>>> = Rc::new(RefCell::new(None));
    let closure_slot = Rc::clone(&slot);

    cx.add_window(move |window, cx| {
        let result = provider
            .build(
                NativeViewContext {
                    window: WindowId::new(),
                    props: json!({"label": 3}),
                    events,
                },
                window,
                cx,
            )
            .map(|_| ());
        *closure_slot.borrow_mut() = Some(result);
        Empty
    });

    let error = slot
        .borrow_mut()
        .take()
        .expect("the window construction captured the build result")
        .expect_err("a numeric label is refused");
    assert!(
        error.starts_with("[kit_invalid_props]"),
        "the trait-boundary string keeps the stable code: {error}"
    );
    assert!(error.contains("label"));
}

#[gpui::test]
fn every_provider_builds_updates_and_invokes_headlessly(cx: &mut TestAppContext) {
    let cases: [(&str, Value, &str, Value, &str, Value); 6] = [
        (
            "nc-ui-kit::button",
            json!({"label": "Save", "variant": "primary"}),
            "click",
            json!({}),
            "clicked",
            json!({"label": "Save", "origin": "command"}),
        ),
        (
            "nc-ui-kit::input",
            json!({"value": "hello", "placeholder": "Name"}), // ncui-allow-placeholder: the input hint-text prop — a UI domain term, not unfinished work
            "clear",
            json!({}),
            "changed",
            json!({"value": "", "origin": "command"}),
        ),
        (
            "nc-ui-kit::dropdown",
            json!({"options": ["Alpha", "Beta"], "selected": 1}),
            "select",
            json!({"index": 0}),
            "selected",
            json!({"index": 0, "option": "Alpha", "origin": "command"}),
        ),
        (
            "nc-ui-kit::checkbox",
            json!({"label": "Enable sync", "checked": true}),
            "toggle",
            json!({}),
            "toggled",
            json!({"checked": false, "origin": "command"}),
        ),
        (
            "nc-ui-kit::dialog",
            json!({"title": "Delete file", "description": "This cannot be undone.", "open": true}),
            "close",
            json!({}),
            "closed",
            json!({"title": "Delete file", "origin": "command"}),
        ),
        (
            "nc-ui-kit::settings-tabs",
            json!({"active": "general", "panels": {"advanced": "Advanced settings copy."}}),
            "select",
            json!({"tab": "advanced"}),
            "selected",
            json!({"tab": "advanced", "origin": "command"}),
        ),
    ];

    for (id, props, action, payload, event_action, event_payload) in cases {
        let mut built = build_registered(cx, id, props.clone());
        built.visual.run_until_parked();
        let mode = built
            .visual
            .update(|_, cx| nc_ui_kit::theme::Theme::get(cx).mode);
        assert_eq!(
            mode,
            nc_ui_kit::theme::ThemeMode::Light,
            "{id} did not apply the test window's light appearance at construction"
        );
        assert!(
            built.weak.upgrade().is_some(),
            "{id} did not keep a live root entity"
        );

        let updated = built
            .visual
            .update(|window, cx| built.instance.update_props(&props, window, cx));
        assert!(
            updated.is_ok(),
            "{id} refused valid replacement props: {}",
            updated.unwrap_err()
        );
        built.visual.run_until_parked();
        let invoked = built
            .visual
            .update(|window, cx| built.instance.invoke(action, &payload, window, cx));
        assert!(
            invoked.is_ok(),
            "{id} refused `{action}`: {}",
            invoked.unwrap_err()
        );
        built.visual.run_until_parked();

        let event = built
            .events
            .poll()
            .expect("an accepted state-changing command emits its native event");
        assert_eq!(event.source, id.trim_start_matches("nc-ui-kit::"));
        assert_eq!(event.action, event_action);
        assert_eq!(event.payload, event_payload);
        assert!(
            built.weak.upgrade().is_some(),
            "{id} lost its root entity across commands"
        );
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
    built
        .visual
        .simulate_click(point(px(30.), px(38.)), Modifiers::default());
    built.visual.run_until_parked();

    let event = built
        .events
        .poll()
        .expect("the click handler emits an event");
    assert_eq!(event.source, "button");
    assert_eq!(event.action, "clicked");
    assert_eq!(
        event.payload,
        json!({"label": "Save", "origin": "interaction"})
    );
}

#[gpui::test]
fn typing_in_the_input_emits_the_real_change_event(cx: &mut TestAppContext) {
    let mut built = build_registered(cx, "nc-ui-kit::input", json!({"placeholder": "Name"})); // ncui-allow-placeholder: the input hint-text prop — a UI domain term, not unfinished work
    built.visual.run_until_parked();

    built
        .visual
        .simulate_click(point(px(30.), px(37.)), Modifiers::default());
    built.visual.run_until_parked();
    built.visual.simulate_keystrokes("a");
    built.visual.run_until_parked();

    let event = built.events.poll().expect("typing emits a change event");
    assert_eq!(event.source, "input");
    assert_eq!(event.action, "changed");
    assert_eq!(
        event.payload,
        json!({"value": "a", "origin": "interaction"})
    );
}

#[gpui::test]
fn a_checkbox_click_emits_the_real_toggle_event(cx: &mut TestAppContext) {
    let mut built = build_registered(
        cx,
        "nc-ui-kit::checkbox",
        json!({"label": "Enable sync", "checked": false}),
    );
    built.visual.run_until_parked();

    // The shared root pads the control by 20px; the 16px checkbox square is centred
    // at approximately (28, 28).
    built
        .visual
        .simulate_click(point(px(28.), px(28.)), Modifiers::default());
    built.visual.run_until_parked();

    let event = built
        .events
        .poll()
        .expect("the checkbox click emits an event");
    assert_eq!(event.source, "checkbox");
    assert_eq!(event.action, "toggled");
    assert_eq!(
        event.payload,
        json!({"checked": true, "origin": "interaction"})
    );
}

#[gpui::test]
fn dropdown_keyboard_selection_emits_the_real_selection_event(cx: &mut TestAppContext) {
    let mut built = build_registered(
        cx,
        "nc-ui-kit::dropdown",
        json!({"options": ["Alpha", "Beta"], "selected": 0}),
    );
    built.visual.run_until_parked();

    // The trigger is 220x34 starting at the root's 20px padding. Opening it focuses
    // the menu, after which Down+Enter is the kit's real keyboard selection path.
    built
        .visual
        .simulate_click(point(px(130.), px(37.)), Modifiers::default());
    built.visual.run_until_parked();
    built.visual.simulate_keystrokes("down enter");
    built.visual.run_until_parked();

    let event = built
        .events
        .poll()
        .expect("keyboard selection emits an event");
    assert_eq!(event.source, "dropdown");
    assert_eq!(event.action, "selected");
    assert_eq!(
        event.payload,
        json!({"index": 1, "option": "Beta", "origin": "interaction"})
    );
}

#[gpui::test]
fn pressing_escape_in_the_dialog_emits_the_real_close_event(cx: &mut TestAppContext) {
    let mut built = build_registered(
        cx,
        "nc-ui-kit::dialog",
        json!({"title": "Delete file", "description": "This cannot be undone.", "open": true}),
    );
    built.visual.run_until_parked();

    built.visual.simulate_keystrokes("escape");
    built.visual.run_until_parked();

    let event = built
        .events
        .poll()
        .expect("Escape emits the dialog close event");
    assert_eq!(event.source, "dialog");
    assert_eq!(event.action, "closed");
    assert_eq!(
        event.payload,
        json!({"title": "Delete file", "origin": "interaction"})
    );
}

#[gpui::test]
fn clicking_a_settings_tab_emits_the_real_selection_event(cx: &mut TestAppContext) {
    let mut built = build_registered(cx, "nc-ui-kit::settings-tabs", json!({"active": "general"}));
    built.visual.run_until_parked();

    // The second fixed tab, Appearance, starts after General plus the kit's 2px gap.
    built
        .visual
        .simulate_click(point(px(120.), px(15.)), Modifiers::default());
    built.visual.run_until_parked();

    let event = built.events.poll().expect("the tab click emits an event");
    assert_eq!(event.source, "settings-tabs");
    assert_eq!(event.action, "selected");
    assert_eq!(
        event.payload,
        json!({"tab": "appearance", "origin": "interaction"})
    );
}

#[gpui::test]
fn applying_the_runtime_theme_sets_the_kit_global(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let applied = apply_theme(ncui_webview::Theme::Dark, cx);
        assert_eq!(applied.mode, nc_ui_kit::theme::ThemeMode::Dark);
        assert_eq!(
            nc_ui_kit::theme::Theme::get(cx).mode,
            nc_ui_kit::theme::ThemeMode::Dark
        );
    });
}
