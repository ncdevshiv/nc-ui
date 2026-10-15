//! Multiple windows over one engine port.
//!
//! A `WebViewBackend` is a *single* host: one engine, one lifecycle, one page. An application
//! has several. This module holds the registry that gives each window an identity, a grant,
//! and a lifecycle, and it is the single place a new window is validated before it exists.
//!
//! The registry itself never touches the engine. It depends on an injected factory, so the
//! policy — which windows may be opened, how many at once, with what grant — is testable
//! without a display and without an embedding layer. ADR-001's split is what makes that
//! possible: the runtime owns the decisions, the engine owns the pixels.

use std::collections::BTreeMap;

use ncui_core::{Capability, CapabilitySet, ComponentId, WindowId};

use crate::capability::CapabilityGrant;
use crate::lifecycle::{HostState, WindowLifecycle};
use crate::navigation::NavigationPolicy;
use crate::seam::HostSeam;

/// The maximum number of windows one registry will hold open.
///
/// A bound rather than a configuration: an unbounded registry lets a page that can call
/// `ncui.window.create` exhaust memory and window handles, and the failure would be an
/// engine error far from the call that caused it.
pub const MAX_WINDOWS: usize = 32;

/// Spawns one engine host for a window.
///
/// Returning `Option` rather than a `Result` keeps the registry free of an error type it
/// would otherwise have to invent: a factory that cannot create a host says so by returning
/// `None`, and the registry reports a refusal. The engine's own diagnostics travel in the
/// [`crate::WebViewBackend::last_error`] field.
pub trait HostFactory: Send + Sync + 'static {
    /// Creates one host for `window`, which has already been validated and registered.
    fn spawn(&self, window: WindowId, url: &str) -> Option<Box<dyn crate::WebViewBackend>>;
}

/// One live window: its identity, its policy, and the engine behind it.
#[derive(Debug)]
pub struct Window {
    /// The window's stable identity, reported to agents and carried in seams.
    pub id: WindowId,
    /// Where this window's host is in its life.
    pub lifecycle: WindowLifecycle,
    /// What the page in this window is granted.
    pub grant: CapabilityGrant,
    /// Which origins this window's page may navigate to.
    pub navigation: NavigationPolicy,
    /// The seam the page in this window speaks through.
    pub seam: HostSeam,
    /// The engine host, when one exists.
    pub host: Option<Box<dyn crate::WebViewBackend>>,
    /// The page this window was opened with.
    pub url: String,
    /// The components mounted in this window, in mount order.
    pub components: Vec<MountedComponent>,
}

impl Window {
    /// Whether the engine behind this window can serve right now.
    #[must_use]
    pub fn is_serving(&self) -> bool {
        self.lifecycle.is_serving()
            && self
                .host
                .as_ref()
                .is_some_and(crate::WebViewBackend::is_alive)
    }

    /// The engine's own failure, when it has one.
    #[must_use]
    pub fn last_error(&self) -> Option<String> {
        self.host.as_ref().and_then(|h| h.last_error())
    }

    /// A summary of the window for `ncui.window.list`.
    #[must_use]
    pub fn summary(&self) -> WindowSummary {
        WindowSummary {
            id: self.id,
            state: self.lifecycle.state().unwrap_or(HostState::Created),
            serving: self.is_serving(),
            url: self.url.clone(),
            granted: self.grant.granted(),
            error: self.last_error(),
            components: self.components.clone(),
        }
    }

    /// Records a component being mounted into this window.
    ///
    /// A mount is a runtime decision, not a page one: the component arrives through the
    /// seam's declared actions, so a page cannot mount anything the host did not declare.
    pub fn mount(&mut self, component: MountedComponent) -> Option<&MountedComponent> {
        self.components.retain(|c| c.id != component.id);
        self.components.push(component);
        self.components.last()
    }

    /// Unmounts a component, reporting whether it was mounted.
    pub fn unmount(&mut self, id: ComponentId) -> Option<MountedComponent> {
        let position = self.components.iter().position(|c| c.id == id)?;
        Some(self.components.remove(position))
    }

    /// Behaves as a host that has died when the engine reports it has.
    ///
    /// ADR-001: a crash is a real failure mode that must be *detected*. The engine is the
    /// only thing that knows it has died, so this is where that knowledge enters the state
    /// machine. Without it, `Running` would persist for a host that cannot answer.
    pub fn observe_engine_death(&mut self) {
        if self.last_error().is_some() && !self.lifecycle.is_crashed() {
            let _ = self.lifecycle.transition(HostState::Crashed);
        }
    }
}

