//! Window management, the first half of the application surface.
//!
//! ADR-001 splits the system: the `WebView` owns the page, NC-UI owns the runtime around it.
//! This module is the part of that split which decides *which windows exist*. It does not
//! own an engine — it holds a [`ncui_webview::WindowRegistry`] driven through the
//! [`ncui_webview::HostFactory`] port, so a real embedding layer (`ncui-webview-wry`) plugs
//! in at the seam rather than being special-cased here.
//!
//! The registry is inside a `Mutex` because the JSON-RPC surface is reached from several
//! connections at once (see [`crate::transport`]) and two `ncui.window.create` calls racing
//! must not both believe they opened the first window.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use ncui_core::{Capability, CapabilitySet, CorrelationId, WindowId};
use ncui_security::audit::{AuditEvent, AuditOutcome};
use ncui_webview::{
    ActionTable, HostFactory, HostSeam, MountedComponent, PageMessage, SeamError, WindowRegistry,
    WindowRejection, WindowRequest, WindowSummary,
};

use crate::Runtime;
use crate::commands::CommandTarget;

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

/// The window surface available to agents.
#[derive(Default)]
pub struct Windows {
    registry: Mutex<Option<WindowRegistry>>,
    factory: Mutex<Option<Box<dyn HostFactory>>>,
    manifests: Mutex<Vec<MountedComponent>>,
    /// Where the places of remembered windows are kept, when the runtime was told.
    ///
    /// A page names a window it opens — `remember: "notes"` — and the runtime writes down where
    /// that window was, so the next run can put it back. The name is the page's, because the page
    /// is what knows that two windows of one page are two windows.
    window_places: Mutex<Option<PathBuf>>,
    /// The places read from that file, and the ones written since.
    places: Mutex<BTreeMap<String, ncui_webview::WindowGeometry>>,
    /// Which open windows are remembered, and under which name.
    keys: Mutex<BTreeMap<WindowId, String>>,
    /// The displays the host reported when it was installed.
    ///
    /// Asked for once, at installation, because that is when the engine starts: a remembered
    /// window place has to be answerable *before* the first window exists, and the engine is the
    /// only thing that knows which displays this process can put a window on. Cached because a
    /// display list that changed mid-run is not a list this runtime acts on.
    monitors: Vec<ncui_webview::MonitorRect>,
}

impl fmt::Debug for Windows {
    /// Reports the surface's observable shape rather than dumping every mounted component,
    /// which would itself stall a surface being inspected.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Windows")
            .field("open", &self.len().unwrap_or(0))
            .field(
                "host_installed",
                &self.factory.lock().is_ok_and(|guard| guard.is_some()),
            )
            .finish_non_exhaustive()
    }
}

impl Windows {
    /// An empty surface. No registry is installed, so every window call is refused until
    /// one is — a runtime without a host must say so rather than inventing windows.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Installs a registry, which also installs the host that will open windows.
    ///
    /// A registry starts with an empty navigation allow-list and an empty page grant: both
    /// are widened deliberately by the caller, because a default that permits anything is
    /// the failure this project's capability module exists to prevent.
    #[must_use]
    pub fn with_registry(mut self, registry: WindowRegistry) -> Self {
        self.registry = Mutex::new(Some(registry));
        self
    }

    /// Installs the host factory that `open` uses to spawn engine hosts.
    ///
    /// A runtime without a factory refuses every `ncui.window.create` rather than inventing
    /// windows, so a factory is mandatory before the window surface can be used.
    #[must_use]
    pub fn with_factory(mut self, factory: Box<dyn HostFactory>) -> Self {
        self.factory = Mutex::new(Some(factory));
        self
    }

    /// Runs `body` against the registry, if one is installed.
    ///
    /// A poisoned lock is recovered rather than propagated: a panic in one request must not
    /// make every later window call fail with an opaque message, and the registry stays
    /// consistent because every mutation is a whole map operation.
    fn with_registry_mut<T>(
        &self,
        body: impl FnOnce(&mut WindowRegistry) -> Result<T, WindowRejection>,
    ) -> Result<T, WindowRejection> {
        let mut guard = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match guard.as_mut() {
            Some(registry) => body(registry),
            // Refusing rather than answering empty: an agent told "no windows are open"
            // when the truth is "this runtime has no host" would act on the wrong premise.
            None => Err(WindowRejection::EngineUnavailable {
                reason: Some(
                    "no webview host is installed; window operations are unavailable".to_owned(),
                ),
            }),
        }
    }

    /// Opens a window through the registry's own factory.
    ///
    /// The request carries the whole shape of the window, so an agent and an application
    /// ask for a window the same way and get the same window.
    pub fn open(
        &self,
        request: &WindowRequest,
        grant: Option<CapabilitySet>,
    ) -> Result<WindowId, WindowRejection> {
        let factory = self
            .factory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(factory) = factory.as_deref() else {
            return Err(WindowRejection::EngineUnavailable {
                reason: Some(
                    "no webview host is installed; window operations are unavailable".to_owned(),
                ),
            });
        };
        self.with_registry_mut(|registry| registry.open(factory, request, grant))
    }

    /// Lists every open window.
    pub fn summaries(&self) -> Result<Vec<WindowSummary>, WindowRejection> {
        self.with_registry_mut(|registry| Ok(registry.summaries()))
    }

    /// The displays the host reported when it was installed.
    ///
    /// An empty list is not the same as "one display at the origin": a caller that cannot learn
    /// which displays exist must not move a window, and an empty list is how it learns that.
    #[must_use]
    pub fn monitors(&self) -> Vec<ncui_webview::MonitorRect> {
        self.monitors.clone()
    }

    /// Tells the surface where the places of remembered windows live.
    ///
    /// The file is read here rather than at every lookup: a place is a preference, and a
    /// preference that is re-read from disk on every window creation is a disk access nobody
    /// asked for.
    pub fn remember_windows_in(&self, path: &Path) {
        let places = read_places(path);
        if let Ok(mut guard) = self.places.lock() {
            *guard = places;
        }
        if let Ok(mut guard) = self.window_places.lock() {
            *guard = Some(path.to_path_buf());
        }
    }

