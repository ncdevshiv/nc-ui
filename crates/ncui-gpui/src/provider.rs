//! The provider contract: how host-linked Rust registers a native view.
//!
//! A native window's content is not a sandboxed component and cannot be one — it is Rust
//! in the host process, which no WASM artifact is. The trust decision is therefore made
//! where a provider is registered, not where a window is opened: the embedder — the CLI
//! or the embedding application — registers providers at composition time, and a page
//! that opens a native window chooses only *which registered provider* and *what bounded
//! props*. ADR-020 records the reasoning.
//!
//! The contract is deliberately small. A provider answers its identifier, builds a view
//! from props, and — through the instance it returns — accepts commands after the view
//! exists. Everything else — windows, event loops, capability checks — belongs to the
//! host or the runtime.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use ncui_webview::native::NativeViewEvent;
use ncui_webview::{MAX_QUEUED_NATIVE_EVENTS, NativeViewError};

/// The bounded channel a native view emits events through.
///
/// The sink is shared memory, not an engine round-trip: the view emits from the engine
/// thread, the host drains from any thread, and the bound is enforced where the events
/// are produced. When the queue is full the oldest event is dropped and the drop is
/// counted, because a view outrunning its consumer should lose its stalest news rather
/// than its freshest — and because the count is the only way an operator learns it
/// happened.
#[derive(Debug)]
pub struct NativeEventSink {
    queue: Mutex<std::collections::VecDeque<NativeViewEvent>>,
    dropped: AtomicU64,
}

impl NativeEventSink {
    /// A sink with nothing queued and nothing dropped.
    #[must_use]
    pub fn new() -> Self {
        Self {
            queue: Mutex::new(std::collections::VecDeque::new()),
            dropped: AtomicU64::new(0),
        }
    }

    /// Emits one event, enforcing the boundary bounds.
    ///
    /// An event outside the contract's bounds is refused silently — the provider can ask
    /// [`NativeViewEvent::is_within_bounds`] before emitting, and a sink that accepted
    /// everything would move the boundary's cost into the runtime. When the queue is full
    /// the oldest event is dropped to make room, and the drop is counted.
    pub fn emit(&self, event: NativeViewEvent) {
        if !event.is_within_bounds() {
            return;
        }
        if let Ok(mut queue) = self.queue.lock() {
            if queue.len() >= MAX_QUEUED_NATIVE_EVENTS {
                queue.pop_front();
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            queue.push_back(event);
        }
    }

    /// Takes the oldest queued event, if there is one.
    #[must_use]
    pub fn poll(&self) -> Option<NativeViewEvent> {
        self.queue.lock().ok()?.pop_front()
    }

    /// How many events the queue has dropped because the consumer did not drain in time.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Default for NativeEventSink {
    fn default() -> Self {
        Self::new()
    }
}

/// What a provider is handed when a window is opened.
///
/// The context carries everything the view needs that it did not bring itself: the
/// window's identity, the props it was opened with, and the sink its events leave
/// through. The provider passes the sink into whatever entities it builds — a widget's
/// click handler is where an event is born.
#[derive(Debug)]
pub struct NativeViewContext {
    /// The window the view lives in, as the registry knows it.
    pub window: ncui_core::WindowId,
    /// The props the window was opened with — the same JSON the creation request carried.
    pub props: serde_json::Value,
    /// The sink the view's events leave through.
    pub events: Arc<NativeEventSink>,
}

/// A live native view: the instance half of a provider, created once per window.
///
/// The provider is stateless and shared; the instance is the per-window half. It exists
/// because commands after creation — replacing props, invoking verbs — mean nothing to
/// the host: only the provider knows how its own view rebuilds. The instance is always
/// driven on the engine thread, inside the window's own update, which is why the methods
/// receive the window and the app.
#[cfg(feature = "gpui-engine")]
pub trait NativeViewInstance: Send + 'static {
    /// Replaces the props the view was built with.
    ///
    /// The instance decides how much of the view rebuilds. A refusal is an answer, not a
    /// crash: the props may name a state the view cannot represent, and the caller learns
    /// why.
    ///
    /// # Errors
    ///
    /// Returns the refusal reason when the view rejects the props.
    fn update_props(
        &mut self,
        props: &serde_json::Value,
        window: &mut gpui::Window,
        cx: &mut gpui::App,
    ) -> Result<(), String>;

    /// Invokes a provider-defined verb on the view.
    ///
    /// # Errors
    ///
    /// Returns the refusal reason when the verb is unknown, the arguments are rejected,
    /// or the view is in a state where the verb makes no sense.
    fn invoke(
        &mut self,
        action: &str,
        payload: &serde_json::Value,
        window: &mut gpui::Window,
        cx: &mut gpui::App,
    ) -> Result<(), String>;
}