/// The agent-facing view of one window.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WindowSummary {
    /// The window's identity, as 32 hex characters.
    pub id: WindowId,
    /// The lifecycle state the window's host reports.
    #[serde(rename = "state")]
    pub state: HostState,
    /// Whether the window is serving requests.
    pub serving: bool,
    /// The page the window holds.
    pub url: String,
    /// What the page is granted.
    pub granted: CapabilitySet,
    /// The engine's failure, when it has one.
    pub error: Option<String>,
    /// The components mounted in this window.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<MountedComponent>,
}

/// One component mounted in a window.
///
/// This is the runtime's own record of a component, not the page's claim about it: a page
/// cannot add an entry here, so `ncui.components.list` reports what the host mounted.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MountedComponent {
    /// The component's identity within the window.
    pub id: ComponentId,
    /// The component's name, as the host knows it.
    pub name: String,
    /// The negotiated ABI version, when one was agreed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abi_version: Option<String>,
    /// The capabilities the component was granted after narrowing.
    #[serde(default, skip_serializing_if = "CapabilitySet::is_empty")]
    pub granted: CapabilitySet,
}

/// A refusal to create a window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowRejection {
    /// The registry already holds [`MAX_WINDOWS`] windows.
    AtCapacity {
        /// How many windows are open.
        open: usize,
    },
    /// The grant requested for a new page includes a capability the host may not delegate.
    Ungrantable {
        /// The capability that cannot appear in a page grant.
        capability: Capability,
    },
    /// The engine could not be created. The message is the engine's own.
    EngineUnavailable {
        /// Why the engine failed, when it said.
        reason: Option<String>,
    },
}

impl std::fmt::Display for WindowRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AtCapacity { open } => write!(
                f,
                "at capacity: {open} windows are open and the limit is {MAX_WINDOWS}"
            ),
            Self::Ungrantable { capability } => write!(
                f,
                "a page cannot be granted `{capability}`; it is a runtime authority"
            ),
            Self::EngineUnavailable { reason } => match reason {
                Some(reason) => write!(f, "the engine could not be created: {reason}"),
                None => write!(f, "the engine could not be created"),
            },
        }
    }
}

impl std::error::Error for WindowRejection {}

/// The registry of live windows.
///
/// Kept in a `BTreeMap` keyed by identity so that the order of `ncui.window.list` is
/// deterministic. A `Vec` would answer in insertion order, which changes as windows are
/// closed and makes an agent's view of the list unstable across two identical calls.
#[derive(Debug)]
pub struct WindowRegistry {
    windows: BTreeMap<WindowId, Window>,
    default_navigation: NavigationPolicy,
    /// The default grant a new window's page receives.
    default_grant: CapabilityGrant,
}

impl Default for WindowRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowRegistry {
    /// An empty registry.
    ///
    /// Pages are granted nothing by default. A registry that granted something by default
    /// would be the "allow everything except the bad list" this crate's capability module
    /// exists to refuse.
    #[must_use]
    pub fn new() -> Self {
        Self {
            windows: BTreeMap::new(),
            default_navigation: NavigationPolicy::new(&[]),
            default_grant: CapabilityGrant::none(),
        }
    }

    /// The navigation policy every window without an explicit one uses.
    #[must_use]
    pub fn with_default_navigation(mut self, policy: NavigationPolicy) -> Self {
        self.default_navigation = policy;
        self
    }

    /// The grant every window without an explicit one uses.
    #[must_use]
    pub fn with_default_grant(mut self, grant: CapabilityGrant) -> Self {
        self.default_grant = grant;
        self
    }

    /// How many windows are open.
    #[must_use]
    pub fn len(&self) -> usize {
        self.windows.len()
    }

    /// Whether no windows are open.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    /// Every window, ordered by identity.
    pub fn all(&self) -> impl Iterator<Item = &Window> {
        self.windows.values()
    }

    /// Looks a window up by identity.
    #[must_use]
    pub fn get(&self, id: WindowId) -> Option<&Window> {
        self.windows.get(&id)
    }

    /// A mutable reference to a window, so a caller can drive its lifecycle.
    pub fn get_mut(&mut self, id: WindowId) -> Option<&mut Window> {
        self.windows.get_mut(&id)
    }

    /// The summaries for [`ncui.window.list`](fn).
    #[must_use]
    pub fn summaries(&self) -> Vec<WindowSummary> {
        self.windows.values().map(Window::summary).collect()
    }

