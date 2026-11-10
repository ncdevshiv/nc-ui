"""Tests the platform's theme through the window surface."""

from pathlib import Path

p = Path('crates/ncui-runtime/src/window.rs')
s = p.read_text(encoding='utf-8')

# The fake backend can report a theme.
old = """        fn window_state(&self) -> Option<ncui_webview::WindowState> {
            Some(
                *self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            )
        }"""
new = """        fn window_theme(&self) -> Option<ncui_webview::Theme> {
            *self
                .theme
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }

        fn window_state(&self) -> Option<ncui_webview::WindowState> {
            Some(
                *self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            )
        }"""
assert old in s, 'fake backend state method not found'
s = s.replace(old, new, 1)

# The fake backend's field.
old = """        fn window_state(&self) -> Option<ncui_webview::WindowState> {
            Some(
                *self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            )
        }
        fn set_window_state("""
new = """        fn window_state(&self) -> Option<ncui_webview::WindowState> {
            Some(
                *self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            )
        }
        fn set_window_state("""
assert old in s, 'state method anchor not found'

p.write_text(s, encoding='utf-8')
print('fake backend given a theme')
