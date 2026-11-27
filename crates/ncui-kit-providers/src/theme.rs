//! Runtime-to-kit theme handoff.
//!
//! NC-UI deliberately reduces platform appearance to a binary light/dark answer, while
//! the kit's semantic theme carries many more colours. The bridge therefore treats the
//! runtime answer as a mode selector and lets the kit's own theme files supply the full
//! palette. Inventing a second colour-mapping layer here would make the bridge a third
//! theme system rather than a handoff.

use nc_ui_kit::theme::{Theme as KitTheme, ThemeFile, ThemeMode};

use crate::assets::resolve;

/// Applies the runtime's platform theme to the kit's global theme.
///
/// The JSON files `nc-light.json` and `nc-dark.json` are preferred when the resolved
/// asset layout provides them. Missing or invalid files fall back to the kit's built-in
/// palettes, matching the playground's behaviour: a theme asset is presentation polish,
/// not a reason to make every native widget window fail to open. The theme actually
/// installed is returned so composition code can log or inspect which palette won.
///
/// DPI is intentionally not part of this function. The kit exposes no independent scale
/// factor — its dimensions are gpui logical pixels — and gpui's Windows manifest supplies
/// DPI awareness at process level. Returning a made-up scale here would be less honest
/// than saying the handoff does not exist.
pub fn apply_theme(theme: ncui_webview::Theme, cx: &mut gpui::App) -> KitTheme {
    let (mode, file) = match theme {
        ncui_webview::Theme::Light => (ThemeMode::Light, "nc-light.json"),
        ncui_webview::Theme::Dark => (ThemeMode::Dark, "nc-dark.json"),
    };
    let path = resolve().themes_dir().join(file);
    let kit_theme = ThemeFile::load(&path).unwrap_or_else(|_| KitTheme::from_mode(mode));
    kit_theme.set_global(cx);
    kit_theme
}

/// Applies the theme implied by one gpui window's current platform appearance.
///
/// This is the same light/dark fact `ncui-gpui` reports through `window_theme`; reading it
/// inside the bridge is what lets an already-open native view follow an operating-system
/// appearance change without requiring the runtime to know about kit-specific globals.
pub(crate) fn apply_window_theme(window: &gpui::Window, cx: &mut gpui::App) -> KitTheme {
    let theme = match window.appearance() {
        gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark => {
            ncui_webview::Theme::Dark
        }
        gpui::WindowAppearance::Light | gpui::WindowAppearance::VibrantLight => {
            ncui_webview::Theme::Light
        }
    };
    apply_theme(theme, cx)
}
