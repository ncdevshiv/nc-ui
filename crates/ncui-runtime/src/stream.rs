//! Long-lived streaming responses: the lifecycle half of the page bridge's
//! event path.
//!
//! The transport already exists — `Runtime::emit` delivers host-to-page events
//! through `window.ncui.on` — so what this module owns is everything transport
//! cannot say: when a stream opens, when it closes, who may still write to it,
//! and what happened to the events along the way.
//!
//! # The shape
//!
//! A streaming command's handler answers once, immediately, with the
//! subscription id — and receives a [`StreamSink`]: a cloneable producer
//! handle carrying the window, the event name, and the posting path. The
//! producer thread owns the sink outright. Cancellation is three truths: the
//! page cancels explicitly, the window dying cancels implicitly (the post
//! starts failing, which the sink reads as closure rather than erroring the
//! producer), and the runtime tearing down cancels everything.
//!
//! # The convoy position, honestly
//!
//! The sink posts through the window registry's lock — the same exclusion the
//! page pump holds. That is a deliberate first increment, not an oversight:
//! stream events are interaction-rate (progress, status, feeds — single-digit
//! messages per second, not thirty frames), the post itself is a quick script
//! dispatch, and the video arc's convoy lesson applies to paths whose rate
//! makes contention measurable. If a measurement ever shows a stream convoying
//! the pump, the named direction is a per-window post channel the pump drains
//! — and this module's documentation is where that direction is recorded ahead
//! of need.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ncui_core::WindowId;
use ncui_webview::WindowRegistry;

/// One subscription's shared truth: whether it is closed, and what happened to
/// its events. The counts are the inspectable honesty — a page that cannot
/// keep up is visible here, the same shape as the video ring's dropped frames.
#[derive(Debug, Default)]
pub(crate) struct SubscriptionState {
    closed: AtomicBool,
    delivered: AtomicU64,
    failed: AtomicU64,
}

/// The registry of live subscriptions, keyed by id and by window.
///
/// The window index exists because the commonest cancellation is not a page
/// asking — it is the window dying under the stream, and close-window must
/// find every subscription the window owned without scanning values.
/// The most streams one window may own at once.
///
/// 64, the ceiling the rest of the workspace already uses for per-caller
/// resource bounding (the sidecar's pending calls): enough for an honest
/// page's every stream at once, small enough that a loop opening streams
/// without closing them is bounded in the harm it does to the pump.
pub(crate) const MAX_STREAM_SUBSCRIPTIONS: usize = 64;

/// The shared subscription table — a type alias because the full shape is
/// machinery, not meaning: id to (owning window, shared state).
type SubscriptionTable = Arc<Mutex<HashMap<u64, (WindowId, Arc<SubscriptionState>)>>>;

#[derive(Debug, Clone, Default)]
pub(crate) struct StreamSubscriptions {
    inner: SubscriptionTable,
    next_id: Arc<AtomicU64>,
}

impl StreamSubscriptions {
    /// Opens a subscription on `window`, returning its id and the shared state
    /// the sink reads.
    ///
    /// `None` when the window already owns [`MAX_STREAM_SUBSCRIPTIONS`] streams:
    /// a subscription is a posting channel the pump drains, and an unbounded
    /// count of them is an unbounded share of every pump turn. The refusal is
    /// the window's own, not global — another window's budget is unaffected,
    /// and closing a stream frees the window's capacity for the next open.
    pub(crate) fn open(&self, window: WindowId) -> Option<(u64, Arc<SubscriptionState>)> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let owned = inner.values().filter(|(owner, _)| *owner == window).count();
        if owned >= MAX_STREAM_SUBSCRIPTIONS {
            return None;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let state = Arc::new(SubscriptionState::default());
        inner.insert(id, (window, Arc::clone(&state)));
        Some((id, state))
    }

    /// Closes one subscription explicitly — the page's own cancel. Answers
    /// whether the subscription existed, because a cancel of something already
    /// gone is a truth the caller is owed rather than a silent success.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn close(&self, subscription: u64) -> bool {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((_, state)) = inner.remove(&subscription) {
            state.closed.store(true, Ordering::Release);
            true
        } else {
            false
        }
    }

    /// Closes every subscription a window owns — the window-close path, where
    /// no stream may outlive the page its events were for.
    pub(crate) fn close_window(&self, window: WindowId) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ids: Vec<u64> = inner
            .iter()
            .filter(|(_, (owner, _))| *owner == window)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some((_, state)) = inner.remove(&id) {
                state.closed.store(true, Ordering::Release);
            }
        }
    }

    /// The inspection truth for one subscription: delivered and failed counts.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn counts(&self, subscription: u64) -> Option<(u64, u64, bool)> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&subscription)
            .map(|(_, state)| {
                (
                    state.delivered.load(Ordering::Relaxed),
                    state.failed.load(Ordering::Relaxed),
                    state.closed.load(Ordering::Acquire),
                )
            })
    }
}