    /// Where a remembered window named `key` was, if it was remembered.
    ///
    /// The remembered place is clamped to a display that exists, for the reason the application's
    /// own window is: a place from a display that is gone is a window nobody can find.
    #[must_use]
    pub fn place_for(&self, key: &str) -> Option<ncui_webview::WindowGeometry> {
        let place = self
            .places
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(key)
            .copied()?;
        Some(place.clamped_to(&self.monitors))
    }

    /// Records that `window` is remembered under `key`.
    pub fn remember_place(&self, window: WindowId, key: &str) {
        self.keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(window, key.to_owned());
    }

    /// Writes down where every remembered window that is still open is.
    ///
    /// Called when a remembered window closes and when the application stops: a geometry has to be
    /// read while the window still exists, so this is the last moment it can be read at all. A
    /// window that has already gone is skipped rather than erased, because the place it had is the
    /// place the user chose.
    pub fn save_places(&self) {
        let mut changed = false;
        {
            let keys = self
                .keys
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Ok(mut places) = self.places.lock() else {
                return;
            };
            let Ok(registry) = self.registry.lock() else {
                return;
            };
            let Some(registry) = registry.as_ref() else {
                return;
            };
            for (window, key) in keys.iter() {
                let Some(geometry) = registry
                    .get(*window)
                    .and_then(ncui_webview::Window::geometry)
                else {
                    continue;
                };
                if !geometry.is_usable() {
                    continue;
                }
                if places.get(key) != Some(&geometry) {
                    places.insert(key.clone(), geometry);
                    changed = true;
                }
            }
        }
        if !changed {
            return;
        }
        let path = self
            .window_places
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(path) = path else {
            return;
        };
        let places = self
            .places
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Err(error) = write_places(&path, &places) {
            tracing::warn!(
                path = %path.display(),
                %error,
                "the remembered window places could not be written"
            );
        }
    }

