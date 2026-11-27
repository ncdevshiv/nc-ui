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
use ncui_webview_child::{ChildWebviewEntry, ChildWebviewId, ChildWebviewRequest};

use crate::capability::CapabilityGrant;
use crate::lifecycle::{HostState, WindowLifecycle};
use crate::navigation::{NavigationPolicy, NavigationRequest};
use crate::request::WindowRequest;
use crate::seam::{ActionTable, HostSeam};
use crate::{ChildPageMessage, PageMessage};

/// The maximum number of windows one registry will hold open.
///
/// A bound rather than a configuration: an unbounded registry lets a page that can call
/// `ncui.window.create` exhaust memory and window handles, and the failure would be an
/// engine error far from the call that caused it.
pub const MAX_WINDOWS: usize = 32;

/// The maximum number of child webviews one parent window may own.
///
/// Child surfaces consume engine handles and native composition resources. Keeping the bound
/// per parent lets a runtime support several windows without allowing any one page to create an
/// unbounded number of embedded browsers.
pub const MAX_CHILDREN_PER_WINDOW: usize = 16;

/// The longest one wait blocks on either child IPC source before checking the other.
const CHILD_WAIT_SLICE: std::time::Duration = std::time::Duration::from_millis(10);

/// One child surface owned by a window.
///
/// The lifecycle is shared with the child's seam, but is independent of the parent's lifecycle.
/// The request is the last applied record, including the original URL; bounds-only updates must
/// not turn it into a navigation request.
#[derive(Debug)]
struct ChildSurface {
    lifecycle: WindowLifecycle,
    seam: Arc<HostSeam>,
    request: ChildWebviewRequest,
    pending_messages: VecDeque<ChildPageMessage>,
}

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
    /// What the window's content is: a page at the URL, or a native view from a provider.
    ///
    /// Recorded rather than re-derived, because the URL is empty for a native window and
    /// the kind is the only honest answer to "what is in this window".
    pub content: crate::WindowContent,
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
    /// Child surfaces this parent owns, keyed by the engine's canonical identity.
    children: BTreeMap<ChildWebviewId, ChildSurface>,
    /// Receivers for the IPC channels handed to child engines at creation.
    ///
    /// These stay with the parent rather than in a process-wide registry: dropping the parent
    /// drops both the receivers and the engine that owns the corresponding child surfaces.
    child_ipc_receivers: BTreeMap<ChildWebviewId, std::sync::mpsc::Receiver<ChildPageMessage>>,
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

    /// Attaches a native menu to this window, or removes it when `menu` is `None`.
    #[must_use]
    pub fn set_menu(&self, menu: Option<crate::MenuRequest>) -> bool {
        self.host.as_ref().is_some_and(|host| host.set_menu(menu))
    }

    /// Takes the next native menu selection for this window.
    #[must_use]
    pub fn poll_menu_event(&self) -> Option<crate::MenuEvent> {
        self.host.as_ref().and_then(|host| host.poll_menu_event())
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
            content: self.content.kind_name().to_owned(),
            provider: self.content.provider().map(str::to_owned),
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

    /// Creates one child webview owned by this window.
    ///
    /// The child's initial navigation is checked here because the engine performs that load on
    /// the host's behalf. The child receives the parent's navigation, action and locale policy,
    /// but an independent lifecycle and an empty grant.
    ///
    /// # Errors
    ///
    /// Returns a diagnostic when the request is invalid, the parent is full or unavailable, the
    /// initial URL is refused, the engine cannot build the surface, or the child lifecycle cannot
    /// reach `Running`.
    pub fn create_child_webview(
        &mut self,
        request: &ChildWebviewRequest,
    ) -> Result<ChildWebviewId, String> {
        request.validate().map_err(|error| error.to_string())?;
        if self.children.len() >= MAX_CHILDREN_PER_WINDOW {
            return Err(format!(
                "parent window {} already owns {MAX_CHILDREN_PER_WINDOW} child webviews",
                self.id
            ));
        }
        if !self.is_serving() {
            return Err(format!(
                "parent window {} is not serving with a live host",
                self.id
            ));
        }

        let Some(navigation) = NavigationRequest::from_url(&request.url, true) else {
            return Err(format!(
                "the child webview URL `{}` cannot be read, and an unreadable URL is refused",
                request.url
            ));
        };
        if let crate::navigation::Verdict::Deny { reason } =
            self.seam.evaluate_navigation(&navigation)
        {
            return Err(format!(
                "the child webview URL `{}` is not permitted: {reason}",
                request.url
            ));
        }

        let lifecycle = WindowLifecycle::new();
        lifecycle
            .transition(HostState::Created)
            .map_err(|error| format!("a fresh child lifecycle refused `Created`: {error}"))?;
        lifecycle
            .transition(HostState::Attaching)
            .map_err(|error| format!("the child lifecycle could not begin attaching: {error}"))?;
        let seam = Arc::new(self.seam.isolated_child(lifecycle.clone()));
        let (ipc_tx, ipc_rx) = std::sync::mpsc::sync_channel(crate::MAX_CHILD_IPC_MESSAGES);
        let Some(host) = self.host.as_ref() else {
            return Err(format!("parent window {} has no engine host", self.id));
        };
        let raw = match host.create_child_webview(request, &seam, ipc_tx) {
            Ok(raw) => raw,
            Err(error) => {
                let _ = lifecycle.transition(HostState::Crashed);
                return Err(error);
            }
        };
        let child = ChildWebviewId::from_raw(raw);

        if self.children.contains_key(&child) {
            return Err(Self::close_failed_child_build(
                host.as_ref(),
                raw,
                &lifecycle,
                format!("the engine returned duplicate child webview identity {child}"),
            ));
        }
        if let Err(error) = lifecycle.transition(HostState::Running) {
            return Err(Self::close_failed_child_build(
                host.as_ref(),
                raw,
                &lifecycle,
                format!("the child lifecycle could not reach `Running`: {error}"),
            ));
        }
        if !host.is_alive() {
            return Err(Self::close_failed_child_build(
                host.as_ref(),
                raw,
                &lifecycle,
                format!(
                    "parent window {} lost its engine while child {child} was being built",
                    self.id
                ),
            ));
        }

        // Record only a running child. The IPC receiver is retained beside the policy record so
        // messages sent through the creation channel are drained by the same polling APIs.
        self.child_ipc_receivers.insert(child, ipc_rx);
        self.children.insert(
            child,
            ChildSurface {
                lifecycle,
                seam,
                request: request.clone(),
                pending_messages: VecDeque::new(),
            },
        );
        self.queue_child_ipc_messages(child);
        Ok(child)
    }

    /// Closes an engine child after a build succeeded but could not be committed.
    fn close_failed_child_build(
        host: &dyn crate::WebViewBackend,
        child: u64,
        lifecycle: &WindowLifecycle,
        reason: String,
    ) -> String {
        let _ = lifecycle.transition(HostState::Crashed);
        match host.close_child_webview(child) {
            Ok(()) => reason,
            Err(error) => format!("{reason}; closing the engine child also failed: {error}"),
        }
    }

    /// Moves one child, validating geometry without treating its URL as a navigation.
    ///
    /// # Errors
    ///
    /// Returns a diagnostic for invalid geometry, an unknown child, a missing parent host, or an
    /// engine refusal. Stored bounds change only after the engine accepts them.
    pub fn set_child_webview_bounds(
        &mut self,
        child: ChildWebviewId,
        request: &ChildWebviewRequest,
    ) -> Result<(), String> {
        request
            .validate_bounds()
            .map_err(|error| error.to_string())?;
        if !self.children.contains_key(&child) {
            return Err(format!(
                "child webview {child} is not owned by parent window {}",
                self.id
            ));
        }
        let host = self
            .host
            .as_ref()
            .ok_or_else(|| format!("parent window {} has no engine host", self.id))?;
        host.set_child_webview_bounds(child.raw(), request)?;

        let surface = self
            .children
            .get_mut(&child)
            .ok_or_else(|| format!("child webview {child} disappeared while moving it"))?;
        surface.request.x = request.x;
        surface.request.y = request.y;
        surface.request.width = request.width;
        surface.request.height = request.height;
        Ok(())
    }

    /// Closes one child, removing its record only after the engine confirms the close.
    ///
    /// # Errors
    ///
    /// Returns a diagnostic when the child is unknown, the parent has no host, or the engine
    /// refuses the close.
    pub fn close_child_webview(&mut self, child: ChildWebviewId) -> Result<(), String> {
        let lifecycle = self
            .children
            .get(&child)
            .map(|surface| surface.lifecycle.clone())
            .ok_or_else(|| {
                format!(
                    "child webview {child} is not owned by parent window {}",
                    self.id
                )
            })?;
        let host = self
            .host
            .as_ref()
            .ok_or_else(|| format!("parent window {} has no engine host", self.id))?;
        host.close_child_webview(child.raw())?;

        let _ = lifecycle.transition(HostState::Destroying);
        let _ = lifecycle.transition(HostState::Destroyed);
        self.children.remove(&child);
        self.child_ipc_receivers.remove(&child);
        Ok(())
    }

    /// This parent's children, ordered by their canonical engine identities.
    #[must_use]
    pub fn list_child_webviews(&self) -> Vec<ChildWebviewEntry> {
        self.children
            .iter()
            .map(|(&id, surface)| ChildWebviewEntry {
                id,
                parent: self.id,
                bounds: surface.request.clone(),
            })
            .collect()
    }

    /// The canonical identities of this parent's children, in deterministic order.
    #[must_use]
    pub fn child_ids(&self) -> Vec<ChildWebviewId> {
        self.children.keys().copied().collect()
    }

    /// The isolated seam governing one child, when this parent owns it.
    #[must_use]
    pub fn seam_for_child(&self, child: ChildWebviewId) -> Option<Arc<HostSeam>> {
        self.children
            .get(&child)
            .map(|surface| surface.seam.clone())
    }

    /// Takes one child-page message, polling both IPC and the parent host when necessary.
    pub fn take_child_page_message(&mut self, child: ChildWebviewId) -> Option<ChildPageMessage> {
        if let Some(message) = self.pop_pending_child_message(child) {
            return Some(message);
        }
        self.queue_child_ipc_messages(child);
        if let Some(message) = self.pop_pending_child_message(child) {
            return Some(message);
        }

        let message = self
            .host
            .as_ref()?
            .poll_child_webview_message(child.raw())?;
        if !self.push_child_page_message(child, message) {
            return None;
        }
        self.pop_pending_child_message(child)
    }

    /// Waits for one child-page message and queues anything the wait consumes.
    ///
    /// The parent host receives the whole wait budget for this child. If it times out, the
    /// creation IPC channel is drained as well so a message arriving during the wait is not lost.
    pub fn wait_child_page_message(
        &mut self,
        child: ChildWebviewId,
        timeout: std::time::Duration,
    ) -> Option<ChildPageMessage> {
        let started = std::time::Instant::now();
        let mut primary_connected = self.child_ipc_receivers.contains_key(&child);
        loop {
            if let Some(message) = self.pop_pending_child_message(child) {
                return Some(message);
            }
            self.queue_child_ipc_messages(child);
            if let Some(message) = self.pop_pending_child_message(child) {
                return Some(message);
            }
            if !self.children.contains_key(&child) {
                return None;
            }
            if let Some(message) = self
                .host
                .as_ref()
                .and_then(|host| host.poll_child_webview_message(child.raw()))
                && self.push_child_page_message(child, message)
            {
                return self.pop_pending_child_message(child);
            }

            let elapsed = started.elapsed();
            if elapsed >= timeout {
                break;
            }
            if primary_connected {
                let slice = timeout.saturating_sub(elapsed).min(CHILD_WAIT_SLICE);
                match self
                    .child_ipc_receivers
                    .get_mut(&child)
                    .map(|receiver| receiver.recv_timeout(slice))
                {
                    Some(Ok(message)) => {
                        if self.push_child_page_message(child, message) {
                            return self.pop_pending_child_message(child);
                        }
                    }
                    Some(Err(std::sync::mpsc::RecvTimeoutError::Timeout)) => {}
                    Some(Err(std::sync::mpsc::RecvTimeoutError::Disconnected)) | None => {
                        primary_connected = false;
                        if self.host.is_none() {
                            break;
                        }
                    }
                }
            }

            let elapsed = started.elapsed();
            if elapsed >= timeout {
                break;
            }
            let slice = timeout.saturating_sub(elapsed).min(CHILD_WAIT_SLICE);
            if let Some(message) = self
                .host
                .as_ref()
                .and_then(|host| host.wait_child_webview_message(child.raw(), slice))
                && self.push_child_page_message(child, message)
            {
                return self.pop_pending_child_message(child);
            }
        }

        self.queue_child_ipc_messages(child);
        self.pop_pending_child_message(child)
    }

    /// Delivers a response script to exactly one child through its parent host.
    #[must_use]
    pub fn post_to_child_page(&mut self, child: ChildWebviewId, script: &str) -> bool {
        self.is_serving()
            && self.children.get(&child).is_some_and(|surface| {
                surface.lifecycle.is_serving()
                    && self
                        .host
                        .as_ref()
                        .is_some_and(|host| host.evaluate_child_script(child.raw(), script))
            })
    }

    /// Queues a child message a blocking wait already consumed.
    fn push_child_page_message(
        &mut self,
        child: ChildWebviewId,
        message: ChildPageMessage,
    ) -> bool {
        if message.child != child.raw() {
            return false;
        }
        let Some(surface) = self.children.get_mut(&child) else {
            return false;
        };
        if surface.pending_messages.len() >= crate::MAX_CHILD_IPC_MESSAGES {
            return false;
        }
        surface.pending_messages.push_back(message);
        true
    }

    /// Removes one already-queued child message without polling the engine.
    fn pop_pending_child_message(&mut self, child: ChildWebviewId) -> Option<ChildPageMessage> {
        self.children.get_mut(&child)?.pending_messages.pop_front()
    }

    /// Drains the creation IPC channel into the child's policy-owned queue.
    fn queue_child_ipc_messages(&mut self, child: ChildWebviewId) {
        let Some(surface) = self.children.get_mut(&child) else {
            return;
        };
        let Some(receiver) = self.child_ipc_receivers.get_mut(&child) else {
            return;
        };
        while surface.pending_messages.len() < crate::MAX_CHILD_IPC_MESSAGES
            && let Ok(message) = receiver.try_recv()
        {
            if message.child == child.raw() {
                surface.pending_messages.push_back(message);
            }
        }
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
        let engine_died = self
            .host
            .as_ref()
            .is_some_and(|host| !host.is_alive() || host.last_error().is_some());
        if !engine_died {
            return;
        }
        if !self.lifecycle.is_crashed() {
            let _ = self.lifecycle.transition(HostState::Crashed);
        }
        for surface in self.children.values_mut() {
            if matches!(
                surface.lifecycle.state(),
                Some(HostState::Attaching | HostState::Running | HostState::Suspended)
            ) {
                let _ = surface.lifecycle.transition(HostState::Crashed);
            }
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
    /// What the window's content is, as the machine-readable kind name (`page`/`native`).
    pub content: String,
    /// The provider identifier, when the content is native.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
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
    /// The platform locale and the text direction it lays out in, told to a page that asks
    /// `describe`.
    ///
    /// The runtime reads the locale and derives the direction, so every window's page is told
    /// the same matched pair. Defaults to `"und"`/`"ltr"` — the honest "unknown" — when the
    /// runtime does not say.
    default_locale: String,
    default_text_direction: String,
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
            default_locale: "und".to_owned(),
            default_text_direction: "ltr".to_owned(),
        }
    }

    /// The platform locale and its text direction, told to a page that asks `describe`.
    ///
    /// The runtime derives the direction from the locale before handing both over, so a page
    /// never sees a locale with a direction that contradicts it.
    #[must_use]
    pub fn with_default_locale(
        mut self,
        locale: impl Into<String>,
        direction: impl Into<String>,
    ) -> Self {
        self.default_locale = locale.into();
        self.default_text_direction = direction.into();
        self
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

    /// Creates a child webview owned by `parent`.
    ///
    /// # Errors
    ///
    /// Returns [`WindowRejection::EngineUnavailable`] when the parent is not open. Request,
    /// policy and engine failures remain in the inner [`Result`].
    pub fn create_child_webview(
        &mut self,
        parent: WindowId,
        request: &ChildWebviewRequest,
    ) -> Result<Result<ChildWebviewId, String>, WindowRejection> {
        let window =
            self.windows
                .get_mut(&parent)
                .ok_or_else(|| WindowRejection::EngineUnavailable {
                    reason: Some(format!("parent window {parent} is not open")),
                })?;
        Ok(window.create_child_webview(request))
    }

    /// Moves one child webview owned by `parent`.
    ///
    /// # Errors
    ///
    /// Returns [`WindowRejection::EngineUnavailable`] when the parent is not open. Ownership,
    /// geometry and engine failures remain in the inner [`Result`].
    pub fn set_child_webview_bounds(
        &mut self,
        parent: WindowId,
        child: ChildWebviewId,
        request: &ChildWebviewRequest,
    ) -> Result<Result<(), String>, WindowRejection> {
        let window =
            self.windows
                .get_mut(&parent)
                .ok_or_else(|| WindowRejection::EngineUnavailable {
                    reason: Some(format!("parent window {parent} is not open")),
                })?;
        Ok(window.set_child_webview_bounds(child, request))
    }

    /// Closes one child webview owned by `parent`.
    ///
    /// # Errors
    ///
    /// Returns [`WindowRejection::EngineUnavailable`] when the parent is not open. Ownership and
    /// engine failures remain in the inner [`Result`].
    pub fn close_child_webview(
        &mut self,
        parent: WindowId,
        child: ChildWebviewId,
    ) -> Result<Result<(), String>, WindowRejection> {
        let window =
            self.windows
                .get_mut(&parent)
                .ok_or_else(|| WindowRejection::EngineUnavailable {
                    reason: Some(format!("parent window {parent} is not open")),
                })?;
        Ok(window.close_child_webview(child))
    }

    /// Every live child across all parents, ordered by window and then child identity.
    #[must_use]
    pub fn list_child_webviews(&self) -> Vec<ChildWebviewEntry> {
        self.windows
            .values()
            .flat_map(Window::list_child_webviews)
            .collect()
    }

    /// Takes the next child-page message in deterministic window/child order.
    pub fn take_child_page_message(&mut self) -> Option<(WindowId, ChildPageMessage)> {
        let surfaces: Vec<(WindowId, ChildWebviewId)> = self
            .windows
            .iter()
            .flat_map(|(&parent, window)| {
                window
                    .child_ids()
                    .into_iter()
                    .map(move |child| (parent, child))
            })
            .collect();
        for (parent, child) in surfaces {
            if let Some(message) = self
                .windows
                .get_mut(&parent)
                .and_then(|window| window.take_child_page_message(child))
            {
                return Some((parent, message));
            }
        }
        None
    }

    /// Waits for the next message from any parent or child page, up to `timeout`.
    ///
    /// The budget is divided among every parent and child surface so the first window cannot
    /// hold the whole wait while another surface is ready. Surfaces are visited in deterministic
    /// window/child order, and anything a blocking wait consumes is queued for the runtime's pump.
    pub fn wait_page_message(&mut self, timeout: std::time::Duration) -> bool {
        let surfaces: Vec<(WindowId, Option<ChildWebviewId>)> = self
            .windows
            .values()
            .flat_map(|window| {
                core::iter::once((window.id, None)).chain(
                    window
                        .child_ids()
                        .into_iter()
                        .map(move |child| (window.id, Some(child))),
                )
            })
            .collect();
        if surfaces.is_empty() {
            return false;
        }
        let slice = timeout / surfaces.len() as u32;
        for (parent, child) in surfaces {
            let queued = match child {
                None => {
                    let message = self
                        .windows
                        .get_mut(&parent)
                        .and_then(|window| window.host.as_ref())
                        .and_then(|host| host.wait_page_message(slice));
                    if let Some(message) = message
                        && let Some(window) = self.windows.get_mut(&parent)
                    {
                        window.push_page_message(message);
                        true
                    } else {
                        false
                    }
                }
                Some(child) => self.windows.get_mut(&parent).is_some_and(|window| {
                    if window
                        .children
                        .get(&child)
                        .is_some_and(|surface| !surface.pending_messages.is_empty())
                    {
                        return true;
                    }
                    window
                        .wait_child_page_message(child, slice)
                        .is_some_and(|message| window.push_child_page_message(child, message))
                }),
            };
            if queued {
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
        // A native window is exempt rather than rejected: it has no URL and no navigation,
        // so there is nothing for the policy to decide — validation has already refused a
        // native request that tried to carry one.
        if !request.content.is_native() {
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
            .with_action_table(self.actions.clone())
            .with_locale(
                self.default_locale.clone(),
                self.default_text_direction.clone(),
            );
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
            content: request.content.clone(),
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
            children: BTreeMap::new(),
            child_ipc_receivers: BTreeMap::new(),
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
            window: WindowId,
            _request: &WindowRequest,
            _seam: &Arc<HostSeam>,
        ) -> Option<Box<dyn crate::WebViewBackend>> {
            let backend = RecordingBackend::alive(window);
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

    /// One child build, including the IPC sender the runtime handed to the engine.
    #[derive(Debug, Clone)]
    struct ChildCreation {
        request: ChildWebviewRequest,
        seam: Arc<HostSeam>,
        ipc_tx: std::sync::mpsc::SyncSender<ChildPageMessage>,
    }

    /// The state a recording engine shares between the window and the test that made it.
    #[derive(Debug, Default)]
    struct RecordingState {
        parent: WindowId,
        alive: std::sync::atomic::AtomicBool,
        error: std::sync::Mutex<Option<String>>,
        messages: std::sync::Mutex<VecDeque<PageMessage>>,
        scripts: std::sync::Mutex<Vec<String>>,
        child_creations: std::sync::Mutex<Vec<ChildCreation>>,
        child_bounds: std::sync::Mutex<Vec<(WindowId, u64, ChildWebviewRequest)>>,
        child_closes: std::sync::Mutex<Vec<(WindowId, u64)>>,
        child_scripts: std::sync::Mutex<Vec<(u64, String)>>,
        child_messages: std::sync::Mutex<BTreeMap<u64, VecDeque<ChildPageMessage>>>,
        child_waits: std::sync::Mutex<Vec<(u64, std::time::Duration)>>,
        issued_child_ids: std::sync::Mutex<VecDeque<u64>>,
        fail_child_bounds: std::sync::Mutex<bool>,
        fail_child_close: std::sync::Mutex<bool>,
    }

    /// A backend that records the bridge traffic, so the page-to-host and host-to-page
    /// directions can be driven without an engine.
    #[derive(Debug, Default, Clone)]
    struct RecordingBackend {
        state: Arc<RecordingState>,
    }

    impl RecordingBackend {
        fn alive(parent: WindowId) -> Self {
            let state = RecordingState {
                parent,
                alive: std::sync::atomic::AtomicBool::new(true),
                ..RecordingState::default()
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

    impl RecordingBackend {
        /// Makes the next child build return an exact engine identity.
        fn issue_child_id(&self, child: u64) {
            self.state
                .issued_child_ids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push_back(child);
        }

        /// Queues a message from the engine's child-message channel.
        fn deliver_child(&self, child: u64, body: &str) {
            self.state
                .child_messages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(child)
                .or_default()
                .push_back(ChildPageMessage::new(child, body));
        }

        fn child_creations(&self) -> Vec<ChildCreation> {
            self.state
                .child_creations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        fn child_bounds(&self) -> Vec<(WindowId, u64, ChildWebviewRequest)> {
            self.state
                .child_bounds
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        fn child_closes(&self) -> Vec<(WindowId, u64)> {
            self.state
                .child_closes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        fn child_scripts(&self) -> Vec<(u64, String)> {
            self.state
                .child_scripts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        fn child_waits(&self) -> Vec<(u64, std::time::Duration)> {
            self.state
                .child_waits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        fn fail_child_close(&self, fail: bool) {
            *self
                .state
                .fail_child_close
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = fail;
        }

        fn kill(&self) {
            self.state
                .alive
                .store(false, std::sync::atomic::Ordering::SeqCst);
            *self
                .state
                .error
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some("recording engine crashed".to_owned());
        }
    }

    impl crate::WebViewBackend for RecordingBackend {
        fn as_any(&self) -> &dyn core::any::Any {
            self
        }

        fn last_error(&self) -> Option<String> {
            self.state
                .error
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
        fn is_alive(&self) -> bool {
            self.state.alive.load(std::sync::atomic::Ordering::SeqCst)
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

        fn create_child_webview(
            &self,
            request: &ChildWebviewRequest,
            seam: &Arc<HostSeam>,
            ipc_tx: std::sync::mpsc::SyncSender<ChildPageMessage>,
        ) -> Result<u64, String> {
            let child = self
                .state
                .issued_child_ids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
                .unwrap_or_else(|| {
                    let creations = self
                        .state
                        .child_creations
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    10_000 + creations.len() as u64
                });
            self.state
                .child_creations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(ChildCreation {
                    request: request.clone(),
                    seam: seam.clone(),
                    ipc_tx,
                });
            Ok(child)
        }

        fn poll_child_webview_message(&self, child: u64) -> Option<ChildPageMessage> {
            self.state
                .child_messages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_mut(&child)?
                .pop_front()
        }

        fn wait_child_webview_message(
            &self,
            child: u64,
            timeout: std::time::Duration,
        ) -> Option<ChildPageMessage> {
            self.state
                .child_waits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((child, timeout));
            let deadline = std::time::Instant::now() + timeout;
            loop {
                if let Some(message) = self.poll_child_webview_message(child) {
                    return Some(message);
                }
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(1).min(remaining));
            }
        }

        fn evaluate_child_script(&self, child: u64, script: &str) -> bool {
            self.state
                .child_scripts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((child, script.to_owned()));
            true
        }

        fn set_child_webview_bounds(
            &self,
            child: u64,
            request: &ChildWebviewRequest,
        ) -> Result<(), String> {
            self.state
                .child_bounds
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((self.state.parent, child, request.clone()));
            if *self
                .state
                .fail_child_bounds
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
            {
                return Err("recording engine refused child bounds".to_owned());
            }
            Ok(())
        }

        fn close_child_webview(&self, child: u64) -> Result<(), String> {
            self.state
                .child_closes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((self.state.parent, child));
            if *self
                .state
                .fail_child_close
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
            {
                return Err("recording engine refused child close".to_owned());
            }
            Ok(())
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

    fn open_window(registry: &mut WindowRegistry, factory: &Live) -> WindowId {
        registry
            .open(
                factory,
                &WindowRequest::new("https://app.example.com"),
                None,
            )
            .expect("a live factory opens a window")
    }

    fn child_request(url: &str) -> ChildWebviewRequest {
        ChildWebviewRequest {
            url: url.to_owned(),
            x: 10,
            y: 20,
            width: 320,
            height: 240,
        }
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

    #[test]
    fn a_child_is_listed_under_its_canonical_arbitrary_engine_id() {
        let factory = Live::default();
        let mut registry = registry();
        let parent = open_window(&mut registry, &factory);
        let engine = factory.created()[0].clone();
        let raw = 0x0123_4567_89ab_cdef;
        engine.issue_child_id(raw);
        let request = child_request("https://app.example.com/child");

        let child = registry
            .create_child_webview(parent, &request)
            .expect("the parent is open")
            .expect("the engine builds the child");
        assert_eq!(child.raw(), raw, "the runtime must not invent an identity");
        let window = registry.get(parent).expect("registered");
        assert_eq!(window.child_ids(), vec![child]);
        assert_eq!(
            window.list_child_webviews(),
            vec![ChildWebviewEntry {
                id: child,
                parent,
                bounds: request.clone(),
            }]
        );
        assert_eq!(registry.list_child_webviews(), window.list_child_webviews());

        let creations = engine.child_creations();
        assert_eq!(creations.len(), 1);
        assert_eq!(creations[0].request, request);
        let child_seam = window.seam_for_child(child).expect("the child has a seam");
        assert!(Arc::ptr_eq(&child_seam, &creations[0].seam));
        assert!(!Arc::ptr_eq(&child_seam, &window.seam));
    }

    #[test]
    fn an_invalid_or_forbidden_child_url_never_reaches_the_engine() {
        let factory = Live::default();
        let mut registry = registry();
        let parent = open_window(&mut registry, &factory);
        let engine = factory.created()[0].clone();

        let mut empty = child_request("");
        assert!(
            registry
                .create_child_webview(parent, &empty)
                .expect("open parent")
                .is_err()
        );
        empty.url = "https://app.example.com".to_owned();
        empty.width = 0;
        assert!(
            registry
                .create_child_webview(parent, &empty)
                .expect("open parent")
                .is_err()
        );

        for url in [
            "not a URL",
            "file:///C:/Windows/win.ini",
            "javascript:alert(1)",
            "https://evil.test/",
            "data:text/html,<script>1</script>",
        ] {
            let error = registry
                .create_child_webview(parent, &child_request(url))
                .expect("the parent is open")
                .expect_err("an unreachable child URL must be refused");
            assert!(error.contains("URL"), "got: {error}");
        }
        assert!(engine.child_creations().is_empty());
        assert!(registry.list_child_webviews().is_empty());
    }

    #[test]
    fn a_child_has_an_empty_grant_copied_policy_and_an_independent_lifecycle() {
        let mut registry = registry()
            .with_seam_action(
                "describe_metrics",
                CapabilitySet::empty().with(Capability::InspectRuntime),
            )
            .with_default_locale("fr-FR", "ltr");
        let factory = Live::default();
        let parent = open_window(&mut registry, &factory);
        let child = registry
            .create_child_webview(parent, &child_request("https://app.example.com/child"))
            .expect("open parent")
            .expect("allowed child");
        let window = registry.get(parent).expect("registered");
        let child_lifecycle = window
            .children
            .get(&child)
            .expect("child recorded")
            .lifecycle
            .clone();
        let parent_lifecycle = window.lifecycle.clone();
        let child_seam = window.seam_for_child(child).expect("child seam");

        assert!(child_seam.grant().granted().is_empty());
        assert!(!window.grant.granted().is_empty());
        assert_eq!(
            child_seam.actions().entries(),
            window.seam.actions().entries()
        );
        let described = child_seam
            .handle(&crate::SeamMessage::Describe, &mut |_request, _payload| {
                Ok(serde_json::Value::Null)
            });
        assert!(described.ok);
        let payload = described.payload.expect("describe returns its policy");
        assert_eq!(payload["locale"], "fr-FR");
        assert_eq!(payload["text_direction"], "ltr");

        child_lifecycle
            .transition(HostState::Suspended)
            .expect("the child can suspend independently");
        assert_eq!(parent_lifecycle.state(), Some(HostState::Running));
        let refused = child_seam
            .handle(&crate::SeamMessage::Describe, &mut |_request, _payload| {
                Ok(serde_json::Value::Null)
            });
        assert!(!refused.ok, "a suspended child seam must not serve");
        child_lifecycle
            .transition(HostState::Running)
            .expect("the child can resume independently");
    }

    #[test]
    fn a_bounds_only_update_preserves_the_original_child_url() {
        let factory = Live::default();
        let mut registry = registry();
        let parent = open_window(&mut registry, &factory);
        let engine = factory.created()[0].clone();
        let child = registry
            .create_child_webview(parent, &child_request("https://app.example.com/original"))
            .expect("open parent")
            .expect("allowed child");
        let bounds = ChildWebviewRequest {
            url: String::new(),
            x: 40,
            y: 50,
            width: 640,
            height: 480,
        };

        registry
            .set_child_webview_bounds(parent, child, &bounds)
            .expect("open parent")
            .expect("the engine moves the child");
        let entry = registry
            .list_child_webviews()
            .into_iter()
            .next()
            .expect("the child is listed");
        assert_eq!(entry.bounds.url, "https://app.example.com/original");
        assert_eq!((entry.bounds.x, entry.bounds.y), (40, 50));
        assert_eq!((entry.bounds.width, entry.bounds.height), (640, 480));
        assert_eq!(
            engine.child_bounds(),
            vec![(parent, child.raw(), bounds.clone())]
        );

        let mut invalid = bounds.clone();
        invalid.width = 0;
        assert!(
            registry
                .set_child_webview_bounds(parent, child, &invalid)
                .expect("open parent")
                .is_err()
        );
        assert_eq!(
            engine.child_bounds().len(),
            1,
            "invalid geometry never reaches the engine"
        );

        *engine
            .state
            .fail_child_bounds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        assert!(
            registry
                .set_child_webview_bounds(parent, child, &bounds)
                .expect("open parent")
                .is_err()
        );
        let retained = registry
            .get(parent)
            .expect("registered")
            .list_child_webviews()[0]
            .clone();
        assert_eq!(
            retained.bounds, entry.bounds,
            "a refused move changes no record"
        );
    }

    #[test]
    fn a_child_cannot_be_moved_or_closed_through_another_parent_or_by_an_unknown_id() {
        let factory = Live::default();
        let mut registry = registry();
        let first = open_window(&mut registry, &factory);
        let second = open_window(&mut registry, &factory);
        let child = registry
            .create_child_webview(first, &child_request("https://app.example.com/child"))
            .expect("open parent")
            .expect("allowed child");
        let request = child_request("");

        assert!(
            registry
                .set_child_webview_bounds(second, child, &request)
                .expect("second parent is open")
                .is_err()
        );
        assert!(
            registry
                .close_child_webview(second, child)
                .expect("second parent is open")
                .is_err()
        );
        let unknown = ChildWebviewId::from_raw(99);
        assert!(
            registry
                .set_child_webview_bounds(first, unknown, &request)
                .expect("first parent is open")
                .is_err()
        );
        assert!(
            registry
                .close_child_webview(first, unknown)
                .expect("first parent is open")
                .is_err()
        );
        assert!(factory.created()[0].child_bounds().is_empty());
        assert!(factory.created()[0].child_closes().is_empty());

        let missing = WindowId::new();
        assert!(matches!(
            registry.create_child_webview(missing, &request),
            Err(WindowRejection::EngineUnavailable { reason: Some(_) })
        ));
        assert!(matches!(
            registry.set_child_webview_bounds(missing, child, &request),
            Err(WindowRejection::EngineUnavailable { reason: Some(_) })
        ));
        assert!(matches!(
            registry.close_child_webview(missing, child),
            Err(WindowRejection::EngineUnavailable { reason: Some(_) })
        ));
    }

    #[test]
    fn child_ipc_and_host_messages_survive_polling_and_waiting() {
        let factory = Live::default();
        let mut registry = registry();
        let parent = open_window(&mut registry, &factory);
        let engine = factory.created()[0].clone();
        let raw = 55;
        engine.issue_child_id(raw);
        let child = registry
            .create_child_webview(parent, &child_request("https://app.example.com/child"))
            .expect("open parent")
            .expect("allowed child");
        let ipc_tx = engine.child_creations()[0].ipc_tx.clone();

        ipc_tx
            .send(ChildPageMessage::new(raw, "ipc poll"))
            .expect("the child IPC sender is live");
        let window = registry.get_mut(parent).expect("registered");
        assert_eq!(
            window
                .take_child_page_message(child)
                .expect("the IPC message is polled")
                .body,
            "ipc poll"
        );

        engine.deliver_child(raw, "host wait one");
        engine.deliver_child(raw, "host wait two");
        assert_eq!(
            window
                .wait_child_page_message(child, std::time::Duration::from_millis(10))
                .expect("the host wait receives a message")
                .body,
            "host wait one"
        );
        assert!(
            engine.child_waits().is_empty(),
            "an already queued host message is polled without blocking"
        );
        assert_eq!(
            window
                .take_child_page_message(child)
                .expect("the second host message was not lost")
                .body,
            "host wait two"
        );

        engine.deliver_child(raw, "registry host wait");
        assert!(registry.wait_page_message(std::time::Duration::from_millis(20)));
        let (owner, message) = registry
            .take_child_page_message()
            .expect("the registry queued the child message");
        assert_eq!(owner, parent);
        assert_eq!(message.child, raw);
        assert_eq!(message.body, "registry host wait");

        ipc_tx
            .send(ChildPageMessage::new(raw, "registry ipc wait"))
            .expect("the child IPC sender is live");
        assert!(registry.wait_page_message(std::time::Duration::ZERO));
        assert_eq!(
            registry
                .take_child_page_message()
                .expect("the registry queued the IPC message")
                .1
                .body,
            "registry ipc wait"
        );
    }

    #[test]
    fn child_ipc_is_bounded_and_resumes_after_the_host_drains_it() {
        let factory = Live::default();
        let mut registry = registry();
        let parent = open_window(&mut registry, &factory);
        let engine = factory.created()[0].clone();
        let raw = 55;
        engine.issue_child_id(raw);
        let child = registry
            .create_child_webview(parent, &child_request("https://app.example.com/child"))
            .expect("open parent")
            .expect("allowed child");
        let ipc_tx = engine.child_creations()[0].ipc_tx.clone();

        for index in 0..crate::MAX_CHILD_IPC_MESSAGES {
            ipc_tx
                .try_send(ChildPageMessage::new(raw, format!("message {index}")))
                .expect("the bounded queue accepts its documented capacity");
        }
        assert!(
            matches!(
                ipc_tx.try_send(ChildPageMessage::new(raw, "overflow")),
                Err(std::sync::mpsc::TrySendError::Full(_))
            ),
            "a child cannot grow IPC memory without bound"
        );

        let window = registry.get_mut(parent).expect("registered");
        assert_eq!(
            window
                .take_child_page_message(child)
                .expect("the host drains one message")
                .body,
            "message 0"
        );
        ipc_tx
            .try_send(ChildPageMessage::new(raw, "after drain"))
            .expect("draining makes room in the bounded queue");
    }

    #[test]
    fn a_primary_child_ipc_message_wakes_a_wait_before_its_deadline() {
        let factory = Live::default();
        let mut registry = registry();
        let parent = open_window(&mut registry, &factory);
        let engine = factory.created()[0].clone();
        let raw = 55;
        engine.issue_child_id(raw);
        let child = registry
            .create_child_webview(parent, &child_request("https://app.example.com/child"))
            .expect("open parent")
            .expect("allowed child");
        let ipc_tx = engine.child_creations()[0].ipc_tx.clone();
        let window = registry.get_mut(parent).expect("registered");
        let sender = ipc_tx.clone();
        let delivered = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            sender
                .send(ChildPageMessage::new(raw, "primary wakeup"))
                .expect("the primary channel is live");
        });
        let started = std::time::Instant::now();
        let message = window
            .wait_child_page_message(child, std::time::Duration::from_secs(2))
            .expect("the primary message is received");
        assert_eq!(message.body, "primary wakeup");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "the primary channel must wake the wait instead of losing the whole timeout: {:?}",
            started.elapsed()
        );
        delivered.join().expect("the sender finishes");
    }

    #[test]
    fn a_child_message_with_the_wrong_identity_is_never_queued() {
        let factory = Live::default();
        let mut registry = registry();
        let parent = open_window(&mut registry, &factory);
        let engine = factory.created()[0].clone();
        let raw = 55;
        engine.issue_child_id(raw);
        let child = registry
            .create_child_webview(parent, &child_request("https://app.example.com/child"))
            .expect("open parent")
            .expect("allowed child");
        let ipc_tx = engine.child_creations()[0].ipc_tx.clone();
        ipc_tx
            .send(ChildPageMessage::new(raw + 1, "wrong IPC child"))
            .expect("the child IPC sender is live");
        ipc_tx
            .send(ChildPageMessage::new(raw, "right IPC child"))
            .expect("the child IPC sender is live");

        let window = registry.get_mut(parent).expect("registered");
        assert_eq!(
            window
                .take_child_page_message(child)
                .expect("the correctly tagged message is polled")
                .body,
            "right IPC child"
        );

        engine
            .state
            .child_messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(raw)
            .or_default()
            .push_back(ChildPageMessage::new(raw + 1, "wrong polled child"));
        assert!(window.take_child_page_message(child).is_none());

        engine
            .state
            .child_messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(raw)
            .or_default()
            .push_back(ChildPageMessage::new(raw + 1, "wrong waited child"));
        assert!(
            window
                .wait_child_page_message(child, std::time::Duration::ZERO)
                .is_none()
        );
        engine.deliver_child(raw, "right waited child");
        assert_eq!(
            window
                .wait_child_page_message(child, std::time::Duration::ZERO)
                .expect("the correctly tagged message is waited for")
                .body,
            "right waited child"
        );
    }

    #[test]
    fn a_response_script_reaches_only_the_named_child() {
        let factory = Live::default();
        let mut registry = registry();
        let parent = open_window(&mut registry, &factory);
        let engine = factory.created()[0].clone();
        engine.issue_child_id(101);
        engine.issue_child_id(202);
        let request = child_request("https://app.example.com/child");
        let first = registry
            .create_child_webview(parent, &request)
            .expect("open parent")
            .expect("first child");
        let second = registry
            .create_child_webview(parent, &request)
            .expect("open parent")
            .expect("second child");

        let window = registry.get_mut(parent).expect("registered");
        assert!(window.post_to_child_page(second, "window.ncui.answer(2)"));
        assert_eq!(
            engine.child_scripts(),
            vec![(second.raw(), "window.ncui.answer(2)".to_owned())]
        );
        assert!(
            engine.evaluated().is_empty(),
            "the parent page is not addressed"
        );
        assert!(
            !engine
                .child_scripts()
                .iter()
                .any(|(child, _)| *child == first.raw()),
            "the other child is not addressed"
        );
        assert_eq!(engine.child_scripts().len(), 1);
        let unknown = ChildWebviewId::from_raw(303);
        assert!(!window.post_to_child_page(unknown, "must not be sent"));
    }

    #[test]
    fn a_suspended_or_crashed_child_cannot_receive_scripts() {
        let factory = Live::default();
        let mut registry = registry();
        let parent = open_window(&mut registry, &factory);
        let engine = factory.created()[0].clone();
        let child = registry
            .create_child_webview(parent, &child_request("https://app.example.com/child"))
            .expect("open parent")
            .expect("allowed child");
        let lifecycle = registry
            .get(parent)
            .expect("registered")
            .children
            .get(&child)
            .expect("child recorded")
            .lifecycle
            .clone();
        let window = registry.get_mut(parent).expect("registered");

        assert!(window.post_to_child_page(child, "running"));
        lifecycle
            .transition(HostState::Suspended)
            .expect("the child suspends");
        assert!(!window.post_to_child_page(child, "suspended"));
        assert_eq!(engine.child_scripts().len(), 1);

        lifecycle
            .transition(HostState::Running)
            .expect("the child resumes");
        assert!(window.post_to_child_page(child, "resumed"));
        lifecycle
            .transition(HostState::Crashed)
            .expect("the child crashes");
        assert!(!window.post_to_child_page(child, "crashed"));
        assert_eq!(
            engine.child_scripts(),
            vec![
                (child.raw(), "running".to_owned()),
                (child.raw(), "resumed".to_owned())
            ]
        );
    }

    #[test]
    fn parent_engine_death_crashes_every_live_child_lifecycle() {
        let factory = Live::default();
        let mut registry = registry();
        let parent = open_window(&mut registry, &factory);
        let engine = factory.created()[0].clone();
        let child = registry
            .create_child_webview(parent, &child_request("https://app.example.com/child"))
            .expect("open parent")
            .expect("allowed child");
        let child_seam = registry
            .get(parent)
            .expect("registered")
            .seam_for_child(child)
            .expect("child seam");

        engine.kill();
        registry
            .get_mut(parent)
            .expect("registered")
            .observe_engine_death();
        let window = registry.get(parent).expect("registered");
        assert_eq!(window.lifecycle.state(), Some(HostState::Crashed));
        assert_eq!(
            window
                .children
                .get(&child)
                .expect("child recorded")
                .lifecycle
                .state(),
            Some(HostState::Crashed)
        );
        let response = child_seam
            .handle(&crate::SeamMessage::Describe, &mut |_request, _payload| {
                Ok(serde_json::Value::Null)
            });
        assert!(!response.ok, "a crashed child seam must refuse requests");
    }

    #[test]
    fn a_parent_cannot_own_more_than_the_child_capacity() {
        let factory = Live::default();
        let mut registry = registry();
        let parent = open_window(&mut registry, &factory);
        let request = child_request("https://app.example.com/child");
        for _ in 0..MAX_CHILDREN_PER_WINDOW {
            registry
                .create_child_webview(parent, &request)
                .expect("open parent")
                .expect("a child below capacity is created");
        }
        let error = registry
            .create_child_webview(parent, &request)
            .expect("open parent")
            .expect_err("the next child exceeds capacity");
        assert!(error.contains(&MAX_CHILDREN_PER_WINDOW.to_string()));
        assert_eq!(
            registry.list_child_webviews().len(),
            MAX_CHILDREN_PER_WINDOW
        );
        assert_eq!(
            factory.created()[0].child_creations().len(),
            MAX_CHILDREN_PER_WINDOW
        );
    }

    #[test]
    fn a_duplicate_engine_child_id_is_closed_and_refused() {
        let factory = Live::default();
        let mut registry = registry();
        let parent = open_window(&mut registry, &factory);
        let engine = factory.created()[0].clone();
        let raw = 77;
        engine.issue_child_id(raw);
        engine.issue_child_id(raw);
        let request = child_request("https://app.example.com/child");
        let child = registry
            .create_child_webview(parent, &request)
            .expect("open parent")
            .expect("the first child is recorded");
        assert_eq!(child.raw(), raw);

        let error = registry
            .create_child_webview(parent, &request)
            .expect("open parent")
            .expect_err("a duplicate identity is refused");
        assert!(error.contains("duplicate"));
        assert_eq!(engine.child_closes(), vec![(parent, raw)]);
        assert_eq!(registry.list_child_webviews().len(), 1);
    }

    #[test]
    fn a_child_is_removed_only_after_the_engine_confirms_close() {
        let factory = Live::default();
        let mut registry = registry();
        let parent = open_window(&mut registry, &factory);
        let engine = factory.created()[0].clone();
        let child = registry
            .create_child_webview(parent, &child_request("https://app.example.com/child"))
            .expect("open parent")
            .expect("allowed child");
        let seam = registry
            .get(parent)
            .expect("registered")
            .seam_for_child(child)
            .expect("child seam");

        engine.fail_child_close(true);
        assert!(
            registry
                .close_child_webview(parent, child)
                .expect("open parent")
                .is_err()
        );
        assert_eq!(registry.list_child_webviews().len(), 1);
        assert_eq!(engine.child_closes(), vec![(parent, child.raw())]);

        engine.fail_child_close(false);
        registry
            .close_child_webview(parent, child)
            .expect("open parent")
            .expect("the engine closes the child");
        assert!(registry.list_child_webviews().is_empty());
        let response = seam.handle(&crate::SeamMessage::Describe, &mut |_request, _payload| {
            Ok(serde_json::Value::Null)
        });
        assert!(!response.ok, "a closed child seam no longer serves");
    }

    #[test]
    fn a_child_requires_a_serving_parent_with_a_live_engine() {
        let factory = Live::default();
        let mut registry = registry();
        let parent = open_window(&mut registry, &factory);
        let engine = factory.created()[0].clone();
        let request = child_request("https://app.example.com/child");

        registry
            .get_mut(parent)
            .expect("registered")
            .lifecycle
            .transition(HostState::Suspended)
            .expect("the parent can suspend");
        assert!(
            registry
                .create_child_webview(parent, &request)
                .expect("open parent")
                .is_err()
        );
        assert!(engine.child_creations().is_empty());

        registry
            .get_mut(parent)
            .expect("registered")
            .lifecycle
            .transition(HostState::Running)
            .expect("the parent can resume");
        engine.kill();
        assert!(
            registry
                .create_child_webview(parent, &request)
                .expect("open parent")
                .is_err()
        );
        assert!(engine.child_creations().is_empty());
    }
}