/// The open/close handle a command handler captures at registration time.
///
/// The factory exists because of where commands execute: a page's invoke is
/// served on the pump thread *while the runtime lock is held*, so a handler
/// that captured the runtime itself would deadlock the first time it reached
/// for it. Everything a streaming command needs — open, close, inspect — is
/// interior state behind the subscriptions mutex and the registry's own
/// mutex, never the runtime lock. Capture the factory, not the runtime.
#[derive(Debug, Clone)]
pub struct StreamFactory {
    subscriptions: StreamSubscriptions,
    registry: Arc<Mutex<Option<WindowRegistry>>>,
}

impl StreamFactory {
    pub(crate) fn new(
        subscriptions: StreamSubscriptions,
        registry: Arc<Mutex<Option<WindowRegistry>>>,
    ) -> Self {
        Self {
            subscriptions,
            registry,
        }
    }

    /// Opens a stream on `window`: the subscription registers, and the answer
    /// is the stream's id plus the producer's sink. A window that is not open
    /// is refused before any subscription exists.
    ///
    /// # Errors
    ///
    /// `NotFound` when the window is not open — a stream for nobody is a bug
    /// in the caller, not a stream.
    pub fn open(
        &self,
        window: WindowId,
        event: &str,
    ) -> Result<(u64, StreamSink), ncui_core::NcError> {
        let exists = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|registry| registry.get(window).is_some());
        if !exists {
            return Err(ncui_core::NcError::new(ncui_core::ErrorKind::NotFound {
                kind: "window".to_owned(),
                id: window.to_string(),
            })
            .operation("stream::open"));
        }
        let Some((id, state)) = self.subscriptions.open(window) else {
            return Err(ncui_core::NcError::new(ncui_core::ErrorKind::Conflict {
                reason: format!(
                    "window {window} already owns {MAX_STREAM_SUBSCRIPTIONS} open streams — the per-window budget exists so a page cannot own the pump"
                ),
            })
            .operation("stream::open"));
        };
        let sink = StreamSink::new(Arc::clone(&self.registry), window, event.to_owned(), state);
        Ok((id, sink))
    }

    /// Closes a stream explicitly — the composition point for a page's
    /// unsubscribe command. Answers whether the subscription existed.
    #[must_use]
    pub fn close(&self, subscription: u64) -> bool {
        self.subscriptions.close(subscription)
    }

    /// A stream's inspection truth: delivered count, failed count, whether it
    /// is closed. `None` when the subscription does not exist.
    #[must_use]
    pub fn counts(&self, subscription: u64) -> Option<(u64, u64, bool)> {
        self.subscriptions.counts(subscription)
    }
}

/// The producer's own handle onto one subscription: cloneable, independent of
/// the runtime's lock, and honest about closure.
///
/// A sink is how a streaming command's producer reaches its page for the
/// stream's lifetime. It never re-enters the runtime; the only lock a post
/// waits on is the window registry's own, which is the documented first-
/// increment position this module's banner explains.
pub struct StreamSink {
    registry: Arc<Mutex<Option<WindowRegistry>>>,
    window: WindowId,
    event: String,
    state: Arc<SubscriptionState>,
}

impl core::fmt::Debug for StreamSink {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StreamSink")
            .field("window", &self.window)
            .field("event", &self.event)
            .finish_non_exhaustive()
    }
}

impl Clone for StreamSink {
    fn clone(&self) -> Self {
        Self {
            registry: Arc::clone(&self.registry),
            window: self.window,
            event: self.event.clone(),
            state: Arc::clone(&self.state),
        }
    }
}

impl StreamSink {
    pub(crate) fn new(
        registry: Arc<Mutex<Option<WindowRegistry>>>,
        window: WindowId,
        event: String,
        state: Arc<SubscriptionState>,
    ) -> Self {
        Self {
            registry,
            window,
            event,
            state,
        }
    }

