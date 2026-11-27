//! The concrete kit widgets exposed through the provider registry.
//!
//! Each module owns one provider's schema, construction and command vocabulary. Keeping
//! them separate is not tidiness for its own sake: props validation is a trust-boundary
//! decision, and a boundary is reviewable when exactly one file answers for one widget.

mod button;
mod checkbox;
mod dialog;
mod dropdown;
mod input;
mod settings_tabs;

pub(crate) use button::ButtonWidget;
pub(crate) use checkbox::CheckboxWidget;
pub(crate) use dialog::DialogWidget;
pub(crate) use dropdown::DropdownWidget;
pub(crate) use input::InputWidget;
pub(crate) use settings_tabs::SettingsTabsWidget;
