//! The fixed-settings tab-strip provider.
//!
//! The kit's `Tabs::new` accepts static `(id, label)` pairs, so a fully dynamic tab
//! catalogue cannot be represented without leaking or copying the kit. Rather than hide
//! that limitation, this provider is deliberately specific: it is a settings surface
//! with three stable tabs whose panel copy is configurable. The fixed identifiers are
//! part of the schema, which keeps memory bounded and the contract honest.

use std::sync::Arc;

use gpui::{
    App, AppContext as _, Context, Entity, IntoElement, ParentElement, Render, Styled, Window, div,
    px,
};
use nc_ui_kit::theme::Theme;
use nc_ui_kit::ui::Tabs;
use ncui_gpui::NativeEventSink;
use ncui_webview::NativeViewEvent;
use serde::Deserialize;
use serde_json::Value;

use crate::KitProviderError;
use crate::props;
use crate::provider::KitWidget;

const ID: &str = "nc-ui-kit::settings-tabs";
const MAX_PANEL_BYTES: usize = 2048;
const TAB_CHOICES: [&str; 3] = ["general", "appearance", "advanced"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TabSpec {
    General,
    Appearance,
    Advanced,
}

impl TabSpec {
    fn as_str(self) -> &'static str {
        match self {
            Self::General => "general",
            Self::Appearance => "appearance",
            Self::Advanced => "advanced",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PanelCopy {
    general: String,
    appearance: String,
    advanced: String,
}

impl Default for PanelCopy {
    fn default() -> Self {
        Self {
            general: "Application-wide preferences and operational defaults.".to_owned(),
            appearance: "Theme, density and visual presentation.".to_owned(),
            advanced: "Low-level options for experienced users.".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SettingsTabsProps {
    active: TabSpec,
    panels: PanelCopy,
}

impl Default for SettingsTabsProps {
    fn default() -> Self {
        Self {
            active: TabSpec::General,
            panels: PanelCopy::default(),
        }
    }
}

/// The provider surface pairing the kit tab strip with the active panel's copy.
pub struct SettingsTabsPanel {
    tabs: Entity<Tabs>,
    active: String,
    panels: PanelCopy,
}

impl SettingsTabsPanel {
    fn copy_for_active(&self) -> &str {
        match self.active.as_str() {
            "appearance" => &self.panels.appearance,
            "advanced" => &self.panels.advanced,
            _ => &self.panels.general,
        }
    }
}

impl Render for SettingsTabsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = Theme::get(cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg)
            .child(self.tabs.clone())
            .child(
                div().flex_1().min_h_0().p(px(20.)).child(
                    div()
                        .rounded(px(10.))
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.panel)
                        .p(px(16.))
                        .text_size(px(13.))
                        .text_color(theme.muted)
                        .child(self.copy_for_active().to_owned()),
                ),
            )
    }
}

/// The stateless implementation of `nc-ui-kit::settings-tabs`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SettingsTabsWidget;

impl SettingsTabsWidget {
    fn validate_shape(object: &props::Object) -> Result<(), KitProviderError> {
        props::check_enum(ID, object, "active", &TAB_CHOICES)?;
        props::check_object(ID, object, "panels")?;
        let Some(Value::Object(panels)) = object.get("panels") else {
            return Ok(());
        };
        for (key, value) in panels {
            let field = format!("panels.{key}");
            if !TAB_CHOICES.contains(&key.as_str()) {
                return Err(props::invalid_props(
                    ID,
                    field,
                    format!("is unknown; expected one of {}", TAB_CHOICES.join(", ")),
                ));
            }
            let Value::String(text) = value else {
                return Err(props::invalid_props(
                    ID,
                    field,
                    format!("must be a string, not {}", kind_name(value)),
                ));
            };
            props::validate_string_text(ID, &field, text, MAX_PANEL_BYTES, false)?;
        }
        Ok(())
    }

    fn emit_selected(events: &NativeEventSink, tab: &str, origin: &str) {
        events.emit(
            NativeViewEvent::new("settings-tabs", "selected").with_payload(serde_json::json!({
                "tab": tab,
                "origin": origin,
            })),
        );
    }
}

fn kind_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

impl KitWidget for SettingsTabsWidget {
    type View = SettingsTabsPanel;
    type Props = SettingsTabsProps;

    const ID: &'static str = ID;

    fn parse_props(value: &Value) -> Result<Self::Props, KitProviderError> {
        props::decode(ID, value, &["active", "panels"], Self::validate_shape)
    }

    fn build_child(
        props: &Self::Props,
        events: Arc<NativeEventSink>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Result<Entity<Self::View>, KitProviderError> {
        let tabs = cx.new(|_| {
            Tabs::new(vec![
                ("general", "General"),
                ("appearance", "Appearance"),
                ("advanced", "Advanced"),
            ])
            .active(props.active.as_str())
            .on_change(move |tab, _window, _cx| {
                Self::emit_selected(&events, tab.as_ref(), "interaction");
            })
        });
        let active = props.active.as_str().to_owned();
        let panels = props.panels.clone();
        let panel = cx.new(|cx| {
            cx.observe(&tabs, |panel: &mut SettingsTabsPanel, tabs, cx| {
                panel.active = tabs.read(cx).active.to_string();
                cx.notify();
            })
            .detach();
            SettingsTabsPanel {
                tabs: tabs.clone(),
                active,
                panels,
            }
        });
        Ok(panel)
    }

    fn invoke_child(
        view: &Entity<Self::View>,
        _props: &Self::Props,
        events: Arc<NativeEventSink>,
        action: &str,
        payload: &Value,
        _window: &mut Window,
        cx: &mut App,
    ) -> Result<(), KitProviderError> {
        if action != "select" {
            return Err(props::unknown_action(ID, action));
        }
        let object = props::payload_object(ID, action, payload, &["tab"])?;
        let tab = props::payload_string(ID, action, object, "tab")?;
        if !TAB_CHOICES.contains(&tab) {
            return Err(props::invalid_payload(
                ID,
                action,
                "tab",
                format!("must be one of {}, not `{tab}`", TAB_CHOICES.join(", ")),
            ));
        }
        let changed = view.read(cx).tabs.read(cx).active.as_ref() != tab;
        if changed {
            let owned = tab.to_owned();
            view.update(cx, |panel, cx| {
                panel.active.clone_from(&owned);
                panel.tabs.update(cx, |tabs, cx| {
                    tabs.active = gpui::SharedString::new(owned);
                    cx.notify();
                });
                cx.notify();
            });
            Self::emit_selected(&events, tab, "command");
        }
        Ok(())
    }

    fn padding() -> f32 {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(value: &Value) -> Result<SettingsTabsProps, KitProviderError> {
        <SettingsTabsWidget as KitWidget>::parse_props(value)
    }

    #[test]
    fn null_props_produce_the_documented_defaults() {
        let props = parse(&Value::Null).expect("null is the empty object shorthand");
        assert_eq!(props.active, TabSpec::General);
        assert!(props.panels.general.contains("Application-wide"));
    }

    #[test]
    fn an_unknown_active_tab_names_the_active_field() {
        let error = parse(&json!({"active": "plugins"})).expect_err("unknown tabs are invalid");
        assert!(error.to_string().contains("active"));
        assert!(error.to_string().contains("plugins"));
    }

    #[test]
    fn a_bad_panel_names_its_nested_field() {
        let error =
            parse(&json!({"panels": {"appearance": 2}})).expect_err("panel copy must be text");
        assert!(error.to_string().contains("panels.appearance"));
    }

    #[test]
    fn an_unknown_panel_key_names_the_nested_field() {
        let error =
            parse(&json!({"panels": {"plugins": "x"}})).expect_err("unknown panels are invalid");
        assert!(error.to_string().contains("panels.plugins"));
    }
}