    /// Forgets that a window was remembered, so its place is not written again after it is gone.
    fn forget_place(&self, window: WindowId) {
        self.keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&window);
    }

    /// Where a window is and how big it is, as its host reports it.
    ///
    /// `None` when the window is unknown, or when its host cannot report a geometry — the two are
    /// different questions and only the caller knows which it is asking, so the answer carries no
    /// reason.
    #[must_use]
    pub fn geometry(&self, id: WindowId) -> Option<ncui_webview::WindowGeometry> {
        let guard = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .as_ref()
            .and_then(|registry| registry.get(id))
            .and_then(ncui_webview::Window::geometry)
    }

    /// Closes a window, reporting whether it was open.
    pub fn close(&self, id: WindowId) -> Result<bool, WindowRejection> {
        // A window that is being closed is the last moment its geometry can be read, so a
        // remembered window's place is written down here rather than only when the application
        // stops — a window the user closed mid-run is a window whose place the user chose.
        self.save_places();
        self.forget_place(id);
        self.with_registry_mut(|registry| Ok(registry.close(id).is_some()))
    }

    /// How a window is being presented, as its host reports it.
    ///
    /// `None` when the window is unknown or its engine cannot ask the platform — the same two
    /// different questions the geometry answers, and only the caller knows which it is asking.
    #[must_use]
    pub fn window_state(&self, id: WindowId) -> Option<ncui_webview::WindowState> {
        let guard = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .as_ref()
            .and_then(|registry| registry.get(id))
            .and_then(ncui_webview::Window::window_state)
    }

    /// Whether the platform is drawing its interface light or dark.
    ///
    /// The user's setting for the whole machine, so a page asks and follows rather than choosing: a
    /// window that picked its own theme would disagree with every other one on the screen. `None`
    /// when the window is unknown or its engine cannot ask the platform — the same two questions the
    /// geometry and the presentation answer.
    #[must_use]
    pub fn window_theme(&self, id: WindowId) -> Option<ncui_webview::Theme> {
        let guard = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .as_ref()
            .and_then(|registry| registry.get(id))
            .and_then(ncui_webview::Window::window_theme)
    }

    /// Asks a window's platform for one change to how it is presented.
    ///
    /// Refuses an unknown window rather than answering `None`, because "this window does not
    /// exist" and "this window's engine cannot do that" are different facts and a caller that
    /// confused them would report a missing window as a platform limitation.
    pub fn set_window_state(
        &self,
        id: WindowId,
        change: ncui_webview::WindowChange,
    ) -> Result<Option<ncui_webview::WindowState>, WindowRejection> {
        self.with_registry_mut(|registry| {
            let window = registry
                .get(id)
                .ok_or_else(|| WindowRejection::EngineUnavailable {
                    reason: Some(format!("window {id} is not open")),
                })?;
            Ok(window.set_window_state(change))
        })
    }

    /// Asks a window's platform to make it take `shape`, recording what it became.
    ///
    /// The record is written only after the platform applied the shape, so `ncui.window.list`
    /// reports the title and size the window actually has rather than the ones that were asked for.
    pub fn set_window_shape(
        &self,
        id: WindowId,
        shape: &ncui_webview::WindowShape,
    ) -> Result<(), WindowRejection> {
        self.with_registry_mut(|registry| {
            let window =
                registry
                    .get_mut(id)
                    .ok_or_else(|| WindowRejection::EngineUnavailable {
                        reason: Some(format!("window {id} is not open")),
                    })?;
            if !window.set_window_shape(shape) {
                return Err(WindowRejection::EngineUnavailable {
                    reason: Some(format!("the engine for window {id} cannot reshape it")),
                });
            }
            window.record_shape(shape);
            Ok(())
        })
    }

    /// Shows the application's tray icon, or removes it, reporting whether the platform agreed.
    ///
    /// Asked through a window because the window is the handle to the engine that owns the
    /// platform's event loop, not because the tray belongs to it: the tray belongs to the
    /// application, and an application whose window is closed is still one a user may need to reach.
    ///
    /// # Errors
    ///
    /// Returns [`WindowRejection::EngineUnavailable`] when the window is not open, when this build
    /// cannot show a tray at all, or when the platform refused it.
    pub fn set_tray(
        &self,
        id: WindowId,
        tray: Option<ncui_webview::TrayRequest>,
    ) -> Result<(), WindowRejection> {
        self.with_registry_mut(|registry| {
            let window =
                registry
                    .get_mut(id)
                    .ok_or_else(|| WindowRejection::EngineUnavailable {
                        reason: Some(format!("window {id} is not open")),
                    })?;
            if !window.set_tray(tray) {
                return Err(WindowRejection::EngineUnavailable {
                    reason: Some(format!(
                        "the engine for window {id} cannot show a tray icon"
                    )),
                });
            }
            Ok(())
        })
    }

    /// Mounts a component into a window.
    ///
    /// Returns the window's component list when the window exists. Mounting into an unknown
    /// window is a refusal rather than a silent success, because an agent that believes a
    /// component mounted must be able to see it.
    pub fn mount(
        &self,
        window: WindowId,
        component: MountedComponent,
    ) -> Result<Vec<MountedComponent>, WindowRejection> {
        self.with_registry_mut(|registry| {
            let entry =
                registry
                    .get_mut(window)
                    .ok_or_else(|| WindowRejection::EngineUnavailable {
                        reason: Some(format!("window {window} is not open")),
                    })?;
            entry.mount(component.clone());
            // Record the mount in the window-manifest index so `ncui.components.list`
            // reports what the host mounted, not what a page claims. A remount replaces
            // the prior entry rather than duplicating it.
            let mut manifests = self
                .manifests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            manifests.retain(|c| c.id != component.id);
            manifests.push(component);
            Ok(entry.components.clone())
        })
    }

    /// Unmounts a component from a window.
    pub fn unmount(
        &self,
        window: WindowId,
        component: ncui_core::ComponentId,
    ) -> Result<bool, WindowRejection> {
        self.with_registry_mut(|registry| {
            let entry =
                registry
                    .get_mut(window)
                    .ok_or_else(|| WindowRejection::EngineUnavailable {
                        reason: Some(format!("window {window} is not open")),
                    })?;
            let removed = entry.unmount(component).is_some();
            if removed {
                let mut manifests = self
                    .manifests
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                manifests.retain(|c| c.id != component);
            }
            Ok(removed)
        })
    }

    /// Every component mounted in any window, each tagged with its window.
    pub fn components(&self) -> Result<Vec<ComponentEntry>, WindowRejection> {
        self.with_registry_mut(|registry| {
            let mut entries = Vec::new();
            for window in registry.all() {
                for component in &window.components {
                    entries.push(ComponentEntry {
                        window: window.id,
                        id: component.id,
                        name: component.name.clone(),
                        abi_version: component.abi_version.clone(),
                        granted: component.granted,
                    });
                }
            }
            Ok(entries)
        })
    }

    /// How many windows are open.
    pub fn len(&self) -> Result<usize, WindowRejection> {
        self.with_registry_mut(|registry| Ok(registry.len()))
    }

    /// Whether no windows are open.
    pub fn is_empty(&self) -> Result<bool, WindowRejection> {
        self.with_registry_mut(|registry| Ok(registry.is_empty()))
    }

    /// Every component recorded in the window-manifest index, cloned out.
    ///
    /// This is what `ncui.components.list` reports: the host's own record of mounted
    /// components, not a page's claim about them.
    pub fn manifests(&self) -> Result<Vec<MountedComponent>, WindowRejection> {
        let manifests = self
            .manifests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(manifests.clone())
    }

    /// Takes the next message a page has sent, from any window.
    ///
    /// Windows are visited in identity order so the order messages are served in does not
    /// depend on which engine happened to deliver first. A page cannot starve another by
    /// sending faster than the runtime drains.
    pub fn take_page_message(&self) -> Result<Option<(WindowId, PageMessage)>, WindowRejection> {
        self.with_registry_mut(|registry| {
            for window in registry.all_mut() {
                if let Some(message) = window.take_page_message() {
                    return Ok(Some((window.id, message)));
                }
            }
            Ok(None)
        })
    }

    /// Waits for a message from any window's page, up to `timeout`.
    ///
    /// The wait is the bridge's idle path: an application's page loop blocks here rather
    /// than sleeping, so a page's call is answered as soon as the engine delivers it instead
    /// of a frame later. A runtime with no host, or with no open windows, returns `None`.
    pub fn wait_page_message(&self, timeout: core::time::Duration) -> bool {
        self.with_registry_mut(|registry| Ok(registry.wait_page_message(timeout)))
            .unwrap_or(false)
    }

    /// The seam a window's page speaks through.
    ///
    /// Cloned out rather than borrowed, so the caller can dispatch through the runtime
    /// while holding it: the seam is policy, and policy does not change while a request
    /// is being served.
    pub fn seam_for(
        &self,
        window: WindowId,
    ) -> Result<Option<std::sync::Arc<HostSeam>>, WindowRejection> {
        self.with_registry_mut(|registry| {
            Ok(registry
                .get(window)
                .map(|entry| std::sync::Arc::clone(&entry.seam)))
        })
    }

    /// What a window's page is granted.
    pub fn grant_for(&self, window: WindowId) -> Result<Option<CapabilitySet>, WindowRejection> {
        self.with_registry_mut(|registry| {
            Ok(registry.get(window).map(|entry| entry.grant.granted()))
        })
    }

    /// Evaluates a script in a window's page.
    ///
    /// The host-to-page direction of the bridge. A window that is not open, or whose engine
    /// is dead, is a refusal rather than a success that never happened.
    pub fn post_to_page(&self, window: WindowId, script: &str) -> Result<bool, WindowRejection> {
        self.with_registry_mut(|registry| {
            let entry =
                registry
                    .get_mut(window)
                    .ok_or_else(|| WindowRejection::EngineUnavailable {
                        reason: Some(format!("window {window} is not open")),
                    })?;
            Ok(entry.post_to_page(script))
        })
    }
}

impl Runtime {
    /// Evaluates the messages the pages have sent.
    ///
    /// This is the page-to-host half of the bridge. A page's message is parsed by the seam,
    /// checked against the window's grant, and — if it is an action the host declared —
    /// dispatched through the *same* pipeline an agent's request goes through, so it is
    /// rate limited, authorized, consent-gated and audited identically.
    ///
    /// Returns how many messages were evaluated. A message that could not be parsed is
    /// counted and answered with a refusal; it is not an error, because a page is an
    /// untrusted input source and malformed input from one is expected.
    pub fn pump_pages(&mut self, now: core::time::Duration) -> usize {
        let mut handled = 0usize;
        while let Ok(Some((window, message))) = self.windows.take_page_message() {
            handled += 1;
            self.serve_page_message(window, &message.body, now);
        }
        handled
    }

