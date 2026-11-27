//! The modal dialog provider.
//!
//! The kit's `Dialog` is an overlay, not a complete window. `DialogPanel` is therefore
//! the small amount of provider chrome that makes it hostable: a real kit button behind
//! the overlay can reopen it, while the dialog itself remains the kit entity that owns
//! backdrop, Escape handling and focus restoration.

use std::sync::Arc;

use gpui::{
    App, AppContext as _, Context, Entity, IntoElement, ParentElement, Render, Styled, Window, div,
};
use nc_ui_kit::theme::Theme;
use nc_ui_kit::ui::{Button, Dialog};
use ncui_gpui::NativeEventSink;
use ncui_webview::NativeViewEvent;
use serde::Deserialize;

use crate::KitProviderError;
use crate::props;
use crate::provider::KitWidget;

const ID: &str = "nc-ui-kit::dialog";
const MAX_TITLE_BYTES: usize = 128;
const MAX_DESCRIPTION_BYTES: usize = 2048;
const MAX_TRIGGER_LABEL_BYTES: usize = 128;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DialogProps {
    title: String,
    description: Option<String>,
    trigger_label: String,
    open: bool,
    width: f32,
}

impl Default for DialogProps {
    fn default() -> Self {
        Self {
            title: "Confirmation".to_owned(),
            description: None,
            trigger_label: "Open dialog".to_owned(),
            open: true,
            width: 420.0,
        }
    }
}

/// The provider chrome that pairs a reopen button with the kit dialog entity.
pub struct DialogPanel {
    trigger: Entity<Button>,
    dialog: Entity<Dialog>,
}

impl Render for DialogPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = Theme::get(cx);
        div()
            .size_full()
            .relative()
            .bg(theme.bg)
            .child(
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(self.trigger.clone()),
            )
            .child(self.dialog.clone())
    }
}

/// The stateless implementation of `nc-ui-kit::dialog`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct DialogWidget;

impl DialogWidget {
    fn validate_shape(object: &props::Object) -> Result<(), KitProviderError> {
        props::check_string(ID, object, "title", MAX_TITLE_BYTES, false)?;
        props::check_string(ID, object, "description", MAX_DESCRIPTION_BYTES, true)?;
        props::check_string(ID, object, "trigger_label", MAX_TRIGGER_LABEL_BYTES, false)?;
        props::check_boolean(ID, object, "open")?;
        props::check_number(ID, object, "width", 240.0, 560.0)
    }

    fn emit_state(events: &NativeEventSink, title: &str, action: &str, origin: &str) {
        events.emit(
            NativeViewEvent::new("dialog", action).with_payload(serde_json::json!({
                "title": title,
                "origin": origin,
            })),
        );
    }

    fn set_open(
        view: &Entity<DialogPanel>,
        open: bool,
        events: &NativeEventSink,
        title: &str,
        origin: &str,
        window: &mut Window,
        cx: &mut App,
    ) {
        let was_open = view.read(cx).dialog.read(cx).open;
        if was_open == open {
            return;
        }
        view.update(cx, |panel, cx| {
            panel.dialog.update(cx, |dialog, cx| {
                dialog.set_open(open, window, cx);
            });
        });
        Self::emit_state(
            events,
            title,
            if open { "opened" } else { "closed" },
            origin,
        );
    }
}

impl KitWidget for DialogWidget {
    type View = DialogPanel;
    type Props = DialogProps;

    const ID: &'static str = ID;

    fn parse_props(value: &serde_json::Value) -> Result<Self::Props, KitProviderError> {
        props::decode(
            ID,
            value,
            &["title", "description", "trigger_label", "open", "width"],
            Self::validate_shape,
        )
    }

    fn build_child(
        props: &Self::Props,
        events: Arc<NativeEventSink>,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<Entity<Self::View>, KitProviderError> {
        let close_events = Arc::clone(&events);
        let close_title = props.title.clone();
        let mut dialog = Dialog::new(props.title.clone())
            .width(props.width)
            .on_close(move |_window, _cx| {
                Self::emit_state(&close_events, &close_title, "closed", "interaction");
            });
        if let Some(description) = &props.description {
            dialog = dialog.description(description.clone());
        }
        let dialog = dialog.finish(cx);
        if props.open {
            dialog.update(cx, |dialog, cx| {
                dialog.set_open(true, window, cx);
            });
        }

        let trigger_events = Arc::clone(&events);
        let trigger_title = props.title.clone();
        let trigger_dialog = dialog.clone();
        let trigger = cx.new(|_| {
            Button::new(props.trigger_label.clone())
                .primary()
                .on_click(move |window, cx| {
                    trigger_dialog.update(cx, |dialog, cx| {
                        dialog.set_open(true, window, cx);
                    });
                    Self::emit_state(&trigger_events, &trigger_title, "opened", "interaction");
                })
        });
        Ok(cx.new(|_| DialogPanel { trigger, dialog }))
    }

    fn invoke_child(
        view: &Entity<Self::View>,
        props: &Self::Props,
        events: Arc<NativeEventSink>,
        action: &str,
        payload: &serde_json::Value,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<(), KitProviderError> {
        match action {
            "open" | "close" => {
                props::ensure_empty_payload(ID, action, payload)?;
                Self::set_open(
                    view,
                    action == "open",
                    &events,
                    &props.title,
                    "command",
                    window,
                    cx,
                );
                Ok(())
            }
            _ => Err(props::unknown_action(ID, action)),
        }
    }

    fn padding() -> f32 {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(value: &serde_json::Value) -> Result<DialogProps, KitProviderError> {
        <DialogWidget as KitWidget>::parse_props(value)
    }

    #[test]
    fn null_props_produce_the_documented_defaults() {
        let props = parse(&serde_json::Value::Null).expect("null is the empty object shorthand");
        assert_eq!(props.title, "Confirmation");
        assert_eq!(props.trigger_label, "Open dialog");
        assert!(props.open);
        assert!((props.width - 420.0).abs() < f32::EPSILON);
    }

    #[test]
    fn an_out_of_range_width_names_the_width_field() {
        let error =
            parse(&json!({"width": 100})).expect_err("a narrow dialog is outside the schema");
        assert!(error.to_string().contains("width"));
        assert!(error.to_string().contains("240"));
    }

    #[test]
    fn a_non_string_description_names_the_description_field() {
        let error =
            parse(&json!({"description": 3})).expect_err("a numeric description is invalid");
        assert!(error.to_string().contains("description"));
    }

    #[test]
    fn an_unknown_field_is_refused() {
        let error = parse(&json!({"closable": true})).expect_err("unknown fields are invalid");
        assert!(error.to_string().contains("closable"));
    }
}
