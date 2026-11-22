//! State-preserving reload: the `--watch` dev loop, without throwing the page's state away.
//!
//! Today a file change reloads the page with a blunt `window.location.reload()`, which destroys
//! every piece of in-memory state the page holds: a form half filled in, a scroll position, a
//! wizard on step three, a counter mid-count. The page comes back fresh and the person using it
//! starts over. For a dev loop that fires on every save, that is the difference between a tool
//! that helps and one that fights.
//!
//! # What a reload can and cannot preserve
//!
//! A page reload tears down the JavaScript heap. That is not an NC-UI limitation — it is what a
//! reload *is* — so no host can carry arbitrary in-memory state across one. What a host *can* do
//! is give the application a clean contract:
//!
//! * the application designates the state it wants kept by assigning it to
//!   `window.__ncui_state` (any value `JSON.stringify` accepts);
//! * the host, before it reloads, snapshots that one value into `sessionStorage`;
//! * after the reload, a bridge script restores it and tells the page.
//!
//! State the application does not put on `window.__ncui_state` is *not* preserved. That is the
//! honest boundary, and it is stated rather than papered over: this is not a framework's
//! component-tree resurrection, it is one well-defined slot the application opts into.
//!
//! # Opt-in, by including one script
//!
//! The restore half cannot run unless code of ours is on the page. Rather than inject a script
//! tag into the application's HTML — a transform that would make the asset server rewrite what it
//! serves — the runtime serves a bridge script at a fixed path on the page's own origin, and the
//! application includes it with one `<script>` tag when it wants state preserved:
//!
//! ```html
//! <script src="/__ncui/reload.js"></script>
//! ```
//!
//! An application that does not include the bridge gets today's behaviour unchanged: the host
//! still snapshots `window.__ncui_state` (harmless if nothing reads it back), reloads, and the
//! page comes back fresh. Nothing is injected, nothing is rewritten, and the asset server stays a
//! file server.
//!
//! # Why `sessionStorage`
//!
//! `sessionStorage` is scoped to the page's origin and its lifetime is the tab's, so the snapshot
//! cannot leak to another application and dies with the window. It is also synchronous, which
//! matters: the snapshot has to complete before `location.reload()` tears the heap down, and a
//! synchronous `setItem` inside the same script that reloads is guaranteed to run first.

use ncui_app::{Responder, Route, Served};

/// The path a page fetches the reload bridge script from.
///
/// Served on the page's own origin, so including it is a same-origin fetch with no second server
/// to trust and no per-platform URL scheme.
pub const RELOAD_BRIDGE_PATH: &str = "/__ncui/reload.js";

/// The global the application assigns the state it wants preserved across a reload.
pub const STATE_GLOBAL: &str = "__ncui_state";

/// The `sessionStorage` key the host's snapshot is written to and the bridge reads back.
///
/// Named with the runtime's own prefix so it cannot collide with a key the application chose.
const STORAGE_KEY: &str = "__ncui_reload_state";

/// The media type the bridge script is served as.
const SCRIPT_CONTENT_TYPE: &str = "text/javascript; charset=utf-8";

/// The bridge script the page includes to get its state back after a reload.
///
/// On load it looks for a snapshot the host left in `sessionStorage`. If there is one it parses it
/// back onto [`STATE_GLOBAL`] and dispatches `ncui:state-restored`, so the application can
/// rehydrate anything derived from that state (re-render a view, restore a scroll position) on an
/// event rather than polling. It then removes the key, so a state is restored exactly once and a
/// later ordinary reload — with no fresh snapshot — starts clean rather than resurrecting a stale
/// one.
///
/// Guarded so that a page which included the bridge but never set [`STATE_GLOBAL`] is untouched:
/// no snapshot means no restore, and the page behaves exactly as it would have without the bridge.
const BRIDGE_SCRIPT: &str = r#"(function () {
  "use strict";
  var KEY = "__ncui_reload_state";
  var GLOBAL = "__ncui_state";
  function restore() {
    var raw = null;
    try { raw = sessionStorage.getItem(KEY); } catch (e) { raw = null; }
    if (raw === null || raw === undefined) { return; }
    try { sessionStorage.removeItem(KEY); } catch (e) {}
    var value;
    try { value = JSON.parse(raw); } catch (e) { return; }
    try { window[GLOBAL] = value; } catch (e) { return; }
    try { window.dispatchEvent(new CustomEvent("ncui:state-restored", { detail: value })); } catch (e) {}
  }
  restore();
})();
"#;

/// The route that serves the reload bridge script.
///
/// The application includes it with `<script src="/__ncui/reload.js"></script>` when it wants
/// state preserved across a `--watch` reload. Serving it from the runtime's own route table — the
/// same mechanism that serves a GPU island frame — keeps the asset server a file server that
/// rewrites nothing.
#[must_use]
pub fn reload_route() -> Route {
    // The reload bridge is produced from the target alone and never reads a body, so the
    // responder drops the body a `POST` would carry.
    let responder: Responder = Arc::new(move |target: &str, _body: &[u8]| bridge_for(target));
    Route::new(RELOAD_BRIDGE_PATH, responder)
}

