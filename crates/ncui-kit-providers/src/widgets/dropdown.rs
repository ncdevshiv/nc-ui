//! The option-selection dropdown provider.
//!
//! Options are required rather than silently defaulted. A dropdown with invented choices
//! would look interactive while meaning nothing to the application that opened it, and a
//! missing required field is exactly the kind of mistake the props boundary exists to
//! report.

use std::sync::Arc;

use nc_ui_kit::ui::Dropdown;
use ncui_gpui::NativeEventSink;
use ncui_webview::NativeViewEvent;
use serde::Deserialize;

use crate::props;
use crate::provider::KitWidget;
use crate::KitProviderError;

const ID: &str = "nc-ui-kit::dropdown";
const MAX_OPTIONS: usize = 32;
const MAX_OPTION_BYTES: usize = 128;
const MAX_PLACEHOLDER_BYTES: usize = 128;

fn default_placeholder() -> String {
    "Select…".to_owned()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct DropdownProps {
    options: Vec<String>,
    #[serde(default)]
    selected: usize,
    #[serde(default = "default_placeholder")]
    placeholder: String,
}

/// The stateless implementation of `nc-ui-kit::dropdown`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct DropdownWidget;

impl DropdownWidget {
    fn validate_shape(object: &props::Object) -> Result<(), KitProviderError> {
        props::require_field(ID, object, "options")?;
        props::check_string_array(ID, object, "options", 1, MAX_OPTIONS, MAX_OPTION_BYTES)?;
        props::check_integer(ID, object, "selected", 0, MAX_OPTIONS - 1)?;
        props::check_string(ID, object, "placeholder", MAX_PLACEHOLDER_BYTES, false)
    }

    fn validate_semantics(props: &DropdownProps) -> Result<(), KitProviderError> {
        if props.selected >= props.options.len() {
            return Err(props::invalid_props(
                ID,
                "selected",
                format!(
                    "must be less than the {} available options, not {}",
                    props.options.len(),
                    props.selected
                ),
            ));
        }
        Ok(())
    }

    fn emit_selected(events: &NativeEventSink, index: usize, option: &str, origin: &str) {
        events.emit(
            NativeViewEvent::new("dropdown", "selected").with_payload(serde_json::json!({
                "index": index,
                "option": option,
                "origin": origin,
            })),
        );
    }
}

impl KitWidget for DropdownWidget {
    type View = Dropdown;
    type Props = DropdownProps;

    const ID: &'static str = ID;

    fn parse_props(value: &serde_json::Value) -> Result<Self::Props, KitProviderError> {
        let props: Self::Props = props::decode(
            ID,
            value,
            &["options", "selected", "placeholder"],
            Self::validate_shape,
        )?;
        Self::validate_semantics(&props)?;
        Ok(props)
    }

    fn build_child(
        props: &Self::Props,
        events: Arc<NativeEventSink>,
        _window: &mut gpui::Window,
        cx: &mut gpui::App,
    ) -> Result<gpui::Entity<Self::View>, KitProviderError> {
        let options_for_events = props.options.clone();
        let dropdown = Dropdown::new(props.options.clone())
            .placeholder(props.placeholder.clone())
            .selected(props.selected)
            .on_select(move |index, _window, _cx| {
                let option = options_for_events.get(index).cloned().unwrap_or_default();
                Self::emit_selected(&events, index, &option, "interaction");
            })
            .finish(cx);
        Ok(dropdown)
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
            "open" | "close" => {
                props::ensure_empty_payload(ID, action, payload)?;
                let open = action == "open";
                view.update(cx, |dropdown, cx| {
                    dropdown.open = open;
                    cx.notify();
                });
                Ok(())
            }
            "focus" => {
                props::ensure_empty_payload(ID, action, payload)?;
                let focus = view.read(cx).focus_handle().clone();
                window.focus(&focus);
                Ok(())
            }
            "select" => {
                let object = props::payload_object(ID, action, payload, &["index"])?;
                let index = props::payload_usize(ID, action, object, "index")?;
                if index >= props.options.len() {
                    return Err(props::invalid_payload(
                        ID,
                        action,
                        "index",
                        format!("must be less than the {} available options, not {index}", props.options.len()),
                    ));
                }
                let changed = view.read(cx).selected != index;
                view.update(cx, |dropdown, cx| {
                    dropdown.selected = index;
                    dropdown.open = false;
                    cx.notify();
                });
                if changed {
                    let option = props.options.get(index).map_or("", String::as_str);
                    Self::emit_selected(&events, index, option, "command");
                }
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

    fn parse(value: serde_json::Value) -> Result<DropdownProps, KitProviderError> {
        <DropdownWidget as KitWidget>::parse_props(&value)
    }

    #[test]
    fn valid_options_decode_with_defaults() {
        let props = parse(json!({"options": ["Alpha", "Beta"]})).expect("valid props decode");
        assert_eq!(props.options, ["Alpha", "Beta"]);
        assert_eq!(props.selected, 0);
        assert_eq!(props.placeholder, "Select…");
    }

    #[test]
    fn missing_options_are_refused_by_field_name() {
        let error = parse(serde_json::Value::Null).expect_err("options are required");
        assert_eq!(error.code(), "kit_invalid_props");
        assert!(error.to_string().contains("options"));
        assert!(error.to_string().contains("required"));
    }

    #[test]
    fn a_bad_option_names_its_array_position() {
        let error = parse(json!({"options": ["Alpha", 2]})).expect_err("non-string options are invalid");
        assert!(error.to_string().contains("options[1]"));
    }

    #[test]
    fn a_selected_index_outside_the_options_is_refused() {
        let error = parse(json!({"options": ["Alpha"], "selected": 1}))
            .expect_err("the selected index must exist");
        assert!(error.to_string().contains("selected"));
        assert!(error.to_string().contains("available options"));
    }

    #[test]
    fn too_many_options_are_refused() {
        let options: Vec<String> = (0..=MAX_OPTIONS).map(|index| format!("Option {index}")).collect();
        let error = parse(json!({"options": options})).expect_err("the options bound is enforced");
        assert!(error.to_string().contains("options"));
    }
}
