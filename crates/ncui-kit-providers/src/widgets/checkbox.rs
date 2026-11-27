//! The labelled boolean checkbox provider.
//!
//! A checkbox looks trivial until it is scripted. The provider separates activation
//! (`toggle`, refused while disabled because it stands in for a user gesture) from state
//! control (`set_checked`, available to the trusted host because props and commands are
//! already host-driven), which keeps those two meanings distinct.

use std::sync::Arc;

use nc_ui_kit::ui::Checkbox;
use ncui_gpui::NativeEventSink;
use ncui_webview::NativeViewEvent;
use serde::Deserialize;

use crate::props;
use crate::provider::KitWidget;
use crate::KitProviderError;

const ID: &str = "nc-ui-kit::checkbox";
const MAX_LABEL_BYTES: usize = 256;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct CheckboxProps {
    checked: bool,
    label: String,
    disabled: bool,
}

impl Default for CheckboxProps {
    fn default() -> Self {
        Self { checked: false, label: "Enabled".to_owned(), disabled: false }
    }
}

/// The stateless implementation of `nc-ui-kit::checkbox`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CheckboxWidget;

impl CheckboxWidget {
    fn validate_shape(object: &props::Object) -> Result<(), KitProviderError> {
        props::check_boolean(ID, object, "checked")?;
        props::check_string(ID, object, "label", MAX_LABEL_BYTES, false)?;
        props::check_boolean(ID, object, "disabled")
    }

    fn emit_toggled(events: &NativeEventSink, checked: bool, origin: &str) {
        events.emit(
            NativeViewEvent::new("checkbox", "toggled").with_payload(serde_json::json!({
                "checked": checked,
                "origin": origin,
            })),
        );
    }

    fn set_checked(
        view: &gpui::Entity<Checkbox>,
        events: &NativeEventSink,
        checked: bool,
        origin: &str,
        cx: &mut gpui::App,
    ) {
        let changed = view.read(cx).checked != checked;
        if !changed {
            return;
        }
        view.update(cx, |checkbox, cx| {
            checkbox.checked = checked;
            cx.notify();
        });
        Self::emit_toggled(events, checked, origin);
    }
}

impl KitWidget for CheckboxWidget {
    type View = Checkbox;
    type Props = CheckboxProps;

    const ID: &'static str = ID;

    fn parse_props(value: &serde_json::Value) -> Result<Self::Props, KitProviderError> {
        props::decode(ID, value, &["checked", "label", "disabled"], Self::validate_shape)
    }

    fn build_child(
        props: &Self::Props,
        events: Arc<NativeEventSink>,
        _window: &mut gpui::Window,
        cx: &mut gpui::App,
    ) -> Result<gpui::Entity<Self::View>, KitProviderError> {
        let mut checkbox = Checkbox::new(props.checked)
            .label(props.label.clone())
            .on_change(move |checked, _window, _cx| {
                Self::emit_toggled(&events, checked, "interaction");
            });
        if props.disabled {
            checkbox = checkbox.disabled();
        }
        Ok(checkbox.finish(cx))
    }

    fn invoke_child(
        view: &gpui::Entity<Self::View>,
        props: &Self::Props,
        events: Arc<NativeEventSink>,
        action: &str,
        payload: &serde_json::Value,
        window: &mut gpui::Window,
        cx: &mut gpui::App,
    ) -> Result<(), KitProviderError> {
        match action {
            "toggle" => {
                props::ensure_empty_payload(ID, action, payload)?;
                if props.disabled {
                    return Err(props::action_unavailable(ID, action, "the checkbox is disabled"));
                }
                let next = !view.read(cx).checked;
                Self::set_checked(view, &events, next, "command", cx);
                Ok(())
            }
            "set_checked" => {
                let object = props::payload_object(ID, action, payload, &["checked"])?;
                let checked = props::payload_boolean(ID, action, object, "checked")?;
                Self::set_checked(view, &events, checked, "command", cx);
                Ok(())
            }
            "focus" => {
                props::ensure_empty_payload(ID, action, payload)?;
                if props.disabled {
                    return Err(props::action_unavailable(ID, action, "the checkbox is disabled"));
                }
                let focus = view.read(cx).focus_handle().clone();
                window.focus(&focus);
                Ok(())
            }
            _ => Err(props::unknown_action(ID, action)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(value: serde_json::Value) -> Result<CheckboxProps, KitProviderError> {
        <CheckboxWidget as KitWidget>::parse_props(&value)
    }

    #[test]
    fn null_props_produce_the_documented_defaults() {
        let props = parse(serde_json::Value::Null).expect("null is the empty object shorthand");
        assert!(!props.checked);
        assert_eq!(props.label, "Enabled");
        assert!(!props.disabled);
    }

    #[test]
    fn a_non_boolean_checked_value_names_its_field() {
        let error = parse(json!({"checked": "true"})).expect_err("a string is not a boolean");
        assert!(error.to_string().contains("checked"));
        assert!(error.to_string().contains("boolean"));
    }

    #[test]
    fn an_empty_label_is_refused() {
        let error = parse(json!({"label": ""})).expect_err("an empty label is invalid");
        assert!(error.to_string().contains("label"));
    }
}
