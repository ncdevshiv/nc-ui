//! The native-view contract: what a window whose content is Rust rather than a page is.
//!
//! ADR-020 adds a second content kind to windows. A page window's content is untrusted web
//! content behind the seam; a native window's content is host-linked Rust — trusted by the
//! act of registration, the way `ncui-native` is trusted — and ADR-020 records why that is
//! a different trust class rather than a hole in the sandbox. This module is the pure
//! contract half: the types both sides share, and the backend trait the GPUI host
//! implements. Nothing here knows gpui exists, which is why the contract can be tested on
//! a machine with no display.
//!
//! The shape deliberately mirrors the page bridge. A page talks to the runtime through
//! page messages and is talked to through evaluated scripts; a native view emits
//! [`NativeViewEvent`]s and accepts [`NativeViewCommand`]s. Both directions are bounded
//! and both directions carry JSON, because both cross a boundary where one side did not
//! write the other.

use serde::{Deserialize, Serialize};

use crate::WindowContent;

/// The largest a serialized event payload or command payload may be, in bytes.
///
/// The bound is the same one props carry, for the same reason: the bytes cross a boundary
/// in a process whose memory is not the sender's to spend, and 64 KiB is generous for
/// anything a widget has to say.
pub const MAX_NATIVE_PAYLOAD_BYTES: usize = 65_536;

/// The most events one window may hold waiting for the runtime to drain.
///
/// A native view can produce events faster than the runtime's pump drains them — a slider
/// being dragged is the ordinary case. The bound makes the overload behaviour a decision
/// rather than an accident: when the queue is full the oldest event is dropped, because a
/// stale event is worth less than a current one, and the drop is counted so an operator
/// can see it happened.
pub const MAX_QUEUED_NATIVE_EVENTS: usize = 256;

/// The longest a view event's source or action name may be, in bytes.
pub const MAX_NATIVE_NAME_BYTES: usize = 128;

/// One thing a native view told the runtime.
///
/// The `source` and `action` names are the provider's vocabulary, not the runtime's: a
/// provider building a form decides that its submit button is the source `submit` with the
/// action `clicked`. The runtime carries the event without interpreting it — interpreting
/// it would make the runtime a second copy of every provider's contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeViewEvent {
    /// Which control inside the view produced the event, in the provider's namespace.
    pub source: String,
    /// What happened, in the provider's vocabulary (`clicked`, `changed`, `submitted`).
    pub action: String,
    /// The event's data — the text of an edit, the value of a toggle, the row of a table.
    #[serde(default)]
    pub payload: serde_json::Value,
}

impl NativeViewEvent {
    /// One event named `source`/`action` with no payload.
    #[must_use]
    pub fn new(source: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            action: action.into(),
            payload: serde_json::Value::Null,
        }
    }

    /// An event carrying a payload.
    #[must_use]
    pub fn with_payload(mut self, payload: serde_json::Value) -> Self {
        self.payload = payload;
        self
    }

    /// Whether the event is small enough and well-enough named to cross the boundary.
    ///
    /// The check lives on the type rather than only in the host so a provider can ask it
    /// before emitting — an event that would be refused is one the provider can reshape
    /// while it still knows what it meant.
    #[must_use]
    pub fn is_within_bounds(&self) -> bool {
        self.source.len() <= MAX_NATIVE_NAME_BYTES
            && self.action.len() <= MAX_NATIVE_NAME_BYTES
            && serde_json::to_string(&self.payload).map_or(0, |s| s.len())
                <= MAX_NATIVE_PAYLOAD_BYTES
    }
}

/// One thing the runtime told a native view.
///
/// Commands are how the runtime — or a page, through the runtime — drives a view after it
/// exists: replacing its props, or invoking a verb the provider exposes (`focus`, `clear`,
/// `select_row`). Like events, the action vocabulary is the provider's own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NativeViewCommand {
    /// Replace the props the view was built with. The provider decides how much of the
    /// view rebuilds; the runtime does not know what the props mean.
    UpdateProps {
        /// The new props, bounded the way creation props are.
        props: serde_json::Value,
    },
    /// Invoke a provider-defined verb on the view.
    Invoke {
        /// The verb, in the provider's vocabulary.
        action: String,
        /// Its arguments.
        #[serde(default)]
        payload: serde_json::Value,
    },
}

impl NativeViewCommand {
    /// Whether the command is small enough to cross the boundary.
    #[must_use]
    pub fn is_within_bounds(&self) -> bool {
        match self {
            Self::UpdateProps { props } => {
                serde_json::to_string(props).map_or(0, |s| s.len()) <= MAX_NATIVE_PAYLOAD_BYTES
            }
            Self::Invoke { action, payload } => {
                action.len() <= MAX_NATIVE_NAME_BYTES
                    && serde_json::to_string(payload).map_or(0, |s| s.len())
                        <= MAX_NATIVE_PAYLOAD_BYTES
            }
        }
    }
}