    /// Whether the stream is closed — pull-shaped, because the producer's own
    /// loop cadence is the engine's business. A closed sink accepts no further
    /// meaning: posts become no-ops counted as failed, because an event into a
    /// dead stream must never pretend it landed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.state.closed.load(Ordering::Acquire)
    }

    /// Posts one event to the page. Returns whether the page received it —
    /// the delivery truth, because an event the page never ran must be visible
    /// rather than silently dropped.
    ///
    /// A post against a closed stream is a counted no-op. A post whose window
    /// is gone marks the stream closed itself — the window dying under the
    /// stream is the implicit cancel, and the producer learns it here first.
    pub fn emit(&self, payload: &serde_json::Value) -> bool {
        if self.is_closed() {
            self.state.failed.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let message = serde_json::json!({ "event": self.event, "payload": payload });
        let Ok(serialised) = serde_json::to_string(&message) else {
            self.state.failed.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        let script = format!("window.ncui && window.ncui.__dispatch({serialised});");
        let posted = {
            let mut guard = self
                .registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.as_mut().is_some_and(|registry| {
                registry
                    .get_mut(self.window)
                    .is_some_and(|entry| entry.post_to_page(&script))
            })
        };
        if posted {
            self.state.delivered.fetch_add(1, Ordering::Relaxed);
        } else {
            self.state.failed.fetch_add(1, Ordering::Relaxed);
            // The window is gone or the engine refused: either way the stream's
            // meaning is over, and the producer learns closure rather than
            // posting into the void forever.
            self.state.closed.store(true, Ordering::Release);
        }
        posted
    }

    /// The event name this sink posts under — the page subscribes with
    /// `window.ncui.on(event, handler)`.
    #[must_use]
    pub fn event(&self) -> &str {
        &self.event
    }

    /// The delivered/failed counts — the stream's own inspection truth.
    #[must_use]
    pub fn counts(&self) -> (u64, u64) {
        (
            self.state.delivered.load(Ordering::Relaxed),
            self.state.failed.load(Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    missing_docs,
    clippy::panic,
    reason = "tests must abort loudly on failure"
)]
mod tests {
    use super::*;

    fn window_id(_byte: u8) -> WindowId {
        // Distinctness is all the tests need — the generator supplies it.
        WindowId::new()
    }

    #[test]
    fn a_close_is_a_truth_the_caller_can_read() {
        let subscriptions = StreamSubscriptions::default();
        let window = window_id(1);
        let (id, _state) = subscriptions.open(window).expect("the first open succeeds");
        assert!(subscriptions.close(id));
        // Closing again is honestly false — already gone is not success.
        assert!(!subscriptions.close(id));
        assert!(subscriptions.counts(id).is_none());
    }

    #[test]
    fn close_window_closes_every_stream_the_window_owned() {
        let subscriptions = StreamSubscriptions::default();
        let first = window_id(1);
        let second = window_id(2);
        let (a, state_a) = subscriptions.open(first).expect("open");
        let (b, state_b) = subscriptions.open(first).expect("open");
        let (c, state_c) = subscriptions.open(second).expect("open");
        subscriptions.close_window(first);
        assert!(state_a.closed.load(Ordering::Acquire));
        assert!(state_b.closed.load(Ordering::Acquire));
        assert!(
            !state_c.closed.load(Ordering::Acquire),
            "another window's stream is not this window's business"
        );
        assert!(subscriptions.counts(a).is_none());
        assert!(subscriptions.counts(b).is_none());
        assert!(subscriptions.counts(c).is_some());
    }

    #[test]
    fn a_closed_sink_counts_posts_as_failed_rather_than_pretending() {
        let subscriptions = StreamSubscriptions::default();
        let window = window_id(1);
        let (id, state) = subscriptions.open(window).expect("the first open succeeds");
        let sink = StreamSink::new(
            Arc::new(Mutex::new(None)),
            window,
            "test.event".to_owned(),
            state,
        );
        subscriptions.close(id);
        assert!(sink.is_closed());
        // The registry is empty — no window exists — but the closure check comes
        // first: the post is a counted no-op either way.
        assert!(!sink.emit(&serde_json::json!({"n": 1})));
        let (_, failed) = sink.counts();
        assert_eq!(failed, 1);
    }

    #[test]
    fn a_post_into_a_void_registry_marks_the_stream_closed() {
        let subscriptions = StreamSubscriptions::default();
        let window = window_id(1);
        let (_id, state) = subscriptions.open(window).expect("the first open succeeds");
        let sink = StreamSink::new(
            Arc::new(Mutex::new(None)),
            window,
            "test.event".to_owned(),
            state,
        );
        assert!(!sink.is_closed());
        // No host is installed — the registry is empty — so the window cannot
        // exist: the post fails, and the sink learns its stream is over rather
        // than posting into the void forever.
        assert!(!sink.emit(&serde_json::json!({"n": 1})));
        assert!(sink.is_closed());
    }

    #[test]
    fn a_window_cannot_own_the_pump() {
        let subscriptions = StreamSubscriptions::default();
        let window = window_id(1);
        let other = window_id(2);
        let mut ids = Vec::new();
        for _ in 0..MAX_STREAM_SUBSCRIPTIONS {
            ids.push(
                subscriptions
                    .open(window)
                    .expect("opens within the budget succeed")
                    .0,
            );
        }
        assert!(
            subscriptions.open(window).is_none(),
            "the sixty-fifth stream on one window is refused — the budget is the window's, not global"
        );
        assert!(
            subscriptions.open(other).is_some(),
            "another window's budget is unaffected"
        );
        // A close frees the window's capacity: the budget bounds open streams,
        // not lifetime opens.
        assert!(subscriptions.close(ids.pop().expect("one id")));
        assert!(
            subscriptions.open(window).is_some(),
            "closing a stream makes room for the next open"
        );
    }
}