/// A registered native view provider.
///
/// Providers are shared across windows and threads — one registration serves every
/// window that names it — which is why the contract is `Send + Sync` and why `build`
/// takes `&self`. Per-window state belongs in the instance `build` returns, not in the
/// provider.
#[cfg(feature = "gpui-engine")]
pub trait NativeViewProvider: core::fmt::Debug + Send + Sync + 'static {
    /// The provider's stable identifier, as a window's content request names it.
    ///
    /// Namespaced and lowercase, matching the validation in `ncui-webview`
    /// (`nc-ui-kit::sonner`). The identifier is part of the agent-facing surface, so it
    /// is chosen once and not renamed.
    fn id(&self) -> &str;

    /// Builds the view for one window.
    ///
    /// The root is returned as a `gpui::AnyView` and the instance alongside it: the host
    /// cannot name the provider's entity types, and does not need to — the root is
    /// type-erased into the window and the instance carries the typed half.
    ///
    /// # Errors
    ///
    /// Returns the refusal reason when the props name a view the provider cannot build —
    /// a missing required key, a value of the wrong shape, a configuration the provider
    /// does not support.
    fn build(
        &self,
        context: NativeViewContext,
        window: &mut gpui::Window,
        cx: &mut gpui::App,
    ) -> Result<(gpui::AnyView, Box<dyn NativeViewInstance>), String>;
}

/// The process-wide registry of native view providers.
///
/// The registry is process-wide for the same reason the engine is: one process has one
/// gpui application, and the providers are the content that application can host.
/// Registration happens at composition time, before the runtime serves — a provider
/// registered late is simply absent from windows opened earlier, which is the honest
/// meaning of a registry rather than a defect.
#[cfg(feature = "gpui-engine")]
#[derive(Debug, Default)]
pub struct ProviderRegistry {
    providers: RwLock<BTreeMap<String, Arc<dyn NativeViewProvider>>>,
}

#[cfg(feature = "gpui-engine")]
impl ProviderRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers one provider, replacing any provider already under its identifier.
    ///
    /// Replacement is deliberate rather than an error: composition roots assemble the
    /// registry from whatever feature set the build carries, and the last registration
    /// winning is a rule an embedder can reason about.
    pub fn register(&self, provider: Arc<dyn NativeViewProvider>) {
        if let Ok(mut providers) = self.providers.write() {
            providers.insert(provider.id().to_owned(), provider);
        }
    }

    /// The provider registered under `id`, when one is.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<Arc<dyn NativeViewProvider>> {
        self.providers.read().ok()?.get(id).cloned()
    }

    /// Every registered identifier, ordered.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.providers
            .read()
            .map(|providers| providers.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// How many providers are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.providers.read().map_or(0, |providers| providers.len())
    }

    /// Whether no providers are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(feature = "gpui-engine")]
static REGISTRY: std::sync::OnceLock<ProviderRegistry> = std::sync::OnceLock::new();

/// The process-wide provider registry.
///
/// Composition roots register providers here before the runtime serves; the factory
/// reads it when a window names a provider. A process-wide registry is the honest shape
/// of the thing: the engine is process-wide, and a per-runtime registry would pretend at
/// an isolation one process cannot have.
#[cfg(feature = "gpui-engine")]
pub fn providers() -> &'static ProviderRegistry {
    REGISTRY.get_or_init(ProviderRegistry::new)
}

impl From<NativeViewError> for crate::GpuiHostError {
    fn from(error: NativeViewError) -> Self {
        match error {
            NativeViewError::WindowGone => Self::WindowGone {
                window: "the window is no longer open".to_owned(),
            },
            NativeViewError::Refused { reason } | NativeViewError::OutOfBounds { reason } => {
                Self::ProviderRefused {
                    provider: String::new(),
                    reason,
                }
            }
            NativeViewError::EngineUnavailable { reason } => Self::EngineUnavailable { reason },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sink_holds_events_in_order() {
        let sink = NativeEventSink::new();
        sink.emit(NativeViewEvent::new("a", "first"));
        sink.emit(NativeViewEvent::new("b", "second"));
        assert_eq!(sink.poll().map(|event| event.action).as_deref(), Some("first"));
        assert_eq!(sink.poll().map(|event| event.action).as_deref(), Some("second"));
        assert!(sink.poll().is_none());
    }

    #[test]
    fn a_full_sink_drops_the_oldest_event_and_counts_the_drop() {
        let sink = NativeEventSink::new();
        for index in 0..MAX_QUEUED_NATIVE_EVENTS + 10 {
            sink.emit(NativeViewEvent::new("slider", format!("moved-{index}")));
        }
        assert_eq!(sink.dropped(), 10);
        let first = sink.poll().expect("events remain queued");
        assert_eq!(first.action, "moved-10");
    }

    #[test]
    fn an_out_of_bounds_event_is_refused_silently() {
        let sink = NativeEventSink::new();
        let payload = serde_json::Value::String(
            "x".repeat(ncui_webview::native::MAX_NATIVE_PAYLOAD_BYTES),
        );
        sink.emit(NativeViewEvent::new("editor", "changed").with_payload(payload));
        assert!(sink.poll().is_none());
        assert_eq!(sink.dropped(), 0, "a refused event is not a dropped one");
    }
}