    /// Evaluates one message from one page and answers it.
    fn serve_page_message(&mut self, window: WindowId, body: &str, now: core::time::Duration) {
        let Ok(Some(seam)) = self.windows.seam_for(window) else {
            return;
        };
        let Ok(Some(granted)) = self.windows.grant_for(window) else {
            return;
        };
        let response = match ncui_webview::SeamRequest::parse(body) {
            Ok(request) => {
                let mut response = seam.handle(&request.message, &mut |request, payload| {
                    self.dispatch_page_action(window, request, payload, granted, now)
                });
                response.id = Some(request.id);
                response
            }
            Err(refusal) => refusal,
        };
        // The answer is embedded as a JSON object literal. JSON escaping is a superset of
        // what a JavaScript string literal needs, and the bridge reads the object directly.
        let Ok(encoded) = serde_json::to_string(&response) else {
            tracing::warn!(?window, "the bridge response could not be serialised");
            return;
        };
        let script = format!("window.ncui && window.ncui.__dispatch({encoded});");
        if !self.windows.post_to_page(window, &script).unwrap_or(false) {
            tracing::warn!(
                ?window,
                "the bridge response could not be delivered to the page"
            );
        }
    }

    /// Dispatches a page's action through the runtime's own pipeline.
    fn dispatch_page_action(
        &mut self,
        window: WindowId,
        request: &ncui_webview::CapabilityRequest,
        payload: serde_json::Value,
        granted: CapabilitySet,
        now: core::time::Duration,
    ) -> Result<serde_json::Value, SeamError> {
        let Some(command) = self.page_commands.get(&request.action) else {
            // The seam already refuses an undeclared action; this arm exists so that a
            // mismatch between the seam's declarations and the runtime's table is a
            // refusal with a reason rather than a dispatch to a method nobody chose.
            return Err(SeamError::capability_denied(format!(
                "`{}` is not an action this host offers",
                request.action
            )));
        };
        if !payload.is_null() && !payload.is_object() {
            return Err(SeamError::new(
                "invalid_params",
                "an action payload must be a JSON object",
            ));
        }
        let mut params = if payload.is_null() {
            serde_json::json!({})
        } else {
            payload.clone()
        };
        if command.caller_window {
            // The page's own window, taken from the call's origin. A page that could name
            // a window could close one it does not own.
            if let Some(object) = params.as_object_mut() {
                object.insert(
                    "window".to_owned(),
                    serde_json::Value::String(window.to_string()),
                );
            }
        }
        match command.target {
            CommandTarget::RuntimeMethod(method) => {
                let mut session = crate::agent::AgentSession::for_page(
                    format!("page:{window}"),
                    granted,
                    self.page_limiter(),
                )
                .map_err(|error| seam_error(&error))?;
                let call = crate::rpc::Request::new(serde_json::Value::Null, method, params);
                match crate::dispatch::dispatch_with(
                    self,
                    &mut session,
                    &call,
                    now,
                    None,
                    crate::dispatch::method_table_cached(),
                ) {
                    Some(response) => match response.result {
                        Some(value) => Ok(value),
                        None => match response.error {
                            Some(error) => Err(SeamError {
                                code: error.data.ncui_code,
                                message: error.message,
                                retryable: error.data.retryable,
                            }),
                            None => Err(SeamError::dispatch("the action produced no result")),
                        },
                    },
                    None => Ok(serde_json::Value::Null),
                }
            }
            CommandTarget::Native(handler) => self.run_native_page_command(PageCall {
                window,
                name: &request.action,
                requires: command.requires,
                handler,
                payload,
                granted,
                now,
            }),
        }
    }

    /// Runs an application command through the same checks a runtime method receives.
    fn run_native_page_command(
        &mut self,
        call: PageCall<'_>,
    ) -> Result<serde_json::Value, SeamError> {
        let PageCall {
            window,
            name,
            requires,
            handler,
            payload,
            granted,
            now,
        } = call;
        let correlation = CorrelationId::new();
        let mut session = crate::agent::AgentSession::for_page(
            format!("page:{window}"),
            granted,
            self.page_limiter(),
        )
        .map_err(|error| seam_error(&error))?;
        let consent = payload
            .get("consent")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let outcome = (|| -> Result<serde_json::Value, SeamError> {
            session.rate_limit(now, 1).map_err(|e| seam_error(&e))?;
            session.authorize(requires).map_err(|e| seam_error(&e))?;
            session
                .require_consent(requires.has_destructive(), consent)
                .map_err(|e| seam_error(&e))?;
            handler(payload)
        })();
        let audit_outcome = match &outcome {
            Ok(_) => AuditOutcome::Allowed,
            Err(error) if error.code == "capability_denied" || error.code == "limit_exceeded" => {
                AuditOutcome::Denied
            }
            Err(_) => AuditOutcome::Failed,
        };
        if let Ok(event) = AuditEvent::new(
            SystemTime::now(),
            &session.agent_id,
            correlation,
            format!("page.{name}"),
            name,
            audit_outcome,
            requires,
        ) {
            self.audit.record(event);
        }
        outcome
    }
}

/// One page command call, gathered so the execution path takes one argument.
struct PageCall<'a> {
    /// The window whose page made the call.
    window: WindowId,
    /// The command name.
    name: &'a str,
    /// What the command requires of the page.
    requires: CapabilitySet,
    /// The command body.
    handler: crate::commands::CommandHandler,
    /// The page's payload.
    payload: serde_json::Value,
    /// What the page holds.
    granted: CapabilitySet,
    /// The clock the rate limiter reads.
    now: core::time::Duration,
}

fn seam_error(error: &ncui_core::NcError) -> SeamError {
    SeamError {
        code: error.code().to_owned(),
        message: error.to_string(),
        retryable: error.is_retryable(),
    }
}

/// The origins a hosted runtime loads by default.
///
/// The loopback names, and nothing else. A local development server is reached at
/// `https://localhost` or `https://127.0.0.1`; anything else has to be named explicitly,
/// because the allow-list is the boundary between "a page this runtime can load" and "a
/// page that can load this runtime".
#[must_use]
pub fn hosted_origins() -> Vec<ncui_webview::Origin> {
    vec![
        ncui_webview::Origin::new(ncui_webview::Scheme::Https, "localhost", None),
        ncui_webview::Origin::new(ncui_webview::Scheme::Https, "127.0.0.1", None),
    ]
}

