//! Engine-neutral native menu requests and events.

use serde::{Deserialize, Serialize};

/// A native menu bar to attach to one window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MenuRequest {
    /// The top-level menus, in display order.
    pub menus: Vec<MenuSubmenu>,
}

/// A top-level menu or nested submenu.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MenuSubmenu {
    /// The label shown to the user.
    pub label: String,
    /// Whether the submenu is enabled.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Its entries, in display order.
    pub items: Vec<MenuEntry>,
}

/// One entry in a native menu.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MenuEntry {
    /// A command the page hears about when the user selects it.
    Item {
        /// The label shown to the user.
        label: String,
        /// The manifest-declared command delivered to the page.
        command: String,
        /// Whether the item is enabled.
        #[serde(default = "default_enabled")]
        enabled: bool,
    },
    /// A nested menu.
    Submenu(MenuSubmenu),
    /// A visual separator.
    Separator,
}

/// One native menu selection delivered to the application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuEvent {
    /// The command declared by the selected item.
    pub command: String,
}

const fn default_enabled() -> bool {
    true
}

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]
#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;
    use crate::{PageMessage, WebViewBackend};

    #[test]
    fn recursive_menu_requests_round_trip_with_enabled_defaults() {
        let request: MenuRequest = serde_json::from_str(
            r#"{"menus":[{"label":"File","items":[
                {"type":"item","label":"Open","command":"open"},
                {"type":"separator"},
                {"type":"submenu","label":"More","enabled":false,"items":[
                    {"type":"item","label":"Close","command":"close","enabled":false}
                ]}
            ]}]}"#,
        )
        .expect("a recursive menu request");

        assert!(request.menus[0].enabled);
        let MenuEntry::Item { enabled, .. } = &request.menus[0].items[0] else {
            panic!("the first entry is an item");
        };
        assert!(*enabled);
        assert_eq!(request.menus[0].items[1], MenuEntry::Separator);
        let MenuEntry::Submenu(submenu) = &request.menus[0].items[2] else {
            panic!("the third entry is a submenu");
        };
        assert!(!submenu.enabled);
        let MenuEntry::Item { enabled, .. } = &submenu.items[0] else {
            panic!("the submenu contains an item");
        };
        assert!(!*enabled);

        let encoded = serde_json::to_value(&request).expect("a menu request serialises");
        assert_eq!(encoded["menus"][0]["items"][0]["type"], "item");
        assert_eq!(encoded["menus"][0]["items"][1]["type"], "separator");
        assert_eq!(encoded["menus"][0]["items"][2]["type"], "submenu");
        assert_eq!(
            serde_json::from_value::<MenuRequest>(encoded).expect("the encoded request parses"),
            request
        );
    }

    #[derive(Debug)]
    struct RefusingBackend;

    impl WebViewBackend for RefusingBackend {
        fn as_any(&self) -> &dyn core::any::Any {
            self
        }

        fn last_error(&self) -> Option<String> {
            None
        }

        fn is_alive(&self) -> bool {
            true
        }

        fn poll_page_message(&self) -> Option<PageMessage> {
            None
        }

        fn evaluate_script(&self, _script: &str) -> bool {
            false
        }
    }

    #[test]
    fn a_backend_without_native_menus_refuses_instead_of_pretending() {
        let backend = RefusingBackend;
        assert!(!backend.set_menu(Some(MenuRequest { menus: Vec::new() })));
        assert!(!backend.set_menu(None));
        assert_eq!(backend.poll_menu_event(), None);
    }

    #[derive(Debug, Default)]
    struct RecordingBackend {
        menu: Mutex<Option<MenuRequest>>,
        events: Mutex<VecDeque<MenuEvent>>,
    }

    impl WebViewBackend for RecordingBackend {
        fn as_any(&self) -> &dyn core::any::Any {
            self
        }

        fn last_error(&self) -> Option<String> {
            None
        }

        fn is_alive(&self) -> bool {
            true
        }

        fn poll_page_message(&self) -> Option<PageMessage> {
            None
        }

        fn evaluate_script(&self, _script: &str) -> bool {
            true
        }

        fn set_menu(&self, menu: Option<MenuRequest>) -> bool {
            *self.menu.lock().expect("the menu recorder is usable") = menu;
            true
        }

        fn poll_menu_event(&self) -> Option<MenuEvent> {
            self.events
                .lock()
                .expect("the event recorder is usable")
                .pop_front()
        }
    }

    #[test]
    fn a_boxed_backend_forwards_menu_requests_and_selections() {
        let backend: Box<dyn WebViewBackend> = Box::new(RecordingBackend::default());
        let request = MenuRequest {
            menus: vec![MenuSubmenu {
                label: "File".to_owned(),
                enabled: true,
                items: vec![MenuEntry::Item {
                    label: "Open".to_owned(),
                    command: "open".to_owned(),
                    enabled: true,
                }],
            }],
        };
        assert!(backend.set_menu(Some(request.clone())));
        let recording = backend
            .as_any()
            .downcast_ref::<RecordingBackend>()
            .expect("the boxed backend retains its concrete type");
        assert_eq!(
            recording
                .menu
                .lock()
                .expect("the recorder is usable")
                .as_ref(),
            Some(&request)
        );

        recording
            .events
            .lock()
            .expect("the recorder is usable")
            .push_back(MenuEvent {
                command: "open".to_owned(),
            });
        assert_eq!(
            backend.poll_menu_event(),
            Some(MenuEvent {
                command: "open".to_owned()
            })
        );
        assert_eq!(backend.poll_menu_event(), None);
    }
}