    /// Opens a window.
    ///
    /// The grant is *narrowed to the default* rather than taken from the caller: a page is
    /// never granted more than the host policy allows. Delegating a capability the host
    /// holds would hand the page the host's authority, which this crate's capability module
    /// names as the failure to prevent.
    ///
    /// # Errors
    ///
    /// Returns [`WindowRejection::AtCapacity`] when the registry is full and
    /// [`WindowRejection::EngineUnavailable`] when the engine cannot be created. A window
    /// whose engine never came up is removed, so the registry never reports a window that
    /// cannot be used.
    pub fn open(
        &mut self,
        factory: &dyn HostFactory,
        url: &str,
        requested: Option<CapabilitySet>,
    ) -> Result<WindowId, WindowRejection> {
        if self.windows.len() >= MAX_WINDOWS {
            return Err(WindowRejection::AtCapacity {
                open: self.windows.len(),
            });
        }

        // A page grant is the intersection of the default and what was asked for. Asking
        // for nothing yields the default; asking for more yields only what the default
        // already allows.
        let grant = match requested {
            Some(keep) => self.default_grant.narrowed_to(keep),
            None => self.default_grant.clone(),
        };
        let seam_grant = grant.clone();

        // A fresh lifecycle accepts `Created` unconditionally — it is the only edge out of
        // "no state". If that ever changes the window is closed rather than left in a state
        // the registry does not understand.
        let mut lifecycle = WindowLifecycle::new();
        if lifecycle.transition(HostState::Created).is_err() {
            return Err(WindowRejection::EngineUnavailable {
                reason: Some("a fresh lifecycle refused `Created`".to_owned()),
            });
        }
        let mut seam_lifecycle = WindowLifecycle::new();
        if seam_lifecycle.transition(HostState::Created).is_err() {
            return Err(WindowRejection::EngineUnavailable {
                reason: Some("a fresh lifecycle refused `Created`".to_owned()),
            });
        }

        let id = WindowId::new();
        let host = factory.spawn(id, url);
        let window = Window {
            id,
            lifecycle,
            grant,
            navigation: self.default_navigation.clone(),
            seam: HostSeam::new(seam_lifecycle, seam_grant, self.default_navigation.clone()),
            host: None,
            url: url.to_owned(),
            components: Vec::new(),
        };
        self.windows.insert(id, window);

        let Some(window) = self.windows.get_mut(&id) else {
            self.windows.remove(&id);
            return Err(WindowRejection::EngineUnavailable {
                reason: Some("the window disappeared while it was opening".to_owned()),
            });
        };

        let Some(host) = host else {
            window.observe_engine_death();
            let reason = window.last_error();
            self.windows.remove(&id);
            return Err(WindowRejection::EngineUnavailable { reason });
        };
        window.host = Some(host);

        // Attach then serve. Both are the only edges out of `Created`; a refusal here means
        // the lifecycle table changed, so the window is closed rather than left stranded.
        if window.lifecycle.transition(HostState::Attaching).is_ok()
            && window.lifecycle.transition(HostState::Running).is_ok()
        {
            // The engine may have died during attach; `Running` is the state that must then
            // be corrected to `Crashed` so the window answers nothing.
            window.observe_engine_death();
            Ok(id)
        } else {
            let reason = window.last_error();
            self.windows.remove(&id);
            Err(WindowRejection::EngineUnavailable { reason })
        }
    }

    /// Closes a window, removing it from the registry.
    ///
    /// Returns the window when it existed, so a caller can observe its final state. Closing
    /// an unknown window is a refusal rather than a silent success: an agent that believes
    /// it closed a window is a bug waiting to be discovered in production.
    pub fn close(&mut self, id: WindowId) -> Option<Window> {
        self.windows.remove(&id)
    }
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
    use crate::navigation::{Origin, Scheme};
    use ncui_core::Capability;

    /// A factory that always succeeds, so lifecycle assertions do not depend on an engine.
    #[derive(Debug, Default)]
    struct Live;

    impl HostFactory for Live {
        fn spawn(&self, _window: WindowId, _url: &str) -> Option<Box<dyn crate::WebViewBackend>> {
            Some(Box::new(Fake::alive()))
        }
    }

    /// A factory that cannot create a host.
    #[derive(Debug, Default)]
    struct Dead;

    impl HostFactory for Dead {
        fn spawn(&self, _window: WindowId, _url: &str) -> Option<Box<dyn crate::WebViewBackend>> {
            None
        }
    }

    /// A minimal backend, so the registry can be driven without an engine.
    #[derive(Debug)]
    struct Fake {
        alive: bool,
        error: Option<String>,
    }

    impl Fake {
        const fn alive() -> Self {
            Self {
                alive: true,
                error: None,
            }
        }
    }

    impl crate::WebViewBackend for Fake {
        fn last_error(&self) -> Option<String> {
            self.error.clone()
        }
        fn is_alive(&self) -> bool {
            self.alive
        }
    }

