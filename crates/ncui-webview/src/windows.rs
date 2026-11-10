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

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use ncui_core::{Capability, CapabilitySet, ComponentId, WindowId};

use crate::PageMessage;
use crate::capability::CapabilityGrant;
use crate::lifecycle::{HostState, WindowLifecycle};
use crate::navigation::{NavigationPolicy, NavigationRequest};
use crate::request::WindowRequest;
use crate::seam::{ActionTable, HostSeam};

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
///
/// The seam is passed in rather than re-derived here, and that is the whole point of the
/// signature. The engine needs the policy to decide a navigation and to gate a message from
/// the page; if the factory built its own copy, the engine would be answering a different
/// question from the one the registry answers, and the two would drift. Sharing the seam
/// means there is exactly one policy object per window.
pub trait HostFactory: Send + Sync + 'static {
    /// Creates one host for `window`, which has already been validated and registered.
    ///
    /// The request carries the whole shape of the window — where it goes, what it is
    /// called, how big it is — so a host cannot invent a window the application did not
    /// ask for. A host that ignores the request and creates its own default window is a
    /// host that answers a different question from the one the registry asked.
    fn spawn(
        &self,
        window: WindowId,
        request: &WindowRequest,
        seam: &Arc<HostSeam>,
    ) -> Option<Box<dyn crate::WebViewBackend>>;

    /// The displays this factory's engine is on, in logical pixels.
    ///
    /// A factory that cannot report them returns nothing, and a caller treats that as "no
    /// opinion": moving a window on a guess would put it somewhere the user did not leave it.
    fn monitors(&self) -> Vec<crate::MonitorRect> {
        Vec::new()
    }

    /// The engine's own diagnostics from the last failed spawn.
    ///
    /// `spawn` returns `None` to say "the engine could not be created", and by itself that
    /// says nothing about *why*. A factory that has an engine's error to report puts it
    /// here, and the registry carries it into the refusal — so an operator reads the
    /// engine's message rather than a generic one.
    fn last_error(&self) -> Option<String> {
        None
    }
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
    ///
    /// Shared with the engine host, so the rules the engine is held to and the rules the
    /// runtime applies are the same object rather than two copies that agree today.
    pub seam: Arc<HostSeam>,
    /// The engine host, when one exists.
    pub host: Option<Box<dyn crate::WebViewBackend>>,
    /// The page this window was opened with.
    pub url: String,
    /// The title the window's frame carries.
    pub title: String,
    /// The window's width in logical pixels.
    pub width: u32,
    /// The window's height in logical pixels.
    pub height: u32,
    /// Where the window was asked to sit, when it was asked for a place.
    pub x: Option<i32>,
    /// Where the window was asked to sit, when it was asked for a place.
    pub y: Option<i32>,
    /// Whether the window was asked to float above other windows.
    pub always_on_top: bool,
    /// Whether the window was asked to carry its own frame.
    pub decorations: bool,
    /// Whether the window's background was asked to be see-through.
    pub transparent: bool,
    /// Whether the user was left able to resize the window.
    pub resizable: bool,
    /// The floor the user may drag the window to, when it was given one.
    pub min_width: Option<u32>,
    /// The floor the user may drag the window to, when it was given one.
    pub min_height: Option<u32>,
    /// Whether the window was asked to fill its display.
    pub fullscreen: bool,
    /// Whether the window was asked to be created maximised.
    pub maximized: bool,
    /// The components mounted in this window, in mount order.
    pub components: Vec<MountedComponent>,
    /// Messages the page has sent and the host has not yet consumed.
    pending_page_messages: VecDeque<PageMessage>,
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

    /// Where the window actually is and how big it is, as its host reports it.
    ///
    /// The requested geometry is what the window was *asked* for; this is what it *has*. They
    /// differ as soon as the user moves or resizes the window, which is exactly what an
    /// application that remembers a window across runs needs to know.
    #[must_use]
    pub fn geometry(&self) -> Option<crate::WindowGeometry> {
        self.host.as_ref().and_then(|host| host.geometry())
    }

    /// How this window is being presented, as its host reports it.
    ///
    /// `None` when the engine cannot ask the platform. The answer is the platform's, so a user who
    /// maximised the window by hand is reported as having maximised it.
    #[must_use]
    pub fn window_state(&self) -> Option<crate::WindowState> {
        self.host.as_ref().and_then(|host| host.window_state())
    }

    /// Asks the platform for one change to this window's presentation.
    ///
    /// Returns the state the platform reported *after* the change, or `None` when the engine
    /// could not make it — a window whose engine has gone, or one that never had the call.
    pub fn set_window_state(&self, change: crate::WindowChange) -> Option<crate::WindowState> {
        self.host
            .as_ref()
            .and_then(|host| host.set_window_state(change))
    }

    /// Asks the platform to make this window take `shape`.
    ///
    /// `false` when there is no engine to ask, which is a different answer from a platform that
    /// refused — the port has no way to tell them apart, and a caller that needs to know asks the
    /// registry, which refuses a window that is not there before this is reached.
    #[must_use]
    pub fn set_window_shape(&self, shape: &crate::WindowShape) -> bool {
        self.host
            .as_ref()
            .is_some_and(|host| host.set_window_shape(shape))
    }

    /// Whether the platform is drawing its interface light or dark.
    ///
    /// `None` when this window's backend cannot ask the platform, which is a different answer from
    /// "light": a page told "light" by a backend that guessed would follow a setting the user does
    /// not have.
    #[must_use]
    pub fn window_theme(&self) -> Option<crate::Theme> {
        self.host.as_ref().and_then(|host| host.window_theme())
    }

    /// Shows the application's tray icon, or removes it, reporting whether the platform agreed.
    ///
    /// Asked through a window because a window is the handle to the engine that owns the platform's
    /// event loop, not because the tray belongs to it: the tray belongs to the application, and an
    /// application whose window is closed is still one a user may need to reach.
    #[must_use]
    pub fn set_tray(&self, tray: Option<crate::TrayRequest>) -> bool {
        self.host.as_ref().is_some_and(|host| host.set_tray(tray))
    }

    /// Records the fields `shape` named, so the window's summary describes the window that exists.
    ///
    /// Called only after the platform applied the shape: recording it first would make `list` report
    /// a title the window does not carry, which is the defect the summary exists to avoid.
    pub fn record_shape(&mut self, shape: &crate::WindowShape) {
        if let Some(title) = &shape.title {
            self.title.clone_from(title);
        }
        if let (Some(x), Some(y)) = (shape.x, shape.y) {
            self.x = Some(x);
            self.y = Some(y);
        }
        if let (Some(width), Some(height)) = (shape.width, shape.height) {
            self.width = width;
            self.height = height;
        }
        if let Some(always_on_top) = shape.always_on_top {
            self.always_on_top = always_on_top;
        }
        if let Some(resizable) = shape.resizable {
            self.resizable = resizable;
        }
    }

    /// A summary of the window for `ncui.window.list`.
    #[must_use]
    pub fn summary(&self) -> WindowSummary {
        WindowSummary {
            id: self.id,
            state: self.lifecycle.state().unwrap_or(HostState::Created),
            serving: self.is_serving(),
            url: self.url.clone(),
            title: self.title.clone(),
            width: self.width,
            height: self.height,
            x: self.x,
            y: self.y,
            always_on_top: self.always_on_top,
            decorations: self.decorations,
            transparent: self.transparent,
            resizable: self.resizable,
            min_width: self.min_width,
            min_height: self.min_height,
            fullscreen: self.fullscreen,
            maximized: self.maximized,
            granted: self.grant.granted(),
            error: self.last_error(),
            components: self.components.clone(),
            pending_page_messages: self.pending_page_messages.len(),
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

    /// Takes the next message the page has sent, if there is one.
    ///
    /// The engine is polled first and its message is queued, so a message is never lost
    /// between "the page sent it" and "the runtime asked for it". The queue is bounded by
    /// the number of messages the engine has actually delivered, which is bounded by the
    /// engine's own channel, so a page cannot make it grow without bound here.
    pub fn take_page_message(&mut self) -> Option<PageMessage> {
        if self.pending_page_messages.is_empty()
            && let Some(host) = self.host.as_ref()
            && let Some(message) = host.poll_page_message()
        {
            self.pending_page_messages.push_back(message);
        }
        self.pending_page_messages.pop_front()
    }

    /// Queues a message the backend already received, so the runtime's pump drains it.
    ///
    /// A blocking wait consumes the message from the engine's channel; the pump reads from
    /// this queue. Without this hand-off a message the wait consumed would be dropped and
    /// the page's call would never be answered.
    pub fn push_page_message(&mut self, message: PageMessage) {
        self.pending_page_messages.push_back(message);
    }

    /// How many messages from the page are waiting to be evaluated.
    #[must_use]
    pub fn pending_page_messages(&self) -> usize {
        self.pending_page_messages.len()
    }

    /// Evaluates a script in this window's page.
    ///
    /// Returns whether the engine accepted it. A window with no engine, or a dead one,
    /// reports `false` rather than a success that never happened.
    pub fn post_to_page(&mut self, script: &str) -> bool {
        self.host
            .as_ref()
            .is_some_and(|host| host.evaluate_script(script))
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
    /// The title the window's frame carries.
    pub title: String,
    /// The window's width in logical pixels.
    pub width: u32,
    /// The window's height in logical pixels.
    pub height: u32,
    /// Where the window was asked to sit, when it was asked for a place.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x: Option<i32>,
    /// Where the window was asked to sit, when it was asked for a place.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub y: Option<i32>,
    /// Whether the window floats above other windows.
    pub always_on_top: bool,
    /// Whether the window carries its own frame.
    pub decorations: bool,
    /// Whether the window's background may be see-through.
    pub transparent: bool,
    /// Whether the user may resize the window.
    pub resizable: bool,
    /// The floor the user may drag the window to, when it was given one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_width: Option<u32>,
    /// The floor the user may drag the window to, when it was given one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_height: Option<u32>,
    /// Whether the window was asked to fill its display.
    pub fullscreen: bool,
    /// Whether the window was asked to be created maximised.
    pub maximized: bool,
    /// What the page is granted.
    pub granted: CapabilitySet,
    /// The engine's failure, when it has one.
    pub error: Option<String>,
    /// The components mounted in this window.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<MountedComponent>,
    /// How many messages the page has sent that the runtime has not yet evaluated.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub pending_page_messages: usize,
}

/// Whether a count is zero, so a zero field is omitted rather than serialised.
///
/// Takes the count by reference because `serde`'s `skip_serializing_if` calls the
/// predicate with a reference to the field. The lint that objects is correct in general
/// and wrong here, and the reason is recorded rather than worked around by a cast.
#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's `skip_serializing_if` calls the predicate with a reference"
)]
const fn is_zero(value: &usize) -> bool {
    *value == 0
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
    /// The request describes a window that cannot be created.
    ///
    /// The reason is the request's own: an empty URL, an empty title, or a size outside the
    /// range a window may be created at.
    Unrequestable {
        /// Why the request cannot be honoured.
        reason: String,
    },
    /// The window's URL is not one the policy permits.
    ///
    /// A window's *initial* load is the one navigation the engine does not offer to the
    /// policy, because the host asked for it. Checking it here is therefore the only thing
    /// standing between `ncui.window.create` and a page that opens a `file:` URL.
    Unreachable {
        /// The URL that was asked for.
        url: String,
        /// Why it was refused.
        reason: String,
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
            Self::Unrequestable { reason } => {
                write!(f, "the window request cannot be honoured: {reason}")
            }
            Self::Unreachable { url, reason } => {
                write!(f, "a window cannot be opened at `{url}`: {reason}")
            }
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
    /// The actions every window's page may perform, and what each requires.
    ///
    /// Shared by reference with each window's seam, so a command declared after a window
    /// opens is visible to that window without rebuilding its seam. The registry remains
    /// the single place that decides what a page can ask for.
    actions: ActionTable,
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
            actions: ActionTable::new(),
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

    /// Declares an action a window's page may perform, and what it requires.
    ///
    /// A page that names an action with no declaration is refused, so the default is
    /// "denied" and never "allowed". This is the same rule the seam applies, declared once
    /// here so that every window's seam is built the same way.
    #[must_use]
    pub fn with_seam_action(self, name: &str, requires: CapabilitySet) -> Self {
        self.actions.declare_trusted(name, requires, "");
        self
    }

    /// Uses an action table shared with the runtime's command registry.
    #[must_use]
    pub fn with_action_table(mut self, actions: ActionTable) -> Self {
        self.actions = actions;
        self
    }

    /// The action table new windows share.
    #[must_use]
    pub fn actions(&self) -> &ActionTable {
        &self.actions
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

    /// Every window, mutably, ordered by identity.
    pub fn all_mut(&mut self) -> impl Iterator<Item = &mut Window> {
        self.windows.values_mut()
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

    /// Waits for the next message from any window's page, up to `timeout`.
    ///
    /// The budget is divided among the open windows so a window visited first cannot hold
    /// the whole wait while another window's page is ready. A message the wait receives is
    /// queued on its window, so the runtime's pump drains it; the return value says whether
    /// one was queued.
    pub fn wait_page_message(&mut self, timeout: std::time::Duration) -> bool {
        if self.windows.is_empty() {
            return false;
        }
        let slice = timeout / self.windows.len().max(1) as u32;
        let ids: Vec<WindowId> = self.windows.values().map(|window| window.id).collect();
        for id in ids {
            let message = self
                .windows
                .get_mut(&id)
                .and_then(|window| window.host.as_ref())
                .and_then(|host| host.wait_page_message(slice));
            if let Some(message) = message
                && let Some(window) = self.windows.get_mut(&id)
            {
                window.push_page_message(message);
                return true;
            }
        }
        false
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
    /// Returns [`WindowRejection::AtCapacity`] when the registry is full,
    /// [`WindowRejection::Unrequestable`] when the request describes a window that cannot
    /// be created, and [`WindowRejection::EngineUnavailable`] when the engine cannot be
    /// created. A window whose engine never came up is removed, so the registry never
    /// reports a window that cannot be used.
    pub fn open(
        &mut self,
        factory: &dyn HostFactory,
        request: &WindowRequest,
        requested: Option<CapabilitySet>,
    ) -> Result<WindowId, WindowRejection> {
        if self.windows.len() >= MAX_WINDOWS {
            return Err(WindowRejection::AtCapacity {
                open: self.windows.len(),
            });
        }
        if let Err(error) = request.validate() {
            return Err(WindowRejection::Unrequestable {
                reason: error.to_string(),
            });
        }

        // The engine permits a window's own initial load without asking the policy: the
        // host asked for that URL, and the allow-list governs what a *page* may reach.
        // That exemption is only safe if the host has already vetted the URL, so it is
        // vetted here — the one place a window is created with a URL the runtime chose.
        let Some(navigation) = NavigationRequest::from_url(&request.url, true) else {
            return Err(WindowRejection::Unreachable {
                url: request.url.clone(),
                reason: "the URL cannot be read, and a URL the policy cannot read is refused"
                    .to_owned(),
            });
        };
        if let crate::navigation::Verdict::Deny { reason } =
            self.default_navigation.evaluate(&navigation)
        {
            return Err(WindowRejection::Unreachable {
                url: request.url.clone(),
                reason,
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
        let lifecycle = WindowLifecycle::new();
        if lifecycle.transition(HostState::Created).is_err() {
            return Err(WindowRejection::EngineUnavailable {
                reason: Some("a fresh lifecycle refused `Created`".to_owned()),
            });
        }
        // The seam shares the window's lifecycle rather than keeping its own copy. Two
        // copies would be two answers to "is this host serving?", and the seam's would be
        // the one nobody advanced — so a running window's seam would refuse every message.
        let seam_lifecycle = lifecycle.clone();

        let id = WindowId::new();
        // The seam is built before the engine is created, because the engine is handed the
        // seam: a host that had to be created first and told its policy afterwards would be
        // a host that had already navigated somewhere before it learned the rules.
        let seam = HostSeam::new(seam_lifecycle, seam_grant, self.default_navigation.clone())
            .with_action_table(self.actions.clone());
        let seam = Arc::new(seam);
        let host = factory.spawn(id, request, &seam);
        let window = Window {
            id,
            lifecycle,
            grant,
            navigation: self.default_navigation.clone(),
            seam,
            host: None,
            url: request.url.clone(),
            title: request.title.clone(),
            width: request.width,
            height: request.height,
            x: request.x,
            y: request.y,
            always_on_top: request.always_on_top,
            decorations: request.decorations,
            transparent: request.transparent,
            resizable: request.resizable,
            min_width: request.min_width,
            min_height: request.min_height,
            fullscreen: request.fullscreen,
            maximized: request.maximized,
            components: Vec::new(),
            pending_page_messages: VecDeque::new(),
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
            // The engine's own diagnostics come from the factory, because the engine is
            // gone by now and the window it would have lived in was never given one.
            let reason = factory.last_error().or_else(|| window.last_error());
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
    ///
    /// It records the engines it made so a test can drive the bridge: the engine the window
    /// holds and the engine the test holds are two handles on one shared state.
    #[derive(Debug, Default, Clone)]
    struct Live {
        created: Arc<std::sync::Mutex<Vec<RecordingBackend>>>,
    }

    impl Live {
        /// The engines this factory has made.
        fn created(&self) -> Vec<RecordingBackend> {
            self.created
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    impl HostFactory for Live {
        fn spawn(
            &self,
            _window: WindowId,
            _request: &WindowRequest,
            _seam: &Arc<HostSeam>,
        ) -> Option<Box<dyn crate::WebViewBackend>> {
            let backend = RecordingBackend::alive();
            self.created
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(backend.clone());
            Some(Box::new(backend))
        }
    }

    /// A factory that cannot create a host.
    #[derive(Debug, Default)]
    struct Dead;

    impl HostFactory for Dead {
        fn spawn(
            &self,
            _window: WindowId,
            _request: &WindowRequest,
            _seam: &Arc<HostSeam>,
        ) -> Option<Box<dyn crate::WebViewBackend>> {
            None
        }
    }

    /// The state a recording engine shares between the window and the test that made it.
    #[derive(Debug, Default)]
    struct RecordingState {
        alive: bool,
        error: Option<String>,
        messages: std::sync::Mutex<VecDeque<PageMessage>>,
        scripts: std::sync::Mutex<Vec<String>>,
    }

    /// A backend that records the bridge traffic, so the page-to-host and host-to-page
    /// directions can be driven without an engine.
    #[derive(Debug, Default, Clone)]
    struct RecordingBackend {
        state: Arc<RecordingState>,
    }

    impl RecordingBackend {
        fn alive() -> Self {
            let state = RecordingState {
                alive: true,
                error: None,
                messages: std::sync::Mutex::new(VecDeque::new()),
                scripts: std::sync::Mutex::new(Vec::new()),
            };
            Self {
                state: Arc::new(state),
            }
        }

        /// Queues a message as though the page had sent it.
        fn deliver(&self, body: &str) {
            self.state
                .messages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push_back(PageMessage::new(body));
        }

        /// The scripts the host asked the engine to evaluate.
        fn evaluated(&self) -> Vec<String> {
            self.state
                .scripts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    impl crate::WebViewBackend for RecordingBackend {
        fn last_error(&self) -> Option<String> {
            self.state.error.clone()
        }
        fn is_alive(&self) -> bool {
            self.state.alive
        }
        fn poll_page_message(&self) -> Option<PageMessage> {
            self.state
                .messages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
        }
        fn evaluate_script(&self, script: &str) -> bool {
            self.state
                .scripts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(script.to_owned());
            true
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
            .open(
                &Live::default(),
                &WindowRequest::new("https://app.example.com"),
                None,
            )
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
    fn a_message_the_page_sent_reaches_the_window_in_order() {
        // The page-to-host direction of the bridge. A message must not be lost between the
        // page sending it and the runtime asking for it, and the order must be the order the
        // page sent them in.
        let factory = Live::default();
        let mut registry = registry();
        let id = registry
            .open(
                &factory,
                &WindowRequest::new("https://app.example.com"),
                None,
            )
            .expect("the registry opens windows");
        let engines = factory.created();
        assert_eq!(engines.len(), 1, "one window is one engine");
        engines[0].deliver("{\"action\":\"describe\"}");
        engines[0].deliver("{\"action\":\"describe\"}");

        let window = registry.get_mut(id).expect("registered");
        let first = window
            .take_page_message()
            .expect("the first message the page sent must arrive");
        assert_eq!(first.body, "{\"action\":\"describe\"}");
        let second = window
            .take_page_message()
            .expect("the second message the page sent must arrive");
        assert_eq!(second.body, "{\"action\":\"describe\"}");
        assert!(
            window.take_page_message().is_none(),
            "a drained queue yields nothing"
        );
    }

    #[test]
    fn a_script_the_host_sends_reaches_the_engine() {
        // The host-to-page direction of the bridge.
        let factory = Live::default();
        let mut registry = registry();
        let id = registry
            .open(
                &factory,
                &WindowRequest::new("https://app.example.com"),
                None,
            )
            .expect("the registry opens windows");
        let engines = factory.created();

        let window = registry.get_mut(id).expect("registered");
        assert!(
            window.post_to_page("window.ncui.ack(1)"),
            "the engine must accept a script"
        );
        assert_eq!(engines[0].evaluated(), ["window.ncui.ack(1)"]);
    }

    #[test]
    fn a_window_is_granted_nothing_when_the_default_grants_nothing() {
        // The property that keeps a page from inheriting the host's authority.
        let mut registry = registry().with_default_grant(CapabilityGrant::none());
        let id = registry
            .open(
                &Live::default(),
                &WindowRequest::new("https://app.example.com"),
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
                &Live::default(),
                &WindowRequest::new("https://app.example.com"),
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
                &Live::default(),
                &WindowRequest::new("https://app.example.com"),
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
            .open(&Dead, &WindowRequest::new("https://app.example.com"), None)
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
            .open(
                &Live::default(),
                &WindowRequest::new("https://app.example.com"),
                None,
            )
            .expect("live factory");
        let closed = registry.close(id).expect("the window was open");
        assert_eq!(closed.id, id);
        assert_eq!(registry.len(), 0);
        assert!(registry.get(id).is_none());
    }

    #[test]
    fn a_window_records_the_title_and_size_it_was_asked_for() {
        let mut registry = registry();
        let request = WindowRequest::new("https://app.example.com")
            .with_title("Counter")
            .with_size(800, 600);
        let id = registry
            .open(&Live::default(), &request, None)
            .expect("live factory");
        let window = registry.get(id).expect("registered");
        assert_eq!(window.title, "Counter");
        assert_eq!(window.width, 800);
        assert_eq!(window.height, 600);
        let summary = window.summary();
        assert_eq!(summary.title, "Counter");
        assert_eq!(summary.width, 800);
        assert_eq!(summary.height, 600);
    }

    #[test]
    fn a_window_cannot_be_opened_at_a_url_the_policy_refuses() {
        // The engine permits a window's own initial load, so this check is the only thing
        // between a page that may create windows and a `file:` URL.
        let mut registry = registry();
        for url in [
            "file:///C:/Windows/win.ini",
            "javascript:alert(1)",
            "https://evil.test/",
            "data:text/html,<script>1</script>",
        ] {
            let error = registry
                .open(&Live::default(), &WindowRequest::new(url), None)
                .expect_err("an unreachable URL must be refused");
            assert!(
                matches!(error, WindowRejection::Unreachable { .. }),
                "{url} produced {error}"
            );
        }
        assert_eq!(registry.len(), 0, "no window may exist after a refusal");
    }

    #[test]
    fn a_window_opens_at_a_url_the_policy_allows() {
        let mut registry = registry();
        let id = registry
            .open(
                &Live::default(),
                &WindowRequest::new("https://app.example.com/dashboard?tab=1"),
                None,
            )
            .expect("an allowed origin opens");
        assert_eq!(registry.len(), 1);
        let window = registry.get(id).expect("registered");
        assert_eq!(window.url, "https://app.example.com/dashboard?tab=1");
    }

    #[test]
    fn a_request_that_describes_no_creatable_window_is_refused_before_the_engine_is_asked() {
        let mut registry = registry();
        let error = registry
            .open(
                &Live::default(),
                &WindowRequest::new("https://app.example.com").with_size(1, 400),
                None,
            )
            .expect_err("an impossible window must be refused");
        assert!(
            matches!(error, WindowRejection::Unrequestable { .. }),
            "got: {error}"
        );
        assert_eq!(registry.len(), 0, "a refused request opens no window");
    }

    #[test]
    fn window_order_is_deterministic() {
        let mut registry = registry();
        let first = registry
            .open(
                &Live::default(),
                &WindowRequest::new("https://app.example.com/1"),
                None,
            )
            .expect("first");
        let second = registry
            .open(
                &Live::default(),
                &WindowRequest::new("https://app.example.com/2"),
                None,
            )
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
