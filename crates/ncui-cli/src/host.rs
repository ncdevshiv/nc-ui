//! Window-host composition: how one runtime receives a page engine and a native engine.
//!
//! The runtime deliberately does not choose an embedding. It accepts one [`HostFactory`],
//! and `ncui-webview`'s routing factory is what lets that one factory represent two kinds
//! of window. This module is the CLI's half of that decision: it constructs the engines the
//! build carries, decides whether routing is needed, and keeps the failure of the native
//! engine visible instead of treating it as absence.
//!
//! The decision itself is pure and separately testable. Only the final construction is
//! feature-gated, because constructing the GPUI factory starts its engine thread.

use core::fmt;

use ncui_webview::HostFactory;

/// Which content kinds a composed host can serve.
///
/// The enum describes functioning engines rather than compiled features. A build may carry
/// the GPUI feature and still be [`HostRouting::PageOnly`] on a machine whose native engine
/// cannot start; reporting that as native-capable would be a lie the next window-open would
/// expose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostRouting {
    /// Neither content kind has an engine.
    NoHost,
    /// Pages can be hosted; native windows cannot.
    PageOnly,
    /// Native windows can be hosted; pages cannot.
    NativeOnly,
    /// Both kinds are present and the factory must route by content kind.
    Routed,
}

impl HostRouting {
    /// Whether at least one content kind has a functioning engine.
    #[must_use]
    pub const fn has_host(self) -> bool {
        !matches!(self, Self::NoHost)
    }

    /// Whether native content has a functioning engine.
    #[must_use]
    pub const fn has_native(self) -> bool {
        matches!(self, Self::NativeOnly | Self::Routed)
    }

    /// The machine-readable name of the routing decision.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoHost => "no_host",
            Self::PageOnly => "page_only",
            Self::NativeOnly => "native_only",
            Self::Routed => "routed",
        }
    }
}

/// The routing implied by the engines which are actually available.
///
/// This is deliberately a pure function: the fallible part of composition is engine
/// construction, while the policy — what the combination means — should be testable without
/// starting either engine.
#[must_use]
pub const fn decide_host_routing(page_host: bool, native_host: bool) -> HostRouting {
    match (page_host, native_host) {
        (true, true) => HostRouting::Routed,
        (true, false) => HostRouting::PageOnly,
        (false, true) => HostRouting::NativeOnly,
        (false, false) => HostRouting::NoHost,
    }
}

/// What happened when composition looked for a native host.
///
/// `Disabled` and `Failed` are different answers. A disabled feature means the build never
/// promised native content, so the page host is installed exactly as it always was. A failed
/// engine means the build *did* promise it and could not deliver: the page host is therefore
/// put behind a router whose native half is absent, so a native window-open is refused with
/// a diagnostic naming the missing host rather than being handed to the page engine.
pub enum NativeHostOutcome {
    /// This build does not carry the native host feature.
    Disabled,
    /// The native engine started and this is its factory.
    Ready(Box<dyn HostFactory>),
    /// The native engine was requested but did not start; the string is its own account.
    Failed(String),
}

impl fmt::Debug for NativeHostOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => f.write_str("NativeHostOutcome::Disabled"),
            Self::Ready(_) => f.write_str("NativeHostOutcome::Ready(..)"),
            Self::Failed(reason) => f
                .debug_tuple("NativeHostOutcome::Failed")
                .field(reason)
                .finish(),
        }
    }
}

/// A host factory plus the composition facts a caller must not have to rediscover.
///
/// The factory is optional because there is no honest factory to install when neither kind
/// has an engine: installing an empty router would make the runtime appear hosted while
/// refusing everything. The routing and the native-start error travel beside it so the
/// caller can record what happened before consuming the factory.
pub struct HostComposition {
    factory: Option<Box<dyn HostFactory>>,
    routing: HostRouting,
    native_configured: bool,
    native_error: Option<String>,
}

impl fmt::Debug for HostComposition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostComposition")
            .field("factory", &self.factory.as_ref().map(|_| "installed"))
            .field("routing", &self.routing)
            .field("native_configured", &self.native_configured)
            .field("native_error", &self.native_error)
            .finish()
    }
}

impl HostComposition {
    /// Which content kinds the composition can serve.
    #[must_use]
    pub const fn routing(&self) -> HostRouting {
        self.routing
    }

    /// Whether native support was part of this composition, whether or not it started.
    ///
    /// This is the distinction between "the build never promised native content" and "the
    /// build promised it and the engine failed": both can have no functioning native host,
    /// and only the second is a degradation the operator must see.
    #[must_use]
    pub const fn native_configured(&self) -> bool {
        self.native_configured
    }