/// Builds the registry a hosted runtime uses.
///
/// The registry declares what a page may do, and the grant is *derived* from those
/// declarations rather than written out separately — so a new action cannot be added
/// without deciding what it costs the page, and a page cannot be granted an authority no
/// action requires.
#[must_use]
pub fn hosted_registry(
    allowed_origins: &[ncui_webview::Origin],
    actions: &ActionTable,
    page_grant: CapabilitySet,
) -> WindowRegistry {
    // Loopback origins may be reached over plain `http:`. A loopback origin names this
    // machine, so the traffic never crosses a network, and a local development server
    // cannot offer `https` without a certificate the user has to trust. The allow-list is
    // unchanged: the origin still has to be named before a page may reach it.
    WindowRegistry::new()
        .with_default_navigation(
            ncui_webview::NavigationPolicy::new(allowed_origins).with_loopback_http(true),
        )
        .with_default_grant(ncui_webview::CapabilityGrant::from_set(page_grant))
        .with_action_table(actions.clone())
}

/// Builds the window surface a hosted runtime uses.
///
/// The registry, the policy and the engine are installed together, because a registry with
/// no factory can never open a window and a factory with no registry has nothing to open
/// one in. Either half alone would be a surface that reports something the runtime cannot
/// do.
#[cfg(feature = "wry-host")]
#[must_use]
pub fn hosted_surface(
    allowed_origins: &[ncui_webview::Origin],
    actions: &ActionTable,
    page_grant: CapabilitySet,
) -> Windows {
    let factory: Box<dyn HostFactory> = Box::new(ncui_webview_wry::WryHostFactory);
    // The displays are asked for here, because installing the host is what starts the engine: a
    // remembered window place has to be answerable before the first window exists, and only the
    // engine knows which displays this process can put a window on.
    let monitors = factory.monitors();
    let mut windows = Windows::new()
        .with_registry(hosted_registry(allowed_origins, actions, page_grant))
        .with_factory(factory);
    windows.monitors = monitors;
    windows
}

/// The places remembered for named windows, read from a file an application wrote.
///
/// A file that cannot be read is treated as no memory at all: refusing to open a window because
/// the file that remembers where windows were is damaged would turn a preference into a failure.
fn read_places(path: &Path) -> BTreeMap<String, ncui_webview::WindowGeometry> {
    let Ok(encoded) = std::fs::read(path) else {
        return BTreeMap::new();
    };
    match serde_json::from_slice(&encoded) {
        Ok(places) => places,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "the remembered window places could not be read, so there are none"
            );
            BTreeMap::new()
        }
    }
}

/// Writes the remembered places down.
fn write_places(
    path: &Path,
    places: &BTreeMap<String, ncui_webview::WindowGeometry>,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let encoded = serde_json::to_vec_pretty(places).map_err(std::io::Error::other)?;
    std::fs::write(path, encoded)
}

