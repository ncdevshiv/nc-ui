//! The single-action button provider.
//!
//! A button is deliberately the smallest provider in the set: it exercises the whole
//! trust boundary — typed props, a real gpui entity, an interaction event and a command —
//! without hiding any of those pieces behind a composite view.

use std::sync::Arc;

use gpui::AppContext as _;
use nc_ui_kit::theme::Size;
use nc_ui_kit::ui::{Button, ButtonVariant};
use ncui_gpui::NativeEventSink;
use ncui_webview::NativeViewEvent;
use serde::Deserialize;

use crate::KitProviderError;
use crate::props;
use crate::provider::KitWidget;

const ID: &str = "nc-ui-kit::button";
const MAX_LABEL_BYTES: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum VariantSpec {
    Primary,
    Secondary,
    Outline,
    Ghost,
    Danger,
    Link,
}

impl VariantSpec {
    fn kit(self) -> ButtonVariant {
        match self {
            Self::Primary => ButtonVariant::Primary,
            Self::Secondary => ButtonVariant::Secondary,
            Self::Outline => ButtonVariant::Outline,
            Self::Ghost => ButtonVariant::Ghost,
            Self::Danger => ButtonVariant::Danger,
            Self::Link => ButtonVariant::Link,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SizeSpec {
    Sm,
    Md,
    Lg,
}

impl SizeSpec {
    fn kit(self) -> Size {
        match self {
            Self::Sm => Size::Sm,
            Self::Md => Size::Md,
            Self::Lg => Size::Lg,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ButtonProps {
    label: String,
    variant: VariantSpec,
    size: SizeSpec,
    disabled: bool,
    loading: bool,
    full_width: bool,
}

impl Default for ButtonProps {
    fn default() -> Self {
        Self {
            label: "Continue".to_owned(),
            variant: VariantSpec::Secondary,
            size: SizeSpec::Md,
            disabled: false,
            loading: false,
            full_width: false,
        }
    }
}

/// The stateless implementation of `nc-ui-kit::button`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ButtonWidget;

impl ButtonWidget {
    fn validate_shape(object: &props::Object) -> Result<(), KitProviderError> {
        props::check_string(ID, object, "label", MAX_LABEL_BYTES, false)?;
        props::check_enum(
            ID,
            object,
            "variant",
            &["primary", "secondary", "outline", "ghost", "danger", "link"],
        )?;
        props::check_enum(ID, object, "size", &["sm", "md", "lg"])?;
        props::check_boolean(ID, object, "disabled")?;
        props::check_boolean(ID, object, "loading")?;
        props::check_boolean(ID, object, "full_width")
    }

    fn emit_click(events: &NativeEventSink, label: &str, origin: &str) {
        events.emit(
            NativeViewEvent::new("button", "clicked").with_payload(serde_json::json!({
                "label": label,
                "origin": origin,
            })),
        );
    }
}

impl KitWidget for ButtonWidget {
    type View = Button;
    type Props = ButtonProps;

    const ID: &'static str = ID;

    fn parse_props(value: &serde_json::Value) -> Result<Self::Props, KitProviderError> {
        props::decode(
            ID,
            value,
            &[
                "label",
                "variant",
                "size",
                "disabled",
                "loading",
                "full_width",
            ],
            Self::validate_shape,
        )
    }

    fn build_child(
        props: &Self::Props,
        events: Arc<NativeEventSink>,
        _window: &mut gpui::Window,
        cx: &mut gpui::App,
    ) -> Result<gpui::Entity<Self::View>, KitProviderError> {
        let event_label = props.label.clone();
        let mut button = Button::new(props.label.clone())
            .variant(props.variant.kit())
            .size(props.size.kit())
            .on_click(move |_window, _cx| {
                Self::emit_click(&events, &event_label, "interaction");
            });
        if props.disabled {
            button = button.disabled();
        }
        if props.loading {
            button = button.loading();
        }
        if props.full_width {
            button = button.full_width();
        }
        Ok(cx.new(|_| button))
    }

    fn invoke_child(
        _view: &gpui::Entity<Self::View>,
        props: &Self::Props,
        events: Arc<NativeEventSink>,
        action: &str,
        payload: &serde_json::Value,
        _window: &mut gpui::Window,
        _cx: &mut gpui::App,
    ) -> Result<(), KitProviderError> {
        if action != "click" {
            return Err(props::unknown_action(ID, action));
        }
        props::ensure_empty_payload(ID, action, payload)?;
        if props.disabled {
            return Err(props::action_unavailable(
                ID,
                action,
                "the button is disabled",
            ));
        }
        if props.loading {
            return Err(props::action_unavailable(
                ID,
                action,
                "the button is busy loading",
            ));
        }
        Self::emit_click(&events, &props.label, "command");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(value: &serde_json::Value) -> Result<ButtonProps, KitProviderError> {
        <ButtonWidget as KitWidget>::parse_props(value)
    }

    #[test]
    fn null_props_produce_the_documented_defaults() {
        let props = parse(&serde_json::Value::Null).expect("null is the empty object shorthand");
        assert_eq!(props.label, "Continue");
        assert_eq!(props.variant, VariantSpec::Secondary);
        assert_eq!(props.size, SizeSpec::Md);
    }

    #[test]
    fn a_non_string_label_is_refused_by_field_name() {
        let error = parse(&json!({"label": 4})).expect_err("a numeric label is invalid");
        assert_eq!(error.code(), "kit_invalid_props");
        assert!(error.to_string().contains("label"));
        assert!(error.to_string().contains("string"));
    }

    #[test]
    fn an_unknown_variant_names_the_field_and_the_choice() {
        let error =
            parse(&json!({"variant": "purple"})).expect_err("an unknown variant is invalid");
        assert!(error.to_string().contains("variant"));
        assert!(error.to_string().contains("purple"));
    }

    #[test]
    fn an_oversized_label_is_refused() {
        let label = "x".repeat(MAX_LABEL_BYTES + 1);
        let error = parse(&json!({"label": label})).expect_err("the label bound is enforced");
        assert!(error.to_string().contains("label"));
        assert!(error.to_string().contains("bound"));
    }

    #[test]
    fn a_non_object_props_value_has_a_typed_refusal() {
        let error = parse(&json!([])).expect_err("arrays are not props objects");
        assert_eq!(error.code(), "kit_props_not_object");
    }
}
