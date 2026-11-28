//! OS file drag-and-drop into windows (`native.os_file_drop`).
//!
//! Editors live on drag-in: media files onto the media pool or straight onto the
//! timeline. The security shape is the file dialog's, not the filesystem
//! capability's: a drop is the user physically deputizing the application for
//! those specific paths, the same way picking a file in a dialog is. So the
//! event discloses real paths without a `filesystem.read` grant — while reading
//! their contents stays behind whatever capability the read itself requires.
//!
//! What this module owns is the *decision*, kept pure and headless-testable:
//! which paths a drop may disclose, in what form, at what size. The host owns
//! the OLE/tao integration and the page delivery; it must not own policy.

use std::path::{Component, Path, PathBuf};

/// The most paths one drop event may carry.
///
/// A drop is user-driven, so this is not a security bound — it is a memory
/// bound. Explorer does not cap selection size, and a ten-thousand-file drop
/// would otherwise copy ten thousand paths across every layer. 1024 is far
/// past any honest editing gesture.
pub const MAX_DROP_PATHS: usize = 1024;

/// The longest single path the event carries, in bytes.
///
/// Windows paths cap at 32 767 UTF-16 code units in the long-path form; the
/// bound is bytes rather than characters because the event crosses a JSON
/// boundary where bytes are what cost. Paths longer than this are dropped
/// rather than truncated: a truncated path is a wrong path, and a wrong path
/// is worse than a missing one.
pub const MAX_PATH_BYTES: usize = 4096;

/// A file-drop gesture as the page receives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDrop {
    /// The dropped paths, normalized and filtered per [`prepare_drop`].
    pub paths: Vec<PathBuf>,
    /// Where the drop landed, relative to the webview's top-left corner.
    pub position: (i32, i32),
}

/// The outcome of preparing a drop for disclosure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropDecision {
    /// The drop may be disclosed to the page as given.
    Disclose(FileDrop),
    /// Nothing disclosable survived filtering: the page is not told.
    ///
    /// Silence is correct here rather than an error event — a drop of only
    /// unnormalizable paths is not an attack worth reporting, it is noise the
    /// shell produced, and an error event would teach pages to expect drops
    /// that never carry anything.
    Suppress,
}

/// Prepares raw dropped paths for disclosure to a page.
///
/// Three rules, each load-bearing:
///
/// * **Count and length bounds** — the event crosses trust boundaries as JSON;
///   its size is not the drag-source's to choose.
/// * **Normalization** — `.` and non-rooted `..` components are resolved away
///   so the page compares paths structurally rather than textually. A `..`
///   that would escape the root is dropped: it does not name a file the user
///   can have meant, and keeping it would make containment checks downstream
///   lie upward.
/// * **No existence checks** — whether the file exists is the reader's
///   problem, not the event's. A drop of a not-yet-flushed path is still the
///   user's gesture.
#[must_use]
pub fn prepare_drop(paths: &[PathBuf], position: (i32, i32)) -> DropDecision {
    let mut out = Vec::with_capacity(paths.len().min(MAX_DROP_PATHS));
    for path in paths.iter().take(MAX_DROP_PATHS) {
        if path.as_os_str().len() > MAX_PATH_BYTES {
            continue;
        }
        if let Some(normalized) = normalize(path) {
            out.push(normalized);
        }
    }
    if out.is_empty() {
        return DropDecision::Suppress;
    }
    DropDecision::Disclose(FileDrop {
        paths: out,
        position,
    })
}

/// Resolves `.` and interior `..` components structurally.
///
/// This is textual normalization, deliberately not `canonicalize`: canonicalize
/// hits the filesystem (slow for a drag stream), resolves symlinks (a disclosure
/// difference the user did not ask for), and fails for files that do not exist
/// yet. The gesture's paths are normalized for comparison, not resolved for
/// truth.
fn normalize(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    // A `..` with nothing to pop escapes the root; the path
                    // does not name anything real.
                    return None;
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// The JavaScript dispatch for a drop event.
///
/// Kept here rather than in the host so the event name and payload shape have
/// exactly one definition, unit-tested against the same string the page
/// contract documents. The event is a `CustomEvent` named `ncui:file-drop`
/// whose detail is `{ paths: string[], position: [x, y] }` — DOM-positive, so a
/// page listens with ordinary `addEventListener` and no bridge-specific code.
#[must_use]
pub fn dispatch_script(drop: &FileDrop) -> String {
    let paths = drop
        .paths
        .iter()
        .map(|p| {
            let text = p
                .to_string_lossy()
                .replace('\\', "\\\\")
                .replace('\'', "\\'");
            format!("'{text}'")
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "window.dispatchEvent(new CustomEvent('ncui:file-drop',{{detail:{{paths:[{paths}],position:[{},{}]}}}}))",
        drop.position.0, drop.position.1
    )
}

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]
#[cfg(test)]
mod tests {
    use super::*;

    fn paths(items: &[&str]) -> Vec<PathBuf> {
        items.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn a_plain_drop_is_disclosed_with_position() {
        let decision = prepare_drop(&paths(&["C:\\media\\a.mp4", "D:\\b.mp4"]), (40, 90));
        let DropDecision::Disclose(drop) = decision else {
            panic!("plain drops disclose");
        };
        assert_eq!(drop.paths.len(), 2);
        assert_eq!(drop.position, (40, 90));
    }

    #[test]
    fn dot_components_are_normalized_away() {
        let decision = prepare_drop(&paths(&["C:\\media\\.\\a.mp4"]), (0, 0));
        let DropDecision::Disclose(drop) = decision else {
            panic!("disclose");
        };
        assert_eq!(drop.paths[0], PathBuf::from("C:\\media\\a.mp4"));
    }

    #[test]
    fn interior_parent_components_resolve() {
        let decision = prepare_drop(&paths(&["C:\\media\\x\\..\\a.mp4"]), (0, 0));
        let DropDecision::Disclose(drop) = decision else {
            panic!("disclose");
        };
        assert_eq!(drop.paths[0], PathBuf::from("C:\\media\\a.mp4"));
    }

    #[test]
    fn root_escaping_parents_are_dropped() {
        // A relative path whose first component is `..` escapes upward; it
        // cannot name the file the user meant.
        let decision = prepare_drop(&paths(&["..\\escape.mp4"]), (0, 0));
        assert_eq!(decision, DropDecision::Suppress);
    }

    #[test]
    fn count_is_bounded() {
        let many: Vec<PathBuf> = (0..2000)
            .map(|i| PathBuf::from(format!("C:\\f{i}")))
            .collect();
        let DropDecision::Disclose(drop) = prepare_drop(&many, (0, 0)) else {
            panic!("disclose");
        };
        assert_eq!(drop.paths.len(), MAX_DROP_PATHS);
    }

    #[test]
    fn overlong_paths_are_dropped_not_truncated() {
        let long = format!("C:\\{}", "a".repeat(MAX_PATH_BYTES));
        let decision = prepare_drop(&paths(&[&long]), (0, 0));
        assert_eq!(decision, DropDecision::Suppress);
    }

    #[test]
    fn the_dispatch_script_is_a_custom_event_with_escaped_paths() {
        let drop = FileDrop {
            paths: vec![PathBuf::from("C:\\media\\it's.mp4")],
            position: (12, 34),
        };
        let script = dispatch_script(&drop);
        assert!(script.contains("ncui:file-drop"));
        assert!(script.contains("position:[12,34]"));
        // Backslashes and quotes are escaped for the JavaScript string literal.
        assert!(script.contains("C:\\\\media\\\\it\\'s.mp4"), "{script}");
    }
}
