//! The nc-ui-kit provider bridge: real kit widgets hosted as NC-UI native views.
//!
//! ADR-020 makes provider registration a composition-time trust decision. This crate is
//! the layer-4 bridge that decision names: it adapts selected `nc-ui-kit` widgets into
//! `ncui-gpui`'s [`ncui_gpui::NativeViewProvider`] contract, validates the bounded JSON
//! props a window carries, and translates widget interactions into bounded
//! [`ncui_webview::NativeViewEvent`]s. It does not decide who may open a window; the
//! runtime has already answered that question before a provider is invoked.
//!
//! # Registered providers
//!
//! [`register_all`] registers six providers. Their props are deliberately small,
//! strictly validated JSON objects. Top-level `null` is treated as an empty object where
//! every field has a default; otherwise a field-level `null` is valid only where the
//! schema explicitly says so (the dialog's nullable `description`). Unknown fields are
//! refused because a mistyped field should not silently become a different control.
//!
//! * `nc-ui-kit::button` — `label`, `variant`, `size`, `disabled`, `loading` and
//!   `full_width`. Emits `button`/`clicked`; accepts the `click` command.
//! * `nc-ui-kit::input` — `value`, `placeholder`, `numeric`, `disabled` and `invalid`. (the annotation is the scanner's: the word is the input hint-text prop, a UI domain term — ncui-allow-placeholder: domain vocabulary)
//!   Emits `input`/`changed`; accepts `clear` and `focus`.
//! * `nc-ui-kit::dropdown` — required `options`, plus `selected` and `placeholder`. (the annotation is the scanner's: the word is the input hint-text prop, a UI domain term — ncui-allow-placeholder: domain vocabulary)
//!   Emits `dropdown`/`selected`; accepts `open`, `close`, `focus` and `select`.
//! * `nc-ui-kit::checkbox` — `checked`, `label` and `disabled`. Emits
//!   `checkbox`/`toggled`; accepts `toggle`, `set_checked` and `focus`.
//! * `nc-ui-kit::dialog` — `title`, `description`, `trigger_label`, `open` and `width`.
//!   Emits `dialog`/`opened` and `dialog`/`closed`; accepts `open` and `close`.
//! * `nc-ui-kit::settings-tabs` — a fixed three-tab settings surface (`general`,
//!   `appearance`, `advanced`) whose panel copy is configurable. Emits
//!   `settings-tabs`/`selected`; accepts `select`.
//!
//! Event payloads carry an `origin` field: `interaction` for a user gesture inside the
//! widget and `command` for a state transition caused by an accepted command. This is
//! deliberate rather than decorative — a host automation loop can otherwise mistake its
//! own command for a user decision.
//!
//! Props replacement rebuilds the widget child inside the same root view. Kit widgets do
//! not expose a uniform partial-update contract, so a complete child rebuild is the only
//! honest implementation; the window root and provider instance survive unchanged.
//!
//! # Theme, assets and scale
//!
//! [`apply_theme`] maps the runtime's light/dark answer onto the kit's global semantic
//! theme, preferring the JSON theme files in the resolved asset layout. Provider roots
//! also apply the window's current platform appearance when they are created and observe
//! later appearance changes, so an open native view follows the same theme fact the host
//! reports through `window_theme`. The kit exposes no independent DPI scale: its
//! dimensions are gpui logical pixels, and Windows scaling belongs to gpui and the
//! executable manifest. Asset resolution is implemented here,
//! but see [`assets`]: gpui 0.2.2 allows an asset source only when the `Application` is
//! constructed, and `ncui-gpui` currently constructs that application internally without
//! one. SVG glyphs can therefore be unavailable even though interaction and events are
//! real; that integration boundary is documented rather than hidden.

#![deny(unsafe_code)]
#![deny(missing_docs)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "test assertions must abort loudly on failure"
    )
)]

pub mod assets;
pub mod error;
mod props;
mod provider;
mod theme;
mod widgets;

pub use assets::{AssetResolution, AssetResolutionSource, KitAssets, asset_source, resolve};
pub use error::KitProviderError;
pub use provider::{KitProvider, KitWidget};
pub use theme::apply_theme;

/// The stable provider identifiers [`register_all`] installs.
///
/// The identifiers are part of the agent-facing surface. They are concentrated in one
/// constant so a review sees the whole vocabulary at once, and so tests can compare the
/// registry with the documentation rather than trusting either side alone.
pub const PROVIDER_IDS: [&str; 6] = [
    "nc-ui-kit::button",
    "nc-ui-kit::input",
    "nc-ui-kit::dropdown",
    "nc-ui-kit::checkbox",
    "nc-ui-kit::dialog",
    "nc-ui-kit::settings-tabs",
];

/// Registers every kit provider into the process-wide `ncui-gpui` registry.
///
/// Registration is idempotent in practice because the registry's documented rule is
/// that the last registration wins. The function returns how many kit provider ids it
/// installed; it does not empty or otherwise disturb providers registered by another
/// composition component.
pub fn register_all() -> usize {
    let registry = ncui_gpui::providers();
    registry.register(std::sync::Arc::new(
        KitProvider::<widgets::ButtonWidget>::new(),
    ));
    registry.register(std::sync::Arc::new(
        KitProvider::<widgets::InputWidget>::new(),
    ));
    registry.register(std::sync::Arc::new(
        KitProvider::<widgets::DropdownWidget>::new(),
    ));
    registry.register(std::sync::Arc::new(
        KitProvider::<widgets::CheckboxWidget>::new(),
    ));
    registry.register(std::sync::Arc::new(
        KitProvider::<widgets::DialogWidget>::new(),
    ));
    registry.register(std::sync::Arc::new(KitProvider::<
        widgets::SettingsTabsWidget,
    >::new()));
    PROVIDER_IDS.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registered_id_passes_the_runtime_provider_rules() {
        assert_eq!(register_all(), PROVIDER_IDS.len());
        for id in PROVIDER_IDS {
            assert!(
                ncui_gpui::providers().get(id).is_some(),
                "{id} was not registered"
            );
            ncui_webview::WindowRequest::native(id)
                .validate()
                .unwrap_or_else(|error| panic!("{id} failed runtime validation: {error}"));
        }
    }

    #[test]
    fn the_documented_ids_are_unique() {
        let mut ids = PROVIDER_IDS.to_vec();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len());
    }
}
