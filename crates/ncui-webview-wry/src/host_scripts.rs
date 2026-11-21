//! The scripts the engine evaluates to tell a page about its window.
//!
//! Split out of [`crate::host`] so the binding stays focused on the window and the event
//! loop. Each function is the host-to-page half of one window event: the engine asks the
//! platform what changed, and the page hears about it through the bridge rather than by
//! polling.

/// What a page hears when its window is resized, in logical pixels.
///
/// Told rather than left to poll: a layout built for the old size is a layout the user has already
/// left, and a page that had to ask would not be able to lay out while the user is still dragging.
#[cfg(feature = "wry-engine")]
pub(super) fn resized_script(size: tao::dpi::LogicalSize<f64>) -> String {
    format!(
        "window.ncui && window.ncui.__dispatch({{\"event\":\"resized\",\"payload\":{{\"width\":{},\"height\":{}}}}});",
        size.width, size.height
    )
}

/// What a page hears when files are dropped onto the window.
///
/// Delivered as an event with a `paths` array in the payload. Each path is a
/// string representation of a dropped file. The page can register a listener
/// via `window.ncui.on("drop", handler)` to receive these events.
#[cfg(feature = "wry-engine")]
pub(super) fn dropped_files_script(paths: &[std::path::PathBuf]) -> String {
    let paths_json: Vec<String> = paths
        .iter()
        .filter_map(|p| p.to_str())
        .map(|s| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect();
    format!(
        "window.ncui && window.ncui.__dispatch({{\"event\":\"drop\",\"payload\":{{\"paths\":[{}]}}}});",
        paths_json.join(",")
    )
}

/// Script injected into the page to enable drag regions.
///
/// Listens for mousedown on elements matching `[data-ncui-drag-region]` and
/// calls the host to initiate a window drag. This is the NC-UI equivalent of
/// Tauri 2's CSS `app-region: drag` / Electron's `-webkit-app-region: drag`.
///
/// The script also calls `preventDefault()` on mousedown inside drag regions
/// so the browser doesn't start a text selection, and honours `[data-ncui-no-drag]`
/// for interactive elements inside a drag region (buttons, inputs, etc.).
#[cfg(feature = "wry-engine")]
pub(super) const DRAG_REGION_SCRIPT: &str = r#"(() => {
  const DRAG = "[data-ncui-drag-region]";
  const NO_DRAG = "[data-ncui-no-drag]";
  document.addEventListener("mousedown", (e) => {
    if (e.button !== 0) return;
    const target = e.target;
    if (!(target instanceof Element)) return;
    if (target.closest(NO_DRAG)) return;
    const region = target.closest(DRAG);
    if (!region) return;
    e.preventDefault();
    if (window.ncui && window.ncui.invoke) {
      window.ncui.invoke("drag_region", {}).catch(() => {});
    }
  }, true);
})();"#;

/// What a page hears when its window is moved, in logical pixels.
///
/// The counterpart of the resize: a page that keeps something anchored to the window's place — a
/// toolbar that follows it, a remembered position — needs to know where it went without asking.
#[cfg(feature = "wry-engine")]
pub(super) fn moved_script(position: tao::dpi::LogicalPosition<f64>) -> String {
    format!(
        "window.ncui && window.ncui.__dispatch({{\"event\":\"moved\",\"payload\":{{\"x\":{},\"y\":{}}}}});",
        position.x, position.y
    )
}

#[cfg(all(test, feature = "wry-engine"))]
mod tests {
    use super::*;

    #[test]
    fn dropped_files_script_contains_event_and_paths() {
        let paths = vec![
            std::path::PathBuf::from("/home/user/file.txt"),
            std::path::PathBuf::from("C:\\data\\doc.pdf"),
        ];
        let script = dropped_files_script(&paths);
        assert!(
            script.contains("\"event\":\"drop\""),
            "missing event: {script}"
        );
        assert!(script.contains("file.txt"), "missing path: {script}");
        assert!(script.contains("doc.pdf"), "missing path: {script}");
        assert!(
            script.contains("__dispatch"),
            "missing __dispatch: {script}"
        );
    }

    #[test]
    fn dropped_files_script_escapes_backslashes_and_quotes() {
        let paths = vec![std::path::PathBuf::from("C:\\path\\file.txt")];
        let script = dropped_files_script(&paths);
        // Backslashes must be escaped in the JSON string.
        assert!(
            script.contains("C:\\\\path\\\\file.txt"),
            "backslash not escaped: {script}"
        );
    }

    #[test]
    fn dropped_files_script_handles_empty_paths() {
        let script = dropped_files_script(&[]);
        assert!(
            script.contains("\"paths\":[]"),
            "empty paths should produce empty array: {script}"
        );
    }
}