    /// Why the native engine did not start, when it was requested and failed.
    #[must_use]
    pub fn native_error(&self) -> Option<&str> {
        self.native_error.as_deref()
    }

    /// Takes the composed factory, when either content kind has an engine.
    #[must_use]
    pub fn into_factory(self) -> Option<Box<dyn HostFactory>> {
        self.factory
    }
}

/// Composes the page and native factories into the one factory a runtime can install.
///
/// A page-only composition with native support disabled returns the page factory itself,
/// preserving the exact pre-native-host behaviour. Once native support is part of the
/// build, even a failed engine produces a router: the router is what turns a later native
/// window-open into the explicit refusal "no native host is installed" instead of a
/// misrouted page-engine failure.
#[must_use]
pub fn compose_host_factory(
    page_host: Option<Box<dyn HostFactory>>,
    native_host: NativeHostOutcome,
) -> HostComposition {
    let (native_factory, native_error, native_configured) = match native_host {
        NativeHostOutcome::Disabled => (None, None, false),
        NativeHostOutcome::Ready(factory) => (Some(factory), None, true),
        NativeHostOutcome::Failed(reason) => (None, Some(reason), true),
    };
    let routing = decide_host_routing(page_host.is_some(), native_factory.is_some());
    let factory: Option<Box<dyn HostFactory>> = match (page_host, native_factory) {
        (Some(page), Some(native)) => Some(Box::new(
            ncui_webview::RoutingFactory::new()
                .with_page_host(page)
                .with_native_host(native),
        )),
        (Some(page), None) if native_configured => Some(Box::new(
            ncui_webview::RoutingFactory::new().with_page_host(page),
        )),
        (Some(page), None) => Some(page),
        (None, Some(native)) => Some(Box::new(
            ncui_webview::RoutingFactory::new().with_native_host(native),
        )),
        (None, None) => None,
    };
    HostComposition {
        factory,
        routing,
        native_configured,
        native_error,
    }
}

/// Constructs the host factories this build of the CLI carries.
///
/// The page factory is infallible because the wry engine starts when a window is spawned;
/// the GPUI factory is fallible because its engine thread starts now. A failed GPUI start is
/// carried in the composition rather than returned as an error: pages can still run, and the
/// reason remains available for the runtime log, metrics, tracing and the command's final
/// report.
#[must_use]
pub fn platform_host_composition() -> HostComposition {
    let composition = compose_host_factory(page_host(), native_host());
    if let Some(reason) = composition.native_error() {
        tracing::warn!(
            %reason,
            "the GPUI native window host could not start; native windows will be refused"
        );
    }
    composition
}

/// The page host this build carries, when it carries one.
#[cfg(feature = "wry-host")]
fn page_host() -> Option<Box<dyn HostFactory>> {
    Some(Box::new(ncui_webview_wry::WryHostFactory))
}

/// The page host this build carries, when it carries one.
#[cfg(not(feature = "wry-host"))]
#[allow(
    clippy::unnecessary_wraps,
    reason = "the enabled form returns `Some`; the disabled form has the same shape so composition does not branch"
)]
fn page_host() -> Option<Box<dyn HostFactory>> {
    None
}

/// Starts the native host this build carries, when it carries one.
#[cfg(feature = "gpui-host")]
fn native_host() -> NativeHostOutcome {
    match ncui_gpui::GpuiHostFactory::new() {
        Ok(factory) => NativeHostOutcome::Ready(Box::new(factory)),
        Err(error) => NativeHostOutcome::Failed(error.to_string()),
    }
}

/// The native host this build carries, when it carries one.
#[cfg(not(feature = "gpui-host"))]
fn native_host() -> NativeHostOutcome {
    NativeHostOutcome::Disabled
}

/// Records a native-engine start failure where the running runtime can answer for it.
///
/// The counter makes the failure visible in the metrics surface even when nobody reads the
/// message, and the log record carries the engine's own reason for the operator who is
/// diagnosing it. The write is deliberately best-effort: observability must not turn a
/// degraded native engine into a failed application launch.
#[cfg(feature = "wry-host")]
pub(crate) fn record_native_start_failure(runtime: &ncui_runtime::Runtime, reason: &str) {
    let labels = ncui_observability::metrics::Labels::new();
    let _ = runtime
        .metrics
        .counter_inc("window.native_host_start_failures", &labels, 1);
    runtime.logs.push(ncui_observability::LogRecord::new(
        unix_millis(),
        ncui_observability::LogLevel::Warn,
        "window",
        "cli::compose_host_factory",
        format!(
            "the GPUI native window host could not start; native windows will be refused: {reason}"
        ),
    ));
}