    fn registry() -> WindowRegistry {
        WindowRegistry::new()
            .with_default_navigation(NavigationPolicy::new(&[Origin::new(
                Scheme::Https,
                "app.example.com",
                None,
            )]))
            .with_default_grant(CapabilityGrant::of(&[Capability::InspectRuntime]))
    }

    #[test]
    fn a_registry_opens_a_window_and_reports_it() {
        let mut registry = registry();
        let id = registry
            .open(&Live, "https://app.example.com", None)
            .expect("a live factory opens a window");
        assert_eq!(registry.len(), 1);
        let window = registry.get(id).expect("the window is registered");
        assert_eq!(window.url, "https://app.example.com");
        assert!(window.is_serving(), "a window over a live engine serves");
        let summaries = registry.summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, id);
    }

    #[test]
    fn a_window_is_granted_nothing_when_the_default_grants_nothing() {
        // The property that keeps a page from inheriting the host's authority.
        let mut registry = registry().with_default_grant(CapabilityGrant::none());
        let id = registry
            .open(
                &Live,
                "https://app.example.com",
                Some(CapabilitySet::empty().with(Capability::InspectRuntime)),
            )
            .expect("the registry opens windows");
        let window = registry.get(id).expect("registered");
        assert!(
            !window
                .grant
                .allows(CapabilitySet::empty().with(Capability::InspectRuntime)),
            "a page may not be granted more than the host allows"
        );
    }

    #[test]
    fn a_page_cannot_ask_its_way_out_of_the_default_grant() {
        // Escalation: a page asks for an authority the host does not delegate. The answer
        // must be a strict subset of the default, never more. `Deploy` is not in the
        // default grant, so the intersection is empty — the page gets nothing, and it
        // cannot reach the runtime's authority by asking for it.
        let mut registry = registry();
        let id = registry
            .open(
                &Live,
                "https://app.example.com",
                Some(CapabilitySet::empty().with(Capability::Deploy)),
            )
            .expect("the registry opens windows");
        let window = registry.get(id).expect("registered");
        assert!(
            !window
                .grant
                .allows(CapabilitySet::empty().with(Capability::InspectRuntime)),
            "asking for an undelegated authority must not widen the grant"
        );
        assert!(
            !window
                .grant
                .allows(CapabilitySet::empty().with(Capability::Deploy)),
            "a page must never be granted what the host does not delegate"
        );
    }

    #[test]
    fn a_page_asking_for_a_subset_of_the_default_receives_that_subset() {
        // The narrowing that *is* legitimate: the page asks for less than it is allowed,
        // and gets exactly what it asked for rather than the whole default.
        let mut registry = registry();
        let id = registry
            .open(
                &Live,
                "https://app.example.com",
                Some(CapabilitySet::empty().with(Capability::InspectRuntime)),
            )
            .expect("the registry opens windows");
        let window = registry.get(id).expect("registered");
        assert!(
            window
                .grant
                .allows(CapabilitySet::empty().with(Capability::InspectRuntime)),
            "a subset of the default is granted when asked for"
        );
    }

    #[test]
    fn an_engine_that_cannot_start_is_reported_rather_than_hidden() {
        let mut registry = registry();
        let error = registry
            .open(&Dead, "https://app.example.com", None)
            .expect_err("a dead factory cannot open a window");
        assert!(
            matches!(error, WindowRejection::EngineUnavailable { .. }),
            "got: {error}"
        );
        assert_eq!(registry.len(), 0, "a window that failed to open is removed");
    }

    #[test]
    fn closing_an_unknown_window_is_a_refusal_rather_than_a_silent_success() {
        let mut registry = registry();
        let id = WindowId::new();
        assert!(
            registry.close(id).is_none(),
            "an agent must not be told it closed a window that never existed"
        );
    }

    #[test]
    fn closing_an_open_window_removes_it() {
        let mut registry = registry();
        let id = registry
            .open(&Live, "https://app.example.com", None)
            .expect("live factory");
        let closed = registry.close(id).expect("the window was open");
        assert_eq!(closed.id, id);
        assert_eq!(registry.len(), 0);
        assert!(registry.get(id).is_none());
    }

    #[test]
    fn window_order_is_deterministic() {
        let mut registry = registry();
        let first = registry
            .open(&Live, "https://app.example.com/1", None)
            .expect("first");
        let second = registry
            .open(&Live, "https://app.example.com/2", None)
            .expect("second");
        let ids: Vec<_> = registry.all().map(|w| w.id).collect();
        assert_eq!(ids.len(), 2);
        assert!(
            ids.contains(&first) && ids.contains(&second),
            "both windows are listed"
        );
        // A second call returns the same order, so an agent's view is stable.
        let again: Vec<_> = registry.summaries().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, again, "the same order on a second call");
    }
}