/// One way a native view can refuse or fail a command.
///
/// The variants are deliberately few: the runtime is not a provider and cannot act on a
/// fine-grained taxonomy, so the taxonomy carries exactly the distinctions the runtime
/// does act on — "the window is gone" is retried nowhere, "the view refused" is reported
/// to the caller, "the engine died" is a host-level failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeViewError {
    /// The window the command named is no longer open.
    WindowGone,
    /// The view refused the command — an unknown verb, arguments it rejects, or a state
    /// in which the verb makes no sense. What the provider reported.
    Refused {
        /// What the provider reported.
        reason: String,
    },
    /// The command could not be delivered because the engine is gone.
    EngineUnavailable {
        /// What the host knows about the failure.
        reason: String,
    },
    /// The command itself was out of bounds before it reached the view.
    OutOfBounds {
        /// Which bound was crossed.
        reason: String,
    },
}

impl NativeViewError {
    /// The stable machine-readable failure name.
    ///
    /// These strings are part of the agent-facing API surface. Renaming one is a breaking
    /// change, so they are concentrated here where a review sees them together.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::WindowGone => "native_window_gone",
            Self::Refused { .. } => "native_view_refused",
            Self::EngineUnavailable { .. } => "native_engine_unavailable",
            Self::OutOfBounds { .. } => "native_out_of_bounds",
        }
    }
}

impl core::fmt::Display for NativeViewError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::WindowGone => write!(f, "the native window is no longer open"),
            Self::Refused { reason } => write!(f, "the native view refused the command: {reason}"),
            Self::EngineUnavailable { reason } => {
                write!(f, "the native engine is unavailable: {reason}")
            }
            Self::OutOfBounds { reason } => {
                write!(f, "the native command is out of bounds: {reason}")
            }
        }
    }
}

impl std::error::Error for NativeViewError {}

/// The native half of a window host whose content is a native view.
///
/// A window host implements this alongside [`crate::WebViewBackend`] — one object, two
/// halves, reached through [`crate::WebViewBackend::native_view`]. The web half answers
/// honestly that there is no page; this half answers the questions a native window
/// actually has: what did the view emit, and what may the runtime tell it.
///
/// Everything is `&self` with interior mutability, matching the web half: the engine
/// thread owns the real objects, and these methods are channel operations against it.
pub trait NativeViewBackend: core::fmt::Debug + Send {
    /// Takes the next event the view has emitted, if there is one.
    ///
    /// Events are bounded per window at [`MAX_QUEUED_NATIVE_EVENTS`]; when the queue is
    /// full the oldest is dropped and the drop is counted, because a view that outruns
    /// the runtime should lose its stalest news rather than its freshest.
    fn poll_native_event(&self) -> Option<NativeViewEvent>;

    /// Waits for the next event from the view, up to `timeout`.
    ///
    /// The default polls, so a backend with no blocking primitive still works — the same
    /// contract [`crate::WebViewBackend::wait_page_message`] carries.
    fn wait_native_event(&self, timeout: std::time::Duration) -> Option<NativeViewEvent> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(event) = self.poll_native_event() {
                return Some(event);
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(1).min(remaining));
        }
    }

    /// Sends one command to the view.
    ///
    /// A refusal is an answer, not a crash: the provider looked at the command and said
    /// no, and the caller learns why. Out-of-bounds commands are refused before they are
    /// sent, because the bound is the contract and the engine thread is not the place to
    /// discover it was broken.
    ///
    /// # Errors
    ///
    /// Returns [`NativeViewError`] when the window is gone, the view refuses, the engine
    /// is unavailable, or the command is out of bounds.
    fn send_view_command(&self, command: NativeViewCommand) -> Result<(), NativeViewError>;

    /// How many events the queue has dropped because the runtime did not drain it in time.
    ///
    /// The count is the overload signal an operator reads: a window dropping events is a
    /// view outrunning its consumer, and zero is the only count that means nothing was
    /// lost. The default is zero — a backend with no queue discipline to report says so
    /// by having nothing dropped.
    fn dropped_native_events(&self) -> u64 {
        0
    }
}

/// A [`crate::HostFactory`] that routes by content kind.
///
/// One runtime may host pages and native views at once — an application whose main window
/// is a page and whose settings panel is a native view is the ordinary shape, not a
/// exotic one. The registry installs exactly one factory, so the routing is itself a
/// factory: it reads the request's content kind and asks the host installed for that
/// kind. A kind with no host installed is refused with the engine's own diagnostic, so
/// the refusal says "no native host is installed" rather than something generic.
///
/// The router is pure policy — it is testable without a display, which is why it lives
/// here rather than in the runtime or a platform crate.
#[derive(Default)]
pub struct RoutingFactory {
    page: Option<Box<dyn crate::HostFactory>>,
    native: Option<Box<dyn crate::HostFactory>>,
    routing_error: std::sync::Mutex<Option<String>>,
}

