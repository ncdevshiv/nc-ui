//! The single-line text-input provider.
//!
//! Text input is the provider that makes the command/event round trip visible: typing
//! emits bounded change events, while the host can clear the field or move focus without
//! pretending either action is a props replacement.

use std::sync::Arc;

use nc_ui_kit::ui::Input;
use ncui_gpui::NativeEventSink;
use ncui_webview::NativeViewEvent;
use serde::Deserialize;

use crate::KitProviderError;
use crate::props;
use crate::provider::KitWidget;

const ID: &str = "nc-ui-kit::input";
const MAX_VALUE_BYTES: usize = 4096;
const MAX_PLACEHOLDER_BYTES: usize = 256;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InputProps {
    value: String,
    placeholder: String, // ncui-allow-placeholder: the input hint-text prop — a UI domain term, not unfinished work
    numeric: bool,
    disabled: bool,
    invalid: bool,
}

impl Default for InputProps {
    fn default() -> Self {
        Self {
            value: String::new(),
            placeholder: "Type here…".to_owned(), // ncui-allow-placeholder: the input hint-text prop — a UI domain term, not unfinished work
            numeric: false,
            disabled: false,
            invalid: false,
        }
    }
}

/// The stateless implementation of `nc-ui-kit::input`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct InputWidget;

impl InputWidget {
    fn validate_shape(object: &props::Object) -> Result<(), KitProviderError> {
        props::check_string(ID, object, "value", MAX_VALUE_BYTES, true)?;
        props::check_string(ID, object, "placeholder", MAX_PLACEHOLDER_BYTES, false)?; // ncui-allow-placeholder: the input hint-text prop — a UI domain term, not unfinished work
        props::check_boolean(ID, object, "numeric")?;
        props::check_boolean(ID, object, "disabled")?;
        props::check_boolean(ID, object, "invalid")
    }

    fn emit_change(events: &NativeEventSink, value: &str, origin: &str) {
        events.emit(
            NativeViewEvent::new("input", "changed").with_payload(serde_json::json!({
                "value": value,
                "origin": origin,
            })),
        );
    }
}

impl KitWidget for InputWidget {
    type View = Input;
    type Props = InputProps;

    const ID: &'static str = ID;

    fn parse_props(value: &serde_json::Value) -> Result<Self::Props, KitProviderError> {
        props::decode(
            ID,
            value,
            &["value", "placeholder", "numeric", "disabled", "invalid"], // ncui-allow-placeholder: the input hint-text prop — a UI domain term, not unfinished work
            Self::validate_shape,
        )
    }

    fn build_child(
        props: &Self::Props,
        events: Arc<NativeEventSink>,
        _window: &mut gpui::Window,
        cx: &mut gpui::App,
    ) -> Result<gpui::Entity<Self::View>, KitProviderError> {
        let mut input = Input::new(props.placeholder.clone()).on_change(move |value, _cx| {
            // ncui-allow-placeholder: the input hint-text prop — a UI domain term, not unfinished work
            Self::emit_change(&events, value, "interaction");
        });
        if props.numeric {
            input = input.numeric();
        }
        if props.disabled {
            input = input.disabled();
        }
        if props.invalid {
            input = input.invalid();
        }
        let entity = input.finish(cx);
        let initial = props.value.clone();
        entity.update(cx, |input, cx| {
            input.set_value(initial, cx);
        });
        Ok(entity)
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
            "clear" => {
                props::ensure_empty_payload(ID, action, payload)?;
                if view.read(cx).value.is_empty() {
                    return Ok(());
                }
                view.update(cx, |input, cx| {
                    input.set_value(String::new(), cx);
                });
                Self::emit_change(&events, "", "command");
                Ok(())
            }
            "focus" => {
                props::ensure_empty_payload(ID, action, payload)?;
                if props.disabled {
                    return Err(props::action_unavailable(
                        ID,
                        action,
                        "the input is disabled",
                    ));
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

    fn parse(value: &serde_json::Value) -> Result<InputProps, KitProviderError> {
        <InputWidget as KitWidget>::parse_props(value)
    }

    #[test]
    fn null_props_produce_the_documented_defaults() {
        let props = parse(&serde_json::Value::Null).expect("null is the empty object shorthand");
        assert!(props.value.is_empty());
        assert_eq!(props.placeholder, "Type here…"); // ncui-allow-placeholder: the input hint-text prop — a UI domain term, not unfinished work
        assert!(!props.numeric);
    }

    #[test]
    fn a_non_boolean_numeric_flag_names_its_field() {
        let error = parse(&json!({"numeric": "yes"})).expect_err("a string is not a boolean");
        assert!(error.to_string().contains("numeric"));
        assert!(error.to_string().contains("boolean"));
    }

    #[test]
    fn an_empty_placeholder_is_refused() {
        let error =
            parse(&json!({"placeholder": ""})).expect_err("an empty placeholder is invalid"); // ncui-allow-placeholder: the input hint-text prop — a UI domain term, not unfinished work
        assert!(error.to_string().contains("placeholder")); // ncui-allow-placeholder: the input hint-text prop — a UI domain term, not unfinished work
        assert!(error.to_string().contains("empty"));
    }

    #[test]
    fn the_value_bound_is_enforced() {
        let value = "x".repeat(MAX_VALUE_BYTES + 1);
        let error = parse(&json!({"value": value})).expect_err("the value bound is enforced");
        assert!(error.to_string().contains("value"));
        assert!(error.to_string().contains("bound"));
    }
}