/// The current Unix time in milliseconds, with zero for a clock before the epoch.
#[cfg(feature = "wry-host")]
fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Asks the native engine to shut down when this build has one.
///
/// GPUI shutdown is a non-blocking request, not a wait for the engine thread: closing the
/// process must not deadlock behind a loop that is already stuck. The no-feature form is a
/// no-op so the caller does not need a second copy of the feature matrix.
pub(crate) fn shutdown_native_host() {
    #[cfg(feature = "gpui-host")]
    ncui_gpui::shutdown();
}

/// Requests native-engine teardown when the composition scope ends.
///
/// A guard is used rather than a single call at the end because `ncui run` has several early
/// exits — a refused manifest, an already-running instance, a failed instance claim — and an
/// engine thread started during composition must not be left behind on any of them. Declare
/// the guard before the application it accompanies: locals drop in reverse declaration
/// order, so the application's windows close first and the engine is asked to stop last.
#[derive(Debug, Default)]
pub(crate) struct NativeHostTeardown;

impl NativeHostTeardown {
    /// A guard which will request native-engine shutdown on drop.
    #[must_use]
    pub(crate) const fn new() -> Self {
        Self
    }
}

impl Drop for NativeHostTeardown {
    fn drop(&mut self) {
        shutdown_native_host();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn availability_maps_to_the_routing_decision() {
        assert_eq!(decide_host_routing(false, false), HostRouting::NoHost);
        assert_eq!(decide_host_routing(true, false), HostRouting::PageOnly);
        assert_eq!(decide_host_routing(false, true), HostRouting::NativeOnly);
        assert_eq!(decide_host_routing(true, true), HostRouting::Routed);
    }

    #[test]
    fn routing_reports_whether_native_content_has_an_engine() {
        assert!(!HostRouting::NoHost.has_host());
        assert!(HostRouting::PageOnly.has_host());
        assert!(!HostRouting::PageOnly.has_native());
        assert!(HostRouting::NativeOnly.has_native());
        assert!(HostRouting::Routed.has_native());
    }

    #[derive(Debug)]
    struct DummyHost;

    impl HostFactory for DummyHost {
        fn spawn(
            &self,
            _window: ncui_core::WindowId,
            _request: &ncui_webview::WindowRequest,
            _seam: &std::sync::Arc<ncui_webview::HostSeam>,
        ) -> Option<Box<dyn ncui_webview::WebViewBackend>> {
            None
        }
    }

    fn page_factory() -> Option<Box<dyn HostFactory>> {
        Some(Box::new(DummyHost))
    }

    #[test]
    fn page_only_composition_keeps_the_page_host_without_a_native_promise() {
        let composition = compose_host_factory(page_factory(), NativeHostOutcome::Disabled);

        assert_eq!(composition.routing(), HostRouting::PageOnly);
        assert!(!composition.native_configured());
        assert_eq!(composition.native_error(), None);
        assert!(composition.into_factory().is_some());
    }

    #[test]
    fn native_failure_keeps_pages_and_carries_the_reason() {
        let composition = compose_host_factory(
            page_factory(),
            NativeHostOutcome::Failed("engine unavailable".to_owned()),
        );

        assert_eq!(composition.routing(), HostRouting::PageOnly);
        assert!(composition.native_configured());
        assert_eq!(composition.native_error(), Some("engine unavailable"));
        assert!(composition.into_factory().is_some());
    }

    #[test]
    fn two_hosts_compose_as_routed() {
        let composition = compose_host_factory(
            page_factory(),
            NativeHostOutcome::Ready(Box::new(DummyHost)),
        );

        assert_eq!(composition.routing(), HostRouting::Routed);
        assert!(composition.native_configured());
        assert_eq!(composition.native_error(), None);
        assert!(composition.into_factory().is_some());
    }

    #[test]
    fn native_only_composition_has_a_factory_but_no_page_host() {
        let composition = compose_host_factory(None, NativeHostOutcome::Ready(Box::new(DummyHost)));

        assert_eq!(composition.routing(), HostRouting::NativeOnly);
        assert!(composition.native_configured());
        assert!(composition.into_factory().is_some());
    }

    #[test]
    fn no_hosts_compose_to_no_factory() {
        let composition = compose_host_factory(None, NativeHostOutcome::Disabled);

        assert_eq!(composition.routing(), HostRouting::NoHost);
        assert!(!composition.native_configured());
        assert!(composition.into_factory().is_none());
    }
}