impl core::fmt::Debug for RoutingFactory {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RoutingFactory")
            .field("page", &self.page.as_ref().map(|_| "installed"))
            .field("native", &self.native.as_ref().map(|_| "installed"))
            .finish_non_exhaustive()
    }
}

impl RoutingFactory {
    /// A router with no hosts installed: every kind is refused.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The host asked when the content is a page.
    #[must_use]
    pub fn with_page_host(mut self, factory: Box<dyn crate::HostFactory>) -> Self {
        self.page = Some(factory);
        self
    }

    /// The host asked when the content is a native view.
    #[must_use]
    pub fn with_native_host(mut self, factory: Box<dyn crate::HostFactory>) -> Self {
        self.native = Some(factory);
        self
    }
}

impl crate::HostFactory for RoutingFactory {
    fn spawn(
        &self,
        window: ncui_core::WindowId,
        request: &crate::WindowRequest,
        seam: &std::sync::Arc<crate::HostSeam>,
    ) -> Option<Box<dyn crate::WebViewBackend>> {
        let host = match request.content {
            WindowContent::Page => self.page.as_ref(),
            WindowContent::Native { .. } => self.native.as_ref(),
        };
        let Some(host) = host else {
            let reason = format!(
                "no {} host is installed: the {} content kind has no engine to create it",
                request.content.kind_name(),
                request.content.kind_name(),
            );
            if let Ok(mut slot) = self.routing_error.lock() {
                *slot = Some(reason);
            }
            return None;
        };
        host.spawn(window, request, seam)
    }

    fn monitors(&self) -> Vec<crate::MonitorRect> {
        // Monitor geometry is a property of the machine, not of one content kind. The page
        // host is asked first because a page host is the common case; a native-only
        // runtime asks its native host; a hostless router has no opinion.
        self.page
            .as_ref()
            .or(self.native.as_ref())
            .map_or_else(Vec::new, |host| host.monitors())
    }

    fn last_error(&self) -> Option<String> {
        if let Ok(slot) = self.routing_error.lock()
            && let Some(reason) = slot.clone()
        {
            return Some(reason);
        }
        self.page
            .as_ref()
            .and_then(|host| host.last_error())
            .or_else(|| self.native.as_ref().and_then(|host| host.last_error()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HostFactory as _;

    #[test]
    fn an_event_within_bounds_is_one_the_boundary_accepts() {
        let event = NativeViewEvent::new("submit", "clicked").with_payload(serde_json::json!({
            "text": "hello"
        }));
        assert!(event.is_within_bounds());
    }

    #[test]
    fn an_event_past_the_payload_bound_is_refused() {
        let payload = serde_json::Value::String("x".repeat(MAX_NATIVE_PAYLOAD_BYTES));
        let event = NativeViewEvent::new("editor", "changed").with_payload(payload);
        assert!(!event.is_within_bounds());
    }

    #[test]
    fn a_command_within_bounds_is_one_the_boundary_accepts() {
        let command = NativeViewCommand::Invoke {
            action: "focus".to_owned(),
            payload: serde_json::Value::Null,
        };
        assert!(command.is_within_bounds());
    }

    #[test]
    fn every_error_variant_has_a_distinct_stable_code() {
        let errors = [
            NativeViewError::WindowGone,
            NativeViewError::Refused {
                reason: "x".to_owned(),
            },
            NativeViewError::EngineUnavailable {
                reason: "x".to_owned(),
            },
            NativeViewError::OutOfBounds {
                reason: "x".to_owned(),
            },
        ];
        let mut codes: Vec<&str> = errors.iter().map(NativeViewError::code).collect();
        let count = codes.len();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), count);
    }

    #[test]
    fn commands_round_trip_through_their_serialized_form() {
        let command = NativeViewCommand::UpdateProps {
            props: serde_json::json!({"label": "Save"}),
        };
        // A failure in either direction surfaces as `None` against `Some(command)`,
        // which still names the command that failed to round-trip.
        let round_tripped = serde_json::to_string(&command)
            .ok()
            .and_then(|text| serde_json::from_str::<NativeViewCommand>(&text).ok());
        assert_eq!(round_tripped, Some(command));
    }

    #[test]
    fn a_router_with_no_hosts_refuses_with_the_kind_in_its_reason() {
        let router = RoutingFactory::new();
        let request = crate::WindowRequest::native("nc-ui-kit::gallery");
        let seam = std::sync::Arc::new(crate::HostSeam::new(
            crate::WindowLifecycle::new(),
            crate::CapabilityGrant::none(),
            crate::NavigationPolicy::new(&[]),
        ));
        let spawned = router.spawn(ncui_core::WindowId::new(), &request, &seam);
        assert!(spawned.is_none());
        let error = router.last_error();
        assert!(
            error
                .as_deref()
                .is_some_and(|reason| reason.contains("native")),
            "a refusal carries a reason naming the kind: {error:?}"
        );
    }
}