/// One component, tagged with the window that holds it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ComponentEntry {
    /// The window this component is mounted in.
    pub window: WindowId,
    /// The component's identity within that window.
    pub id: ncui_core::ComponentId,
    /// The component's name, as the host knows it.
    pub name: String,
    /// The negotiated ABI version, when one was agreed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abi_version: Option<String>,
    /// The capabilities the component was granted after narrowing.
    #[serde(default, skip_serializing_if = "CapabilitySet::is_empty")]
    pub granted: CapabilitySet,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ncui_core::{Capability, ComponentId};
    use ncui_webview::{MountedComponent, WindowRegistry};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A factory that counts its invocations, so a test can prove one window is one host.
    #[derive(Debug, Default)]
    struct Counting {
        spawned: Arc<AtomicUsize>,
    }

    impl Clone for Counting {
        fn clone(&self) -> Self {
            Self {
                spawned: Arc::clone(&self.spawned),
            }
        }
    }

    impl HostFactory for Counting {
        fn spawn(
            &self,
            _window: WindowId,
            _request: &WindowRequest,
            _seam: &Arc<HostSeam>,
        ) -> Option<Box<dyn ncui_webview::WebViewBackend>> {
            self.spawned.fetch_add(1, Ordering::Relaxed);
            Some(Box::new(LiveBackend))
        }
    }

    #[derive(Debug)]
    struct LiveBackend;

    impl ncui_webview::WebViewBackend for LiveBackend {
        fn last_error(&self) -> Option<String> {
            None
        }
        fn is_alive(&self) -> bool {
            true
        }
        fn poll_page_message(&self) -> Option<PageMessage> {
            None
        }
        fn evaluate_script(&self, _script: &str) -> bool {
            true
        }
    }

    /// A registry with one allowed origin, which every test window is opened at.
    ///
    /// A window's URL is vetted against the policy before the engine sees it, so a
    /// registry with no allow-list refuses every window — which is the correct default and
    /// useless for a test that is about something else.
    fn test_registry() -> WindowRegistry {
        WindowRegistry::new().with_default_navigation(ncui_webview::NavigationPolicy::new(&[
            ncui_webview::Origin::new(ncui_webview::Scheme::Https, "app.example.com", None),
        ]))
    }

    #[test]
    fn opening_a_window_without_a_registry_is_refused_rather_than_reporting_empty() {
        let windows = Windows::new();
        let error = windows
            .open(&WindowRequest::new("https://app.example.com"), None)
            .expect_err("a runtime with no host must not pretend to open a window");
        assert!(
            matches!(error, WindowRejection::EngineUnavailable { .. }),
            "got: {error}"
        );
        let listed = windows.summaries().expect_err("nothing to list");
        assert!(
            matches!(listed, WindowRejection::EngineUnavailable { .. }),
            "got: {listed}"
        );
        assert!(
            windows.components().is_err(),
            "an empty surface must not report an empty component list as if it were real"
        );
    }

    #[test]
    fn opening_a_window_with_a_factory_registers_exactly_one_window() {
        let factory = Counting::default();
        let windows = Windows::new()
            .with_registry(test_registry())
            .with_factory(Box::new(factory.clone()));
        let id = windows
            .open(&WindowRequest::new("https://app.example.com"), None)
            .expect("a live factory opens a window");
        assert_eq!(
            factory.spawned.load(Ordering::Relaxed),
            1,
            "one window is one engine"
        );
        let summaries = windows.summaries().expect("a registry is installed");
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, id);
        assert_eq!(windows.len().expect("installed"), 1);
    }

    #[test]
    fn closing_a_window_reports_whether_it_was_open() {
        let windows = Windows::new()
            .with_registry(test_registry())
            .with_factory(Box::new(Counting::default()));
        let id = windows
            .open(&WindowRequest::new("https://app.example.com"), None)
            .expect("opens");
        assert!(windows.close(id).expect("closes"));
        assert!(
            !windows.close(id).expect("the second close is a refusal"),
            "an agent must not be told it closed a window twice"
        );
        assert_eq!(windows.len().expect("installed"), 0);
    }

    #[test]
    fn a_mounted_component_is_listed_with_its_window() {
        let windows = Windows::new()
            .with_registry(test_registry())
            .with_factory(Box::new(Counting::default()));
        let id = windows
            .open(&WindowRequest::new("https://app.example.com"), None)
            .expect("opens");
        windows
            .mount(
                id,
                MountedComponent {
                    id: ComponentId::new(),
                    name: "chart".to_owned(),
                    abi_version: Some("1.0".to_owned()),
                    granted: CapabilitySet::empty().with(Capability::InspectRuntime),
                },
            )
            .expect("mounting into an open window works");
        let components = windows.components().expect("listed");
        assert_eq!(components.len(), 1, "the component is reported");
        assert_eq!(components[0].name, "chart");
        assert_eq!(components[0].window, id, "the component names its window");
        assert_eq!(
            components[0].abi_version.as_deref(),
            Some("1.0"),
            "the negotiated version travels with the component"
        );
    }

    #[test]
    fn mounting_into_an_unknown_window_is_a_refusal() {
        let windows = Windows::new()
            .with_registry(test_registry())
            .with_factory(Box::new(Counting::default()));
        let error = windows
            .mount(
                WindowId::new(),
                MountedComponent {
                    id: ComponentId::new(),
                    name: "chart".to_owned(),
                    abi_version: None,
                    granted: CapabilitySet::empty(),
                },
            )
            .expect_err("a component must not mount into a window that does not exist");
        assert!(
            error.to_string().contains("is not open"),
            "the refusal must name the window; got: {error}"
        );
    }

    #[test]
    fn unmounting_removes_the_component_and_reports_it() {
        let windows = Windows::new()
            .with_registry(test_registry())
            .with_factory(Box::new(Counting::default()));
        let id = windows
            .open(&WindowRequest::new("https://app.example.com"), None)
            .expect("opens");
        let component = ComponentId::new();
        windows
            .mount(
                id,
                MountedComponent {
                    id: component,
                    name: "chart".to_owned(),
                    abi_version: None,
                    granted: CapabilitySet::empty(),
                },
            )
            .expect("mounts");
        assert!(
            windows.unmount(id, component).expect("unmounts"),
            "the component was mounted, so unmounting it succeeds"
        );
        assert!(
            !windows
                .unmount(id, component)
                .expect("the second unmount is a refusal"),
            "an agent must not be told it unmounted what was already gone"
        );
        assert!(windows.components().expect("listed").is_empty());
    }

    #[test]
    fn the_same_component_mounted_twice_is_listed_once() {
        // A remount replaces rather than duplicates: an agent counting components must not
        // see one instance as two.
        let windows = Windows::new()
            .with_registry(test_registry())
            .with_factory(Box::new(Counting::default()));
        let id = windows
            .open(&WindowRequest::new("https://app.example.com"), None)
            .expect("opens");
        let component = ComponentId::new();
        for name in ["first", "second"] {
            windows
                .mount(
                    id,
                    MountedComponent {
                        id: component,
                        name: name.to_owned(),
                        abi_version: None,
                        granted: CapabilitySet::empty(),
                    },
                )
                .expect("mounts");
        }
        let components = windows.components().expect("listed");
        assert_eq!(components.len(), 1, "a remount replaces the entry");
        assert_eq!(components[0].name, "second", "the latest mount wins");
    }

    /// A backend that reports a fixed geometry, so a place can be read back off a window.
    #[derive(Debug)]
    struct PlacedBackend(ncui_webview::WindowGeometry);

    impl ncui_webview::WebViewBackend for PlacedBackend {
        fn last_error(&self) -> Option<String> {
            None
        }
        fn is_alive(&self) -> bool {
            true
        }
        fn poll_page_message(&self) -> Option<PageMessage> {
            None
        }
        fn evaluate_script(&self, _script: &str) -> bool {
            true
        }
        fn geometry(&self) -> Option<ncui_webview::WindowGeometry> {
            Some(self.0)
        }
    }

    /// A factory whose windows sit at one fixed place.
    #[derive(Debug, Clone, Copy)]
    struct Placed(ncui_webview::WindowGeometry);

    impl HostFactory for Placed {
        fn spawn(
            &self,
            _window: WindowId,
            _request: &WindowRequest,
            _seam: &Arc<HostSeam>,
        ) -> Option<Box<dyn ncui_webview::WebViewBackend>> {
            Some(Box::new(PlacedBackend(self.0)))
        }
    }

    /// A places file in the system's own scratch directory, which this test owns.
    fn scratch_places() -> PathBuf {
        use std::sync::atomic::AtomicU32;
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        std::env::temp_dir().join(format!(
            "ncui-window-places-{}-{}.json",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn a_remembered_window_is_put_back_where_it_was() {
        let path = scratch_places();
        std::fs::write(
            &path,
            br#"{"notes":{"x":120,"y":80,"width":640,"height":480}}"#,
        )
        .expect("the places file is written");
        let windows = Windows::new();
        windows.remember_windows_in(&path);
        assert_eq!(
            windows.place_for("notes"),
            Some(ncui_webview::WindowGeometry {
                x: 120,
                y: 80,
                width: 640,
                height: 480
            }),
            "a remembered place is what the page gets back"
        );
        assert_eq!(
            windows.place_for("unknown"),
            None,
            "a name nobody remembered is nothing, not a default"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_place_on_a_display_that_is_gone_is_moved_onto_one_that_exists() {
        let path = scratch_places();
        std::fs::write(
            &path,
            br#"{"notes":{"x":9000,"y":9000,"width":640,"height":480}}"#,
        )
        .expect("the places file is written");
        let mut windows = Windows::new();
        windows.monitors = vec![ncui_webview::MonitorRect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        }];
        windows.remember_windows_in(&path);
        let place = windows.place_for("notes").expect("the name was remembered");
        assert!(
            windows.monitors[0].contains(place.x, place.y),
            "a place from a display that is gone lands on one that exists: {place:?}"
        );
        assert_eq!(
            (place.width, place.height),
            (640, 480),
            "the remembered size is kept"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn closing_a_remembered_window_writes_its_place_down() {
        let path = scratch_places();
        let geometry = ncui_webview::WindowGeometry {
            x: 40,
            y: 60,
            width: 900,
            height: 700,
        };
        let windows = Windows::new()
            .with_registry(test_registry())
            .with_factory(Box::new(Placed(geometry)));
        windows.remember_windows_in(&path);
        let id = windows
            .open(&WindowRequest::new("https://app.example.com"), None)
            .expect("opens");
        windows.remember_place(id, "notes");
        assert!(windows.close(id).expect("closes"), "the window was open");
        let written: BTreeMap<String, ncui_webview::WindowGeometry> =
            serde_json::from_slice(&std::fs::read(&path).expect("the place was written"))
                .expect("the places file parses");
        assert_eq!(
            written.get("notes"),
            Some(&geometry),
            "a closed remembered window's place is what the next run opens at"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_window_that_is_not_a_place_is_not_written_down() {
        let path = scratch_places();
        let windows = Windows::new()
            .with_registry(test_registry())
            .with_factory(Box::new(Placed(ncui_webview::WindowGeometry {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            })));
        windows.remember_windows_in(&path);
        let id = windows
            .open(&WindowRequest::new("https://app.example.com"), None)
            .expect("opens");
        windows.remember_place(id, "notes");
        assert!(windows.close(id).expect("closes"));
        assert!(
            !path.exists(),
            "a geometry no window could have is not remembered at all"
        );
    }

    #[test]
    fn a_places_file_that_cannot_be_read_is_no_memory() {
        let path = scratch_places();
        std::fs::write(&path, b"not json").expect("a damaged file is written");
        let windows = Windows::new();
        windows.remember_windows_in(&path);
        assert_eq!(
            windows.place_for("notes"),
            None,
            "a damaged file means nothing was remembered, not that opening a window fails"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A backend that holds a presentation and applies changes to it, as a platform does.
    #[derive(Debug, Default, Clone)]
    struct Stateful {
        state: Arc<Mutex<ncui_webview::WindowState>>,
        /// The theme the platform reports, when it reports one.
        theme: Option<ncui_webview::Theme>,
    }

    impl ncui_webview::WebViewBackend for Stateful {
        fn last_error(&self) -> Option<String> {
            None
        }
        fn is_alive(&self) -> bool {
            true
        }
        fn poll_page_message(&self) -> Option<PageMessage> {
            None
        }
        fn evaluate_script(&self, _script: &str) -> bool {
            true
        }
        fn window_theme(&self) -> Option<ncui_webview::Theme> {
            self.theme
        }

        fn window_state(&self) -> Option<ncui_webview::WindowState> {
            Some(
                *self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            )
        }
        fn set_window_state(
            &self,
            change: ncui_webview::WindowChange,
        ) -> Option<ncui_webview::WindowState> {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match change {
                ncui_webview::WindowChange::Fullscreen(value) => state.fullscreen = value,
                ncui_webview::WindowChange::Maximized(value) => state.maximized = value,
                ncui_webview::WindowChange::Minimized(value) => state.minimized = value,
                ncui_webview::WindowChange::Visible(value) => state.visible = value,
                ncui_webview::WindowChange::Focus => state.focused = true,
            }
            Some(*state)
        }
        fn set_window_shape(&self, _shape: &ncui_webview::WindowShape) -> bool {
            true
        }
    }

    /// A factory whose windows all report one shared presentation.
    #[derive(Debug, Default, Clone)]
    struct StatefulFactory(Stateful);

    impl HostFactory for StatefulFactory {
        fn spawn(
            &self,
            _window: WindowId,
            _request: &WindowRequest,
            _seam: &Arc<HostSeam>,
        ) -> Option<Box<dyn ncui_webview::WebViewBackend>> {
            Some(Box::new(self.0.clone()))
        }
    }

    #[test]
    fn the_platforms_theme_is_reported_and_an_engine_that_cannot_say_says_so() {
        // The theme is the user's setting for the whole machine, so the runtime reports the
        // platform's answer rather than one of its own — and a backend that cannot ask reports
        // nothing, which is a different answer from "light".
        let dark = Stateful {
            theme: Some(ncui_webview::Theme::Dark),
            ..Stateful::default()
        };
        let windows = Windows::new()
            .with_registry(test_registry())
            .with_factory(Box::new(StatefulFactory(dark)));
        let id = windows
            .open(&WindowRequest::new("https://app.example.com"), None)
            .expect("opens");
        assert_eq!(windows.window_theme(id), Some(ncui_webview::Theme::Dark));

        // A backend that cannot report a theme is asked through the same window: the answer is
        // nothing rather than a guess, and a window that does not exist is the other `None`.
        let quiet = Stateful::default();
        let other = Windows::new()
            .with_registry(test_registry())
            .with_factory(Box::new(StatefulFactory(quiet)));
        let quiet_id = other
            .open(&WindowRequest::new("https://app.example.com"), None)
            .expect("opens");
        assert_eq!(other.window_theme(quiet_id), None);
        assert_eq!(other.window_theme(WindowId::new()), None);
    }

    #[test]
    fn a_window_reports_the_presentation_its_platform_reports() {
        let backend = Stateful::default();
        let windows = Windows::new()
            .with_registry(test_registry())
            .with_factory(Box::new(StatefulFactory(backend.clone())));
        let id = windows
            .open(&WindowRequest::new("https://app.example.com"), None)
            .expect("opens");
        assert_eq!(
            windows.window_state(id),
            Some(ncui_webview::WindowState::default()),
            "a window that was asked for nothing is windowed and unfocused"
        );
        // The platform changes it without the runtime asking: the user maximised the window by
        // hand. The runtime must report the platform's answer, not its own request.
        backend
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .maximized = true;
        assert!(
            windows
                .window_state(id)
                .is_some_and(|state| state.maximized),
            "the answer must be the platform's, not what the runtime last asked for"
        );
        // The frame is read the same way, and for the same reason: whether the platform is drawing a
        // title bar, and whether the window floats, are facts about the window rather than about the
        // last shape it was given. A runtime that reported its own memory here would tell a page to
        // draw a second title bar over the platform's.
        {
            let mut state = backend
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.decorated = true;
            state.always_on_top = true;
            state.resizable = false;
        }
        let reported = windows.window_state(id).expect("the engine can report");
        assert!(
            reported.decorated && reported.always_on_top && !reported.resizable,
            "the frame must be reported from the platform, got {reported:?}"
        );
    }

    #[test]
    fn changing_a_presentation_reports_the_answer_after_the_change() {
        let windows = Windows::new()
            .with_registry(test_registry())
            .with_factory(Box::new(StatefulFactory::default()));
        let id = windows
            .open(&WindowRequest::new("https://app.example.com"), None)
            .expect("opens");
        let state = windows
            .set_window_state(id, ncui_webview::WindowChange::Fullscreen(true))
            .expect("the window is open")
            .expect("the engine can make the change");
        assert!(state.fullscreen, "the answer is the state after the change");
        let state = windows
            .set_window_state(id, ncui_webview::WindowChange::Fullscreen(false))
            .expect("the window is open")
            .expect("the engine can make the change");
        assert!(!state.fullscreen, "and it can be given back");
        let state = windows
            .set_window_state(id, ncui_webview::WindowChange::Visible(false))
            .expect("the window is open")
            .expect("the engine can make the change");
        assert!(!state.visible, "a hidden window reports itself hidden");
        let state = windows
            .set_window_state(id, ncui_webview::WindowChange::Visible(true))
            .expect("the window is open")
            .expect("the engine can make the change");
        assert!(state.visible, "and a shown one reports itself shown");
    }

    #[test]
    fn a_window_whose_engine_cannot_report_its_presentation_says_so() {
        let windows = Windows::new()
            .with_registry(test_registry())
            .with_factory(Box::new(Counting::default()));
        let id = windows
            .open(&WindowRequest::new("https://app.example.com"), None)
            .expect("opens");
        assert_eq!(
            windows.window_state(id),
            None,
            "an engine that cannot ask the platform must not report a default as if it were the screen"
        );
        assert_eq!(
            windows
                .set_window_state(id, ncui_webview::WindowChange::Focus)
                .expect("the window is open"),
            None,
            "and a change it cannot make is `None`, not a state it invented"
        );
    }

    #[test]
    fn changing_the_presentation_of_an_unknown_window_is_a_refusal() {
        let windows = Windows::new()
            .with_registry(test_registry())
            .with_factory(Box::new(StatefulFactory::default()));
        let id = windows
            .open(&WindowRequest::new("https://app.example.com"), None)
            .expect("opens");
        assert!(windows.close(id).expect("closes"));
        let error = windows
            .set_window_state(id, ncui_webview::WindowChange::Focus)
            .expect_err("a closed window is not a window that cannot be focused");
        assert!(
            matches!(error, WindowRejection::EngineUnavailable { .. }),
            "got: {error}"
        );
    }

    #[test]
    fn reshaping_a_window_records_what_it_became() {
        let windows = Windows::new()
            .with_registry(test_registry())
            .with_factory(Box::new(StatefulFactory::default()));
        let id = windows
            .open(&WindowRequest::new("https://app.example.com"), None)
            .expect("opens");
        let shape = ncui_webview::WindowShape {
            title: Some("renamed".to_owned()),
            x: Some(40),
            y: Some(60),
            width: Some(900),
            height: Some(700),
            ..ncui_webview::WindowShape::default()
        };
        windows
            .set_window_shape(id, &shape)
            .expect("the window is open and its engine can reshape it");
        let summary = windows
            .summaries()
            .expect("the surface")
            .into_iter()
            .find(|summary| summary.id == id)
            .expect("the window");
        assert_eq!(summary.title, "renamed");
        assert_eq!((summary.x, summary.y), (Some(40), Some(60)));
        assert_eq!((summary.width, summary.height), (900, 700));
        // The fields the shape did not name are the ones the window was created with, not reset.
        assert!(!summary.always_on_top && summary.resizable);
    }

    #[test]
    fn reshaping_a_window_whose_engine_cannot_is_a_refusal() {
        let windows = Windows::new()
            .with_registry(test_registry())
            .with_factory(Box::new(Counting::default()));
        let id = windows
            .open(&WindowRequest::new("https://app.example.com"), None)
            .expect("opens");
        let error = windows
            .set_window_shape(
                id,
                &ncui_webview::WindowShape {
                    title: Some("renamed".to_owned()),
                    ..ncui_webview::WindowShape::default()
                },
            )
            .expect_err("an engine that cannot reshape a window must say so");
        assert!(
            matches!(error, WindowRejection::EngineUnavailable { .. }),
            "got: {error}"
        );
        let summary = windows
            .summaries()
            .expect("the surface")
            .into_iter()
            .find(|summary| summary.id == id)
            .expect("the window");
        assert_ne!(
            summary.title, "renamed",
            "a title the platform never took must not be reported as the window's"
        );
    }

    #[test]
    fn reshaping_an_unknown_window_is_a_refusal() {
        let windows = Windows::new()
            .with_registry(test_registry())
            .with_factory(Box::new(StatefulFactory::default()));
        let id = windows
            .open(&WindowRequest::new("https://app.example.com"), None)
            .expect("opens");
        assert!(windows.close(id).expect("closes"));
        let error = windows
            .set_window_shape(
                id,
                &ncui_webview::WindowShape {
                    resizable: Some(false),
                    ..ncui_webview::WindowShape::default()
                },
            )
            .expect_err("a closed window cannot be reshaped");
        assert!(
            matches!(error, WindowRejection::EngineUnavailable { .. }),
            "got: {error}"
        );
    }
}