/// The bridge script for `target`, or `None` when the request is not the bridge's.
///
/// `None` is the honest "not mine", which the asset server turns into a 404. An empty body would
/// hand the page a blank script and a silent failure.
fn bridge_for(target: &str) -> Option<Served> {
    let path = target.split('?').next().unwrap_or(target);
    if path != RELOAD_BRIDGE_PATH {
        return None;
    }
    Some(Served {
        content_type: SCRIPT_CONTENT_TYPE,
        body: BRIDGE_SCRIPT.as_bytes().to_vec(),
    })
}

/// The script the host evaluates to reload the page with its designated state kept.
///
/// It snapshots [`STATE_GLOBAL`] into `sessionStorage` and *then* reloads, in that order, in one
/// synchronous script — the snapshot is guaranteed to land before the heap is torn down. Every
/// step is in a `try`, so an application that never set [`STATE_GLOBAL`], or set it to something
/// `JSON.stringify` rejects, still gets a plain reload rather than a broken one.
///
/// This is what [`crate::application`] pushes on a `--watch` file change instead of a bare
/// `window.location.reload()`.
#[must_use]
pub fn state_preserving_reload_script() -> String {
    format!(
        r"(function () {{
  try {{
    var s = window[{STATE_GLOBAL:?}];
    if (s !== undefined) {{ sessionStorage.setItem({STORAGE_KEY:?}, JSON.stringify(s)); }}
  }} catch (e) {{}}
  window.location.reload();
}})();"
    )
}

/// The `sessionStorage` key the snapshot is written to, exposed for tests and for an application
/// that wants to inspect or clear it.
#[must_use]
pub const fn storage_key() -> &'static str {
    STORAGE_KEY
}

use std::sync::Arc;

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bridge_route_serves_the_script_at_its_own_path() {
        let served = bridge_for(RELOAD_BRIDGE_PATH).expect("the bridge path is served");
        assert_eq!(served.content_type, "text/javascript; charset=utf-8");
        assert!(
            served.body.starts_with(b"(function"),
            "the script is JavaScript"
        );
    }

    #[test]
    fn a_query_string_does_not_change_the_bridge_path() {
        // A cache-busting query must not make the bridge miss: the page fetches the same script
        // whether or not a reload appended `?t=...`.
        assert!(bridge_for("/__ncui/reload.js?t=123").is_some());
    }

    #[test]
    fn a_path_that_is_not_the_bridge_is_declined_not_answered_with_an_empty_body() {
        // `None` is the honest "not mine", which the file server turns into a 404. An empty 200
        // would hand the page a blank script and a silent failure.
        assert!(bridge_for("/__ncui/other.js").is_none());
        assert!(bridge_for("/__ncui/reload.js/extra").is_none());
        assert!(bridge_for("/index.html").is_none());
    }

    #[test]
    fn the_bridge_script_restores_onto_the_designated_global_and_clears_the_key() {
        let served = bridge_for(RELOAD_BRIDGE_PATH).expect("served");
        let script = std::str::from_utf8(&served.body).expect("the bridge is UTF-8");
        // The contract, asserted on the script rather than trusted: it reads the same storage key
        // the host writes, restores onto the same global the app assigns, removes the key so the
        // state is restored exactly once, and dispatches the rehydrate event.
        assert!(
            script.contains(STORAGE_KEY),
            "the bridge must read the host's storage key `{STORAGE_KEY}`"
        );
        assert!(
            script.contains(STATE_GLOBAL),
            "the bridge must restore onto `{STATE_GLOBAL}`"
        );
        assert!(
            script.contains("removeItem"),
            "the key must be cleared so a stale state is never resurrected"
        );
        assert!(
            script.contains("ncui:state-restored"),
            "the page is told the state came back"
        );
    }

    #[test]
    fn the_reload_signal_snapshots_before_it_reloads() {
        let script = state_preserving_reload_script();
        let snapshot = script
            .find(STORAGE_KEY)
            .expect("the signal writes the storage key");
        let global = script
            .find(STATE_GLOBAL)
            .expect("the signal reads the global");
        let reload = script
            .find("location.reload()")
            .expect("the signal reloads");
        // Order is the whole contract: the snapshot must be written before the heap goes away.
        assert!(
            global < snapshot && snapshot < reload,
            "the signal must read the global, then snapshot, then reload"
        );
    }

    #[test]
    fn the_reload_signal_is_grounded_so_a_missing_or_unserialisable_state_still_reloads() {
        let script = state_preserving_reload_script();
        // Both halves are guarded: a page that never set the global, or set it to a cyclic value,
        // must still get a plain reload rather than a script that throws and leaves the page stuck.
        assert!(script.contains("try"), "the snapshot is guarded");
        assert!(script.contains("catch"), "the snapshot has a catch");
        assert!(
            script.contains("s !== undefined"),
            "a page that never set the global is not snapshotted"
        );
        // The reload is outside the guarded snapshot block, so it runs either way.
        let reload_line = script
            .lines()
            .find(|line| line.trim().starts_with("window.location.reload()"))
            .expect("the reload is its own statement");
        assert!(!reload_line.contains("try {"), "the reload always runs");
    }

    #[test]
    fn the_storage_key_is_namespaced_to_the_runtime() {
        // A key the application could have chosen would risk a collision; the runtime's own prefix
        // keeps the snapshot channel private.
        assert!(STORAGE_KEY.starts_with("__ncui_"));
        assert!(RELOAD_BRIDGE_PATH.starts_with("/__ncui/"));
    }
}
