//! The actions a page may ask the runtime to perform.

use ncui_core::{Capability, CapabilitySet};

/// One action a page may ask the runtime to perform.
///
/// Each action names a JSON-RPC method, so a page's request travels the *same* pipeline an
/// agent's does: the same rate limit, the same authorization, the same consent gate, the
/// same audit event. A page is not a second, lesser caller — it is a caller whose
/// credentials are its window's grant rather than a bearer token.
///
/// The page names the *action*, never the method. That is what keeps the page from
/// addressing the runtime's dispatch table: it can ask for "the metrics", and the host
/// decides that this means `ncui.metrics.query` and requires `read.metrics` to do it.
#[derive(Debug, Clone, Copy)]
pub struct SeamAction {
    /// The name the page uses.
    pub name: &'static str,
    /// The JSON-RPC method the action maps to.
    pub method: &'static str,
    /// The capability the action requires of the page's grant.
    pub requires: CapabilitySet,
    /// What the action does, for page introspection.
    pub summary: &'static str,
    /// Whether a page is granted this action's capability without asking.
    ///
    /// The read-only actions are: a page that can ask what the runtime knows is the whole
    /// point of the bridge. An action that changes the application is not, and must be
    /// granted by the manifest.
    pub granted_by_default: bool,
    /// Whether the runtime supplies the calling window's id to the method.
    ///
    /// A page may close *the window it is in*, and must not be able to name another one.
    /// The id is taken from the call's origin, never from the page's payload.
    pub targets_caller_window: bool,
}

/// The actions a page may perform.
///
/// The default-granted ones are read-only and narrow: a page can ask what the runtime
/// knows, not what it may change. The window actions are declared but not granted — an
/// application that wants a page to open its own windows says so in its manifest, and the
/// capability is named `window`.
pub const SEAM_ACTIONS: &[SeamAction] = &[
    SeamAction {
        name: "describe",
        method: "ncui.describe",
        requires: CapabilitySet::empty(),
        summary: "Describe the runtime, its actions and the page's grant.",
        granted_by_default: true,
        targets_caller_window: false,
    },
    SeamAction {
        name: "health",
        method: "ncui.health",
        requires: CapabilitySet::empty(),
        summary: "Report runtime health.",
        granted_by_default: true,
        targets_caller_window: false,
    },
    SeamAction {
        name: "describe_metrics",
        method: "ncui.metrics.query",
        requires: CapabilitySet::empty().with(Capability::ReadMetrics),
        summary: "Read the runtime's metric snapshot.",
        granted_by_default: true,
        targets_caller_window: false,
    },
    SeamAction {
        name: "query_logs",
        method: "ncui.logs.query",
        requires: CapabilitySet::empty().with(Capability::ReadLogs),
        summary: "Query the bounded log ring.",
        granted_by_default: true,
        targets_caller_window: false,
    },
    SeamAction {
        name: "list_windows",
        method: "ncui.window.list",
        requires: CapabilitySet::empty().with(Capability::InspectRuntime),
        summary: "List the open windows.",
        granted_by_default: true,
        targets_caller_window: false,
    },
    SeamAction {
        name: "drag_region",
        method: "ncui.window.drag.region",
        requires: CapabilitySet::empty().with(Capability::Window),
        summary: "Initiate a window drag from a drag region (titlebar).",
        granted_by_default: false,
        targets_caller_window: true,
    },
    SeamAction {
        name: "list_components",
        method: "ncui.components.list",
        requires: CapabilitySet::empty().with(Capability::InspectRuntime),
        summary: "List the components mounted in every window.",
        granted_by_default: true,
        targets_caller_window: false,
    },
    SeamAction {
        name: "create_window",
        method: "ncui.window.create",
        requires: CapabilitySet::empty().with(Capability::Window),
        summary: "Open a window at an allowed URL. It may be placed with `x` and `y`, floated \
                  with `always_on_top`, framed or not with `decorations`, made see-through with \
                  `transparent`, fixed with `resizable: false`, floored with `min_width` and \
                  `min_height`, opened filling the display with `fullscreen`, opened maximised \
                  with `maximized`, or remembered by name with `remember`. Requires the `window` \
                  capability.",
        granted_by_default: false,
        targets_caller_window: false,
    },
    SeamAction {
        name: "close_window",
        method: "ncui.window.close",
        requires: CapabilitySet::empty().with(Capability::Window),
        summary: "Close the window this page is in. Requires the `window` capability.",
        granted_by_default: false,
        targets_caller_window: true,
    },
    SeamAction {
        name: "window_state",
        method: "ncui.window.state",
        requires: CapabilitySet::empty().with(Capability::Window),
        summary: "Read how this page's window is presented, or change it: `fullscreen`, \
                  `maximized`, `minimized`, `visible` or `focus`, one at a time. The answer is the \
                  state the platform reports after the change, so a user who maximised the window \
                  by hand is reported as having maximised it. The answer also reports the window's \
                  frame — whether the platform is drawing it, whether the user may resize it and \
                  whether it floats — as the platform has it rather than as the last shape asked \
                  for. The page is told when its window is resized or moved, through `resized` and \
                  `moved` events. Requires the `window` capability.",
        granted_by_default: false,
        targets_caller_window: true,
    },
    SeamAction {
        name: "window_theme",
        method: "ncui.window.theme",
        requires: CapabilitySet::empty().with(Capability::Window),
        summary: "Whether the platform is drawing its interface light or dark. The answer is the \
                  user's setting for the whole machine, so a page asks and follows rather than \
                  choosing for itself. Requires the `window` capability.",
        granted_by_default: false,
        targets_caller_window: true,
    },
    SeamAction {
        name: "window_shape",
        method: "ncui.window.shape",
        requires: CapabilitySet::empty().with(Capability::Window),
        summary: "Change how this page's window looks: its `title`, where it sits with `x` and \
                  `y`, how big it is with `width` and `height`, whether it floats with \
                  `always_on_top`, or whether the user may resize it with `resizable`. A field \
                  that is not named is left alone, so asking about the title does not move a \
                  window the user moved. Requires the `window` capability.",
        granted_by_default: false,
        targets_caller_window: true,
    },
];

/// The grant a page receives when the host offers these actions.
///
/// Derived from the actions rather than written out, so a new action cannot be added
/// without deciding what it costs the page. Only the actions marked
/// [`SeamAction::granted_by_default`] contribute: an action that changes the application
/// is declared in the table but is not handed to a page unless the manifest grants it.
#[must_use]
pub fn default_page_grant(actions: &[SeamAction]) -> ncui_webview::CapabilityGrant {
    let mut granted = CapabilitySet::empty();
    for action in actions.iter().filter(|action| action.granted_by_default) {
        for capability in action.requires.iter() {
            granted = granted.with(capability);
        }
    }
    ncui_webview::CapabilityGrant::of(&granted.iter().collect::<Vec<_>>())
}
