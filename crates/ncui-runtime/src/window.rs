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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ncui_core::{CapabilitySet, WindowId};
use ncui_webview::{
    ActionTable, ChildPageMessage, HostFactory, HostSeam, MountedComponent, PageMessage,
    PageTarget, TrustedServiceSeamFactory, WindowConfirmation, WindowRegistry, WindowRejection,
    WindowRequest, WindowSummary,
};

use crate::Runtime;

mod actions;
mod native;
mod page_dispatch;
pub use actions::{SEAM_ACTIONS, SeamAction, default_page_grant};
pub(crate) use page_dispatch::PendingPageConsent;

/// The window surface available to agents.
#[derive(Default)]
pub struct Windows {
    registry: Arc<Mutex<Option<WindowRegistry>>>,
    /// The next parent or child surface to poll, so a busy page cannot starve another.
    page_cursor: AtomicUsize,
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
    /// The cumulative dropped-event count each native window reported at its last drain.
    ///
    /// The backend's counter is cumulative and monotonic, while a drain's caller is owed
    /// what changed since it last asked: the delta is the difference, and this is the
    /// memory the difference is taken against. Forgotten when the window closes, because a
    /// window that is gone has no queue left to account for.
    native_drops_reported: Mutex<BTreeMap<WindowId, u64>>,
    /// The video surfaces every window owns, keyed inside the registry by window.
    ///
    /// Surfaces live here rather than in the registry entries because their lifecycle is
    /// narrower than the window's — a page tears down and recreates them freely — while
    /// their destruction is exactly as wide: no surface may outlive its window.
    video_surfaces: Arc<crate::video_surface::VideoSurfaces>,
    /// The audio streams every window owns, keyed inside the registry by window.
    ///
    /// Same lifecycle logic as video surfaces: streams die with their window because
    /// the page that mixed them is gone — audio playing after its window closed would
    /// be sound from nowhere.
    audio_streams: Arc<crate::audio::AudioStreams>,
    /// The live stream subscriptions every window owns, keyed inside the registry
    /// by window — same lifecycle logic as surfaces: a stream's events are for the
    /// page, and the page dying is the stream's end whether anyone said so or not.
    stream_subscriptions: Arc<crate::stream::StreamSubscriptions>,
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
        self.registry = Arc::new(Mutex::new(Some(registry)));
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
        // The drop cursor goes with the window, exactly as the remembered-place key does:
        // a window that is gone has no queue left to account for, and its id will never be
        // answered again, so the memory of what it last reported is dropped rather than kept.
        self.native_drops_reported
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        // A surface may not outlive the window its presenter is childed into: the child
        // dies with the parent either way, but the registry entry would linger presenting
        // into a dead handle — teardown is the window's business, not the page's memory.
        #[cfg(target_os = "windows")]
        self.video_surfaces.teardown_window(id);
        #[cfg(target_os = "windows")]
        self.audio_streams.teardown_window(id);
        self.stream_subscriptions.close_window(id);
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

    /// Attaches or removes one window's manifest-declared native menu.
    ///
    /// # Errors
    ///
    /// Returns [`WindowRejection::EngineUnavailable`] when the window is not open or its engine
    /// cannot attach the menu.
    pub fn set_menu(
        &self,
        id: WindowId,
        menu: Option<ncui_webview::MenuRequest>,
    ) -> Result<(), WindowRejection> {
        self.with_registry_mut(|registry| {
            let window =
                registry
                    .get_mut(id)
                    .ok_or_else(|| WindowRejection::EngineUnavailable {
                        reason: Some(format!("window {id} is not open")),
                    })?;
            if !window.set_menu(menu) {
                return Err(WindowRejection::EngineUnavailable {
                    reason: Some(format!(
                        "the engine for window {id} cannot attach a native menu"
                    )),
                });
            }
            Ok(())
        })
    }

    /// Takes the next native menu selection for one window.
    ///
    /// # Errors
    ///
    /// Returns [`WindowRejection::EngineUnavailable`] when the window is not open or no window
    /// registry is installed.
    pub fn take_menu_event(
        &self,
        id: WindowId,
    ) -> Result<Option<ncui_webview::MenuEvent>, WindowRejection> {
        self.with_registry_mut(|registry| {
            let window = registry
                .get(id)
                .ok_or_else(|| WindowRejection::EngineUnavailable {
                    reason: Some(format!("window {id} is not open")),
                })?;
            Ok(window.poll_menu_event())
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

    /// Takes one page message, rotating fairly across parents and children.
    pub fn take_page_target_message(
        &self,
    ) -> Result<Option<(PageTarget, String)>, WindowRejection> {
        self.with_registry_mut(|registry| {
            let surfaces: Vec<(WindowId, Option<ncui_webview_child::ChildWebviewId>)> = registry
                .all()
                .flat_map(|window| {
                    core::iter::once((window.id, None)).chain(
                        window
                            .child_ids()
                            .into_iter()
                            .map(Some)
                            .map(|child| (window.id, child)),
                    )
                })
                .collect();
            if surfaces.is_empty() {
                return Ok(None);
            }
            let start = self.page_cursor.fetch_add(1, Ordering::Relaxed) % surfaces.len();
            for offset in 0..surfaces.len() {
                let (parent, child) = surfaces[(start + offset) % surfaces.len()];
                let Some(window) = registry.get_mut(parent) else {
                    continue;
                };
                let body = match child {
                    None => window.take_page_message().map(|message| message.body),
                    Some(child) => window
                        .take_child_page_message(child)
                        .map(|message| message.body),
                };
                if let Some(body) = body {
                    let target = child.map_or(PageTarget::Window(parent), |child| {
                        PageTarget::Child { parent, child }
                    });
                    return Ok(Some((target, body)));
                }
            }
            Ok(None)
        })
    }

    /// Drains every page message currently queued for one window, in order.
    ///
    /// Part of the supersede machinery: the pump cannot know whether a keyed
    /// command is stale without looking at what is queued behind it, and the
    /// queue is the channel plus what the wait path already consumed. The
    /// drained messages are handed back through
    /// [`Windows::requeue_window_page_messages`] — nothing is ever dropped
    /// here, because a scan that eats messages is not a scan.
    pub fn drain_window_page_messages(&self, window: WindowId) -> Vec<PageMessage> {
        self.with_registry_mut(|registry| {
            let mut drained = Vec::new();
            if let Some(entry) = registry.get_mut(window) {
                while let Some(message) = entry.take_page_message() {
                    drained.push(message);
                }
            }
            Ok(drained)
        })
        .unwrap_or_default()
    }

    /// Returns drained page messages to one window's queue, in their original
    /// order.
    ///
    /// The drain empties the queue, so pushing back in pull order reconstructs
    /// it exactly; the scan these exist for must be invisible to every message
    /// it does not supersede.
    pub fn requeue_window_page_messages(&self, window: WindowId, messages: Vec<PageMessage>) {
        let _: Result<(), WindowRejection> = self.with_registry_mut(|registry| {
            if let Some(entry) = registry.get_mut(window) {
                for message in messages {
                    entry.push_page_message(message);
                }
            }
            Ok(())
        });
    }

    /// Takes the next message sent by any child page.
    pub fn take_child_page_message(
        &self,
    ) -> Result<Option<(WindowId, ChildPageMessage)>, WindowRejection> {
        self.with_registry_mut(|registry| Ok(registry.take_child_page_message()))
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
    pub fn seam_for(&self, window: WindowId) -> Result<Option<Arc<HostSeam>>, WindowRejection> {
        self.with_registry_mut(|registry| {
            Ok(registry.get(window).map(|entry| Arc::clone(&entry.seam)))
        })
    }

    /// The seam a particular window or child page speaks through.
    pub fn seam_for_target(
        &self,
        target: PageTarget,
    ) -> Result<Option<Arc<HostSeam>>, WindowRejection> {
        self.with_registry_mut(|registry| {
            let Some(entry) = registry.get(target.parent()) else {
                return Ok(None);
            };
            Ok(match target {
                PageTarget::Window(_) => Some(Arc::clone(&entry.seam)),
                PageTarget::Child { child, .. } => entry.seam_for_child(child),
            })
        })
    }

    /// What a window's page is granted.
    pub fn grant_for(&self, window: WindowId) -> Result<Option<CapabilitySet>, WindowRejection> {
        self.with_registry_mut(|registry| {
            Ok(registry.get(window).map(|entry| entry.grant.granted()))
        })
    }

    /// What a particular window or child page is granted.
    pub fn grant_for_target(
        &self,
        target: PageTarget,
    ) -> Result<Option<CapabilitySet>, WindowRejection> {
        Ok(self
            .seam_for_target(target)?
            .map(|seam| seam.grant().granted()))
    }

    /// The parented confirmation surface owned by `window`, when its backend can show one.
    ///
    /// The returned object is independent of the registry lock, so consent can block on a
    /// person without holding the window map or starving unrelated windows.
    #[must_use]
    pub fn confirmation_for(&self, window: WindowId) -> Option<Box<dyn WindowConfirmation>> {
        self.with_registry_mut(|registry| {
            Ok(registry
                .get(window)
                .and_then(|entry| entry.host.as_ref())
                .and_then(|host| host.confirmation()))
        })
        .ok()
        .flatten()
    }

    /// Evaluates a script in a window's page.
    ///
    /// The host-to-page direction of the bridge. A window that is not open, or whose engine
    /// is dead, is a refusal rather than a success that never happened.
    pub fn post_to_page(&self, window: WindowId, script: &str) -> Result<bool, WindowRejection> {
        self.post_to_target(PageTarget::Window(window), script)
    }

    /// Evaluates a script in a particular window or child page.
    pub fn post_to_target(
        &self,
        target: PageTarget,
        script: &str,
    ) -> Result<bool, WindowRejection> {
        self.with_registry_mut(|registry| {
            let entry = registry.get_mut(target.parent()).ok_or_else(|| {
                WindowRejection::EngineUnavailable {
                    reason: Some(format!("window {} is not open", target.parent())),
                }
            })?;
            Ok(match target {
                PageTarget::Window(_) => entry.post_to_page(script),
                PageTarget::Child { child, .. } => entry.post_to_child_page(child, script),
            })
        })
    }

    /// Asks one window's engine to start a platform drag.
    ///
    /// `Ok(false)` means the window is open but its engine cannot drag — a
    /// headless engine, or one whose platform refuses. The caller reports that
    /// rather than answering a drag that never began.
    pub fn drag_window(&self, window: WindowId) -> Result<bool, WindowRejection> {
        self.with_registry_mut(|registry| {
            let entry =
                registry
                    .get_mut(window)
                    .ok_or_else(|| WindowRejection::EngineUnavailable {
                        reason: Some(format!("window {window} is not open")),
                    })?;
            Ok(entry
                .host
                .as_ref()
                .is_some_and(|engine| engine.drag_window()))
        })
    }

    /// Asks one window's engine to start a platform edge-resize drag.
    ///
    /// The frameless-window complement of [`Windows::drag_window`]: the page's
    /// own border regions ask for the resize the OS border would have owned.
    /// `Ok(false)` keeps the honest refusal shape — open window, engine that
    /// cannot drag.
    pub fn drag_resize(
        &self,
        window: WindowId,
        edge: ncui_webview::ResizeEdge,
    ) -> Result<bool, WindowRejection> {
        self.with_registry_mut(|registry| {
            let entry =
                registry
                    .get_mut(window)
                    .ok_or_else(|| WindowRejection::EngineUnavailable {
                        reason: Some(format!("window {window} is not open")),
                    })?;
            Ok(entry
                .host
                .as_ref()
                .is_some_and(|engine| engine.drag_resize(edge)))
        })
    }

    /// Scales one window's page by `factor`, where `1.0` is the platform default.
    ///
    /// `Ok(false)` means the window is open but its engine cannot scale — a
    /// headless engine, or one whose platform refuses. The caller reports that
    /// rather than answering a zoom that never happened.
    pub fn set_zoom(&self, window: WindowId, factor: f64) -> Result<bool, WindowRejection> {
        self.with_registry_mut(|registry| {
            let entry =
                registry
                    .get_mut(window)
                    .ok_or_else(|| WindowRejection::EngineUnavailable {
                        reason: Some(format!("window {window} is not open")),
                    })?;
            Ok(entry
                .host
                .as_ref()
                .is_some_and(|engine| engine.set_zoom(factor)))
        })
    }

    /// Opens one window's platform web inspector, reporting whether the engine accepted.
    pub fn open_devtools(&self, window: WindowId) -> Result<bool, WindowRejection> {
        self.with_registry_mut(|registry| {
            let entry =
                registry
                    .get_mut(window)
                    .ok_or_else(|| WindowRejection::EngineUnavailable {
                        reason: Some(format!("window {window} is not open")),
                    })?;
            Ok(entry
                .host
                .as_ref()
                .is_some_and(|engine| engine.open_devtools()))
        })
    }

    /// Closes one window's platform web inspector, reporting whether the engine accepted.
    pub fn close_devtools(&self, window: WindowId) -> Result<bool, WindowRejection> {
        self.with_registry_mut(|registry| {
            let entry =
                registry
                    .get_mut(window)
                    .ok_or_else(|| WindowRejection::EngineUnavailable {
                        reason: Some(format!("window {window} is not open")),
                    })?;
            Ok(entry
                .host
                .as_ref()
                .is_some_and(|engine| engine.close_devtools()))
        })
    }

    /// Whether one window's platform web inspector is open.
    ///
    /// `Ok(None)` means the window is open but its engine cannot say — a platform that
    /// opens the inspector without a state query. Reported as unknown, not as closed.
    pub fn is_devtools_open(&self, window: WindowId) -> Result<Option<bool>, WindowRejection> {
        self.with_registry_mut(|registry| {
            let entry = registry
                .get(window)
                .ok_or_else(|| WindowRejection::EngineUnavailable {
                    reason: Some(format!("window {window} is not open")),
                })?;
            Ok(entry
                .host
                .as_ref()
                .and_then(|engine| engine.is_devtools_open()))
        })
    }

    /// Embeds a child webview into `parent`'s frame (ADR-017 Phase 3).
    ///
    /// The request is validated by the child crate before the engine is asked; the
    /// engine builds the surface on its own thread. The child is recorded with its
    /// parent's identity, so a page can move or close it later and enumeration can
    /// show the tree.
    pub fn create_child_webview(
        &self,
        parent: WindowId,
        request: &ncui_webview_child::ChildWebviewRequest,
    ) -> Result<Result<ncui_webview_child::ChildWebviewId, String>, WindowRejection> {
        self.with_registry_mut(|registry| registry.create_child_webview(parent, request))
    }

    /// Moves a live child webview to a new rectangle.
    pub fn set_child_webview_bounds(
        &self,
        parent: WindowId,
        child: ncui_webview_child::ChildWebviewId,
        request: &ncui_webview_child::ChildWebviewRequest,
    ) -> Result<Result<(), String>, WindowRejection> {
        self.with_registry_mut(|registry| registry.set_child_webview_bounds(parent, child, request))
    }

    /// Closes a live child webview.
    pub fn close_child_webview(
        &self,
        parent: WindowId,
        child: ncui_webview_child::ChildWebviewId,
    ) -> Result<Result<(), String>, WindowRejection> {
        self.with_registry_mut(|registry| registry.close_child_webview(parent, child))
    }

    /// The platform-native handle of a window, when its backend can say.
    ///
    /// Video surfaces child into the host window at platform level; this is the truth they
    /// child into. `None` covers both unknown windows and backends without a handle the
    /// presenter understands — the registry's typed refusal names which, because a page
    /// debugging the difference should not have to guess.
    #[must_use]
    pub fn native_window_handle(&self, window: WindowId) -> Option<Option<isize>> {
        self.with_registry_mut(|registry| {
            Ok(registry.get(window).map(|entry| {
                entry
                    .host
                    .as_ref()
                    .and_then(|host| host.native_window_handle())
            }))
        })
        .ok()
        .flatten()
    }

    /// The window's video surface registry — the surface operations the dispatch layer
    /// drives when a page or an agent asks.
    #[cfg(target_os = "windows")]
    pub(crate) fn video_surfaces(&self) -> &crate::video_surface::VideoSurfaces {
        &self.video_surfaces
    }

    /// The producer's own handle onto the registry: cloneable, independent of
    /// any lock the runtime or the pump holds, because frames are the hottest
    /// path in the system and must never convoy behind page traffic.
    #[cfg(target_os = "windows")]
    pub(crate) fn video_surface_producer(&self) -> crate::video_surface::VideoSurfaceProducer {
        crate::video_surface::VideoSurfaceProducer::new(Arc::clone(&self.video_surfaces))
    }

    /// The shared window-registry handle the stream sinks post through — the
    /// convoy position documented in the stream module: interaction-rate events
    /// wait on the same exclusion the pump holds, and that is deliberate.
    #[allow(
        dead_code,
        reason = "the streaming command path reaches the registry through this accessor; wired as the seam lands"
    )]
    pub(crate) fn registry_handle(&self) -> Arc<Mutex<Option<WindowRegistry>>> {
        Arc::clone(&self.registry)
    }

    /// The stream subscription registry — where streaming commands open and
    /// cancel subscriptions.
    pub(crate) fn stream_subscriptions(&self) -> &crate::stream::StreamSubscriptions {
        &self.stream_subscriptions
    }

    /// Builds the lock-free factory a streaming command captures: subscriptions
    /// plus the shared registry, never the runtime itself.
    #[must_use]
    pub(crate) fn stream_factory(&self) -> crate::stream::StreamFactory {
        crate::stream::StreamFactory::new(
            (*self.stream_subscriptions).clone(),
            Arc::clone(&self.registry),
        )
    }

    /// The window's audio stream registry — the stream operations the dispatch layer
    /// drives when a page or an agent asks.
    #[cfg(target_os = "windows")]
    #[allow(
        dead_code,
        reason = "the dispatch layer reaches the registry through this accessor; wired as the seam lands"
    )]
    pub(crate) fn audio_streams(&self) -> &crate::audio::AudioStreams {
        &self.audio_streams
    }

    /// The producer's own handle onto the audio registry: cloneable, independent of
    /// any lock the runtime or the pump holds — the same convoy lesson as video.
    #[cfg(target_os = "windows")]
    pub(crate) fn audio_stream_producer(&self) -> crate::audio::AudioStreamProducer {
        crate::audio::AudioStreamProducer::new(Arc::clone(&self.audio_streams))
    }

    /// Lists every live child webview across open windows.
    #[must_use]
    pub fn list_child_webviews(&self) -> Vec<ncui_webview_child::ChildWebviewEntry> {
        self.with_registry_mut(|registry| Ok(registry.list_child_webviews()))
            .unwrap_or_default()
    }

    /// Sends one CDP command to a window's own `DevTools` endpoint (ADR-017 Phase 2).
    ///
    /// The window's engine was created with remote debugging enabled and the endpoint it
    /// opened is published in the window's user-data directory; this asks the engine to
    /// carry the command there. An engine without the transport — a headless fallback, or
    /// a build whose platform does not expose the port — refuses by name rather than
    /// answering a delivery that never happened.
    pub fn cdp_send(
        &self,
        window: WindowId,
        command: &serde_json::Value,
    ) -> Result<Result<serde_json::Value, ncui_webview::CdpError>, WindowRejection> {
        self.with_registry_mut(|registry| {
            let entry = registry
                .get(window)
                .ok_or_else(|| WindowRejection::EngineUnavailable {
                    reason: Some(format!("window {window} is not open")),
                })?;
            // A registry entry without a backend still has a typed refusal, rather than a
            // string that dispatch would have to classify by parsing its wording.
            Ok(match entry.host.as_ref() {
                Some(backend) => backend.cdp_send(command),
                None => Err(ncui_webview::CdpError::UnsupportedPlatform {
                    reason: "this window's engine has no CDP transport".to_owned(),
                }),
            })
        })
    }

    /// Evaluates a script in every open window's page, counting the deliveries.
    pub fn post_to_all_pages(&self, script: &str) -> Result<usize, WindowRejection> {
        self.with_registry_mut(|registry| {
            let mut delivered = 0;
            for entry in registry.all_mut() {
                if entry.post_to_page(script) {
                    delivered += 1;
                }
            }
            Ok(delivered)
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
    /// Embeds a child webview into `parent`'s frame (ADR-017 Phase 3).
    pub fn create_child_webview(
        &self,
        parent: WindowId,
        request: &ncui_webview_child::ChildWebviewRequest,
    ) -> Result<Result<ncui_webview_child::ChildWebviewId, String>, WindowRejection> {
        self.windows.create_child_webview(parent, request)
    }

    /// Moves a live child webview to a new rectangle.
    pub fn set_child_webview_bounds(
        &self,
        parent: WindowId,
        child: ncui_webview_child::ChildWebviewId,
        request: &ncui_webview_child::ChildWebviewRequest,
    ) -> Result<Result<(), String>, WindowRejection> {
        self.windows
            .set_child_webview_bounds(parent, child, request)
    }

    /// Closes a live child webview.
    pub fn close_child_webview(
        &self,
        parent: WindowId,
        child: ncui_webview_child::ChildWebviewId,
    ) -> Result<Result<(), String>, WindowRejection> {
        self.windows.close_child_webview(parent, child)
    }

    /// Lists every live child webview across open windows.
    #[must_use]
    pub fn list_child_webviews(&self) -> Vec<ncui_webview_child::ChildWebviewEntry> {
        self.windows.list_child_webviews()
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
    hosted_registry_with_services(allowed_origins, actions, page_grant, None)
}

/// Builds the registry a hosted runtime uses, with the native-view service channel installed.
///
/// The service factory is optional because the contract belongs to the window layer: tests
/// and embeddings that only exercise pages should not have to know native services exist.
/// A production runtime installs one so a native window is born with its own narrowed
/// authority already closed over.
#[must_use]
pub fn hosted_registry_with_services(
    allowed_origins: &[ncui_webview::Origin],
    actions: &ActionTable,
    page_grant: CapabilitySet,
    service_factory: Option<TrustedServiceSeamFactory>,
) -> WindowRegistry {
    // Loopback origins may be reached over plain `http:`. A loopback origin names this
    // machine, so the traffic never crosses a network, and a local development server
    // cannot offer `https` without a certificate the user has to trust. The allow-list is
    // unchanged: the origin still has to be named before a page may reach it.
    // Every window's page is told the platform's locale and the direction it lays out in, so a
    // page matches its formatting and text direction to the machine without keeping its own
    // table of right-to-left scripts. The direction is derived from the locale, so the pair
    // cannot disagree.
    let locale = crate::describe::platform_locale();
    let direction = ncui_a11y::TextDirection::for_locale(&locale).as_str();
    let registry = WindowRegistry::new()
        .with_default_navigation(
            ncui_webview::NavigationPolicy::new(allowed_origins).with_loopback_http(true),
        )
        .with_default_grant(ncui_webview::CapabilityGrant::from_set(page_grant))
        .with_action_table(actions.clone())
        .with_default_locale(locale, direction);
    match service_factory {
        Some(factory) => registry.with_service_seam_factory(factory),
        None => registry,
    }
}

/// Builds the window surface a hosted runtime uses, over the caller's host factory.
///
/// The registry, the policy and the engine are installed together, because a registry with
/// no factory can never open a window and a factory with no registry has nothing to open
/// one in. Either half alone would be a surface that reports something the runtime cannot
/// do. The factory is the caller's because which host a window opens in is an embedding
/// decision — the runtime wires the policy around whatever host the embedding chooses,
/// and a routing factory carrying several hosts is exactly as welcome as a single one.
#[must_use]
pub fn hosted_surface_with(
    factory: Box<dyn HostFactory>,
    allowed_origins: &[ncui_webview::Origin],
    actions: &ActionTable,
    page_grant: CapabilitySet,
) -> Windows {
    hosted_surface_with_services(factory, allowed_origins, actions, page_grant, None)
}

/// Builds the window surface a hosted runtime uses, with native-view services installed.
///
/// This is the production-shape constructor: the page policy, the native service channel
/// and the host are installed together so no window can be born before the authority it
/// will carry is decided.
#[must_use]
pub fn hosted_surface_with_services(
    factory: Box<dyn HostFactory>,
    allowed_origins: &[ncui_webview::Origin],
    actions: &ActionTable,
    page_grant: CapabilitySet,
    service_factory: Option<TrustedServiceSeamFactory>,
) -> Windows {
    // The displays are asked for here, because installing the host is what starts the engine: a
    // remembered window place has to be answerable before the first window exists, and only the
    // engine knows which displays this process can put a window on.
    let monitors = factory.monitors();
    let mut windows = Windows::new()
        .with_registry(hosted_registry_with_services(
            allowed_origins,
            actions,
            page_grant,
            service_factory,
        ))
        .with_factory(factory);
    windows.monitors = monitors;
    windows
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
    hosted_surface_with(factory, allowed_origins, actions, page_grant)
}

/// Runs the window engine's event loop on the main thread, with the application's
/// work on a worker thread.
///
/// macOS only: `AppKit` requires the event loop on the main thread and `tao` offers
/// no any-thread switch there, so the engine loop and the runtime pump exchange
/// the threads they use on the other platforms (ADR-018). The process ends with
/// the code the application's work finishes with.
#[cfg(all(target_os = "macos", feature = "wry-host"))]
pub fn run_engine_on_main_thread(app: impl FnOnce() -> i32 + Send + 'static) -> ! {
    ncui_webview_wry::host::run_on_main_thread(app)
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
    use super::page_dispatch::probe_supersede_key;
    use super::*;
    use ncui_core::{Capability, CapabilitySet, ComponentId, Limits};
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
        fn as_any(&self) -> &dyn core::any::Any {
            self
        }

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

    /// What a native test view and the test staging it both see: the events the view has
    /// queued and the cumulative count of drops a full queue would have lost, which the
    /// drain is owed as a delta rather than restated.
    #[derive(Debug, Default)]
    struct NativeViewFixture {
        events: Mutex<std::collections::VecDeque<ncui_webview::NativeViewEvent>>,
        dropped: Mutex<u64>,
    }

    /// A native test view: a working event queue over the fixture, so a drain driven
    /// against it answers what the contract promises rather than an empty invention.
    #[derive(Debug)]
    struct NativeBackend {
        fixture: Arc<NativeViewFixture>,
    }

    impl ncui_webview::NativeViewBackend for NativeBackend {
        fn poll_native_event(&self) -> Option<ncui_webview::NativeViewEvent> {
            self.fixture
                .events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
        }

        fn send_view_command(
            &self,
            _command: ncui_webview::NativeViewCommand,
        ) -> Result<(), ncui_webview::NativeViewError> {
            Ok(())
        }

        fn dropped_native_events(&self) -> u64 {
            *self
                .fixture
                .dropped
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }

    impl ncui_webview::WebViewBackend for NativeBackend {
        fn as_any(&self) -> &dyn core::any::Any {
            self
        }

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
        fn native_view(&self) -> Option<&dyn ncui_webview::NativeViewBackend> {
            Some(self)
        }
    }

    /// The factory a native-window test installs: every window it spawns carries the view
    /// the fixture stages.
    #[derive(Debug)]
    struct NativeFactory {
        fixture: Arc<NativeViewFixture>,
    }

    impl HostFactory for NativeFactory {
        fn spawn(
            &self,
            _window: WindowId,
            _request: &WindowRequest,
            _seam: &Arc<HostSeam>,
        ) -> Option<Box<dyn ncui_webview::WebViewBackend>> {
            Some(Box::new(NativeBackend {
                fixture: Arc::clone(&self.fixture),
            }))
        }
    }

    /// A window surface with one native window open, and the fixture its view is staged through.
    fn native_window() -> (Windows, Arc<NativeViewFixture>, WindowId) {
        let fixture = Arc::new(NativeViewFixture::default());
        let windows = Windows::new()
            .with_registry(test_registry())
            .with_factory(Box::new(NativeFactory {
                fixture: Arc::clone(&fixture),
            }));
        let window = windows
            .open(&WindowRequest::native("test.panel"), None)
            .expect("the native window opens");
        (windows, fixture, window)
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
    fn a_native_drain_answers_everything_queued_in_the_order_it_was_emitted() {
        let (windows, fixture, window) = native_window();
        fixture
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend([
                ncui_webview::NativeViewEvent::new("test.panel", "clicked"),
                ncui_webview::NativeViewEvent::new("test.panel", "changed"),
            ]);

        let (events, dropped) = windows
            .drain_native_events(window)
            .expect("an open window is not a refusal")
            .expect("a native window has a queue to drain");
        assert_eq!(
            events,
            vec![
                ncui_webview::NativeViewEvent::new("test.panel", "clicked"),
                ncui_webview::NativeViewEvent::new("test.panel", "changed"),
            ],
            "the drain carries everything queued, in order"
        );
        assert_eq!(dropped, 0);

        // The queue drained is honestly empty rather than a refusal: an empty answer is
        // what "nothing since last time" looks like.
        let (events, dropped) = windows
            .drain_native_events(window)
            .expect("still open")
            .expect("still native");
        assert!(events.is_empty(), "the queue was drained: {events:?}");
        assert_eq!(dropped, 0);
    }

    #[test]
    fn a_native_drain_is_owed_the_drops_since_the_last_drain_and_no_more() {
        let (windows, fixture, window) = native_window();

        // The backend's counter is cumulative: the first drain is owed everything lost
        // since the window opened.
        *fixture
            .dropped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = 5;
        let (_, dropped) = windows
            .drain_native_events(window)
            .expect("open")
            .expect("native");
        assert_eq!(dropped, 5, "the first drain is owed the whole count");

        // Two more drops since — the second drain is owed the delta, because one loss
        // counted twice is a metric that lies.
        *fixture
            .dropped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = 7;
        let (_, dropped) = windows
            .drain_native_events(window)
            .expect("open")
            .expect("native");
        assert_eq!(dropped, 2, "the second drain is owed only what is new");

        // A quiet interval is owed zero rather than the count restated.
        let (_, dropped) = windows
            .drain_native_events(window)
            .expect("open")
            .expect("native");
        assert_eq!(dropped, 0);
    }

    #[test]
    fn draining_a_page_window_is_no_answer_rather_than_an_empty_one() {
        let windows = Windows::new()
            .with_registry(test_registry())
            .with_factory(Box::new(Counting::default()));
        let page = windows
            .open(&WindowRequest::new("https://app.example.com"), None)
            .expect("opens");

        let drained = windows
            .drain_native_events(page)
            .expect("an open window is not a refusal");
        assert!(
            drained.is_none(),
            "a page window has no queue to drain, and the answer says so"
        );
    }

    #[test]
    fn draining_a_window_that_is_not_open_is_a_refusal() {
        let (windows, _fixture, window) = native_window();
        windows.close(window).expect("closes");

        let error = windows
            .drain_native_events(window)
            .expect_err("a closed window has no queue at all");
        assert!(
            matches!(error, WindowRejection::EngineUnavailable { .. }),
            "got: {error}"
        );
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
        fn as_any(&self) -> &dyn core::any::Any {
            self
        }

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
        fn as_any(&self) -> &dyn core::any::Any {
            self
        }

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
    #[test]
    fn the_supersede_probe_reads_only_keyed_performs() {
        let keyed =
            r#"{"id":"p1-x","action":"perform","name":"playhead_move","supersede":"playhead"}"#;
        let (id, key) = probe_supersede_key(keyed).expect("a keyed perform probes");
        assert_eq!(id, "p1-x");
        assert_eq!(key, "playhead");
        // An unkeyed perform does not participate — the default is safety.
        let plain = r#"{"id":"p2-x","action":"perform","name":"playhead_move"}"#;
        assert!(probe_supersede_key(plain).is_none());
        // Other actions never participate, key or no key.
        let probe = r#"{"id":"p3-x","action":"probe_navigation","supersede":"playhead"}"#;
        assert!(probe_supersede_key(probe).is_none());
        // Abuse shapes do not participate: empty, over-long, non-ASCII-printable.
        let empty = r#"{"id":"p4-x","action":"perform","supersede":""}"#;
        assert!(probe_supersede_key(empty).is_none());
        let oversized = format!(
            r#"{{"id":"p5-x","action":"perform","supersede":"{}"}}"#,
            "k".repeat(300)
        );
        assert!(probe_supersede_key(&oversized).is_none());
        // Malformed JSON is not a keyed perform, merely malformed.
        assert!(probe_supersede_key("{not json").is_none());
        // An id-less message cannot be answered, so it cannot be superseded —
        // serving it and letting the seam refuse is the honest path.
        let no_id = r#"{"action":"perform","supersede":"playhead"}"#;
        assert!(probe_supersede_key(no_id).is_none());
    }

    /// A page-message fixture the pump evidence is staged through: the page's
    /// scripted messages in, every script the host was asked to evaluate out.
    /// The evaluate capture is the response path — a seam answer is a posted
    /// script, so what the page would settle is readable here exactly.
    #[derive(Debug, Default)]
    struct PageFixture {
        messages: Mutex<std::collections::VecDeque<PageMessage>>,
        scripts: Mutex<Vec<String>>,
    }

    #[derive(Debug)]
    struct PageBackend {
        fixture: Arc<PageFixture>,
    }

    impl ncui_webview::WebViewBackend for PageBackend {
        fn as_any(&self) -> &dyn core::any::Any {
            self
        }
        fn last_error(&self) -> Option<String> {
            None
        }
        fn is_alive(&self) -> bool {
            true
        }
        fn poll_page_message(&self) -> Option<PageMessage> {
            self.fixture
                .messages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
        }
        fn evaluate_script(&self, script: &str) -> bool {
            self.fixture
                .scripts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(script.to_owned());
            true
        }
    }

    #[derive(Debug)]
    struct PageFactory {
        fixture: Arc<PageFixture>,
    }

    impl HostFactory for PageFactory {
        fn spawn(
            &self,
            _window: WindowId,
            _request: &WindowRequest,
            _seam: &Arc<HostSeam>,
        ) -> Option<Box<dyn ncui_webview::WebViewBackend>> {
            Some(Box::new(PageBackend {
                fixture: Arc::clone(&self.fixture),
            }))
        }
        fn monitors(&self) -> Vec<ncui_webview::MonitorRect> {
            Vec::new()
        }
    }

    /// A runtime whose window surface opens queue-backed windows, with one
    /// native command registered whose executions are counted — the evidence
    /// that distinguishes "served" from "superseded before it ever ran".
    fn runtime_with_counting_echo() -> (Runtime, Arc<PageFixture>, Arc<AtomicUsize>) {
        let mut runtime = Runtime::start(Limits::default()).expect("runtime starts");
        let executions = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&executions);
        let registration = runtime
            .page_commands
            .register(
                "echo",
                CapabilitySet::empty(),
                "echoes the payload",
                move |payload| {
                    counted.fetch_add(1, Ordering::Relaxed);
                    Ok(serde_json::json!({ "echoed": payload }))
                },
            )
            .expect("the echo command registers");
        // The registration must outlive the test's pumps: forgetting is the
        // harness's honest answer — a registration dropped mid-pump would
        // unregister the command the evidence depends on.
        std::mem::forget(registration);
        let fixture = Arc::new(PageFixture::default());
        let actions = runtime.page_commands.actions();
        let grant = runtime.page_grant();
        let registry = hosted_registry(
            &[ncui_webview::Origin::new(
                ncui_webview::Scheme::Https,
                "app.example.com",
                None,
            )],
            &actions,
            grant,
        );
        let windows = Windows::new()
            .with_registry(registry)
            .with_factory(Box::new(PageFactory {
                fixture: Arc::clone(&fixture),
            }));
        runtime.windows = windows;
        (runtime, fixture, executions)
    }

    fn keyed_invoke(id: &str, position: u32) -> String {
        format!(
            r#"{{"id":"{id}","action":"perform","name":"echo","payload":{{"position_ms":{position}}},"supersede":"playhead"}}"#
        )
    }

    fn open_page_window(runtime: &Runtime) -> WindowId {
        runtime
            .windows
            .open(&WindowRequest::new("https://app.example.com"), None)
            .expect("the window opens")
    }

    fn stage(fixture: &Arc<PageFixture>, bodies: &[String]) {
        let mut queue = fixture
            .messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for body in bodies {
            queue.push_back(PageMessage::new(body.clone()));
        }
    }

    #[test]
    fn a_stale_keyed_command_is_answered_superseded_and_never_executes() {
        let (mut runtime, fixture, executions) = runtime_with_counting_echo();
        let _window = open_page_window(&runtime);
        // Three scrub positions queued before the pump runs: the first two are
        // stale the moment their successors exist.
        stage(
            &fixture,
            &[
                keyed_invoke("p1", 0),
                keyed_invoke("p2", 33),
                keyed_invoke("p3", 66),
            ],
        );
        let handled = runtime.pump_pages(core::time::Duration::from_millis(100));
        assert_eq!(handled, 3, "every message is answered, none vanish");
        assert_eq!(
            executions.load(Ordering::Relaxed),
            1,
            "only the newest position executes — the rest were stale before they ran"
        );
        let scripts = fixture
            .scripts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let joined = scripts.join("\n");
        let superseded = joined.matches("superseded").count();
        assert_eq!(
            superseded, 2,
            "two answers carry the superseded outcome: {joined}"
        );
        assert!(
            joined.contains(r#""id":"p1","ok":true,"outcome":"superseded""#),
            "p1 is answered superseded: {joined}"
        );
        assert!(
            joined.contains(r#""id":"p2","ok":true,"outcome":"superseded""#),
            "p2 is answered superseded: {joined}"
        );
        assert!(
            joined.contains(r#""id":"p3","ok":true,"outcome":"performed""#),
            "p3 is the one that executes: {joined}"
        );
    }

    #[test]
    fn unkeyed_and_other_keyed_messages_are_neither_superseded_nor_reordered() {
        let (mut runtime, fixture, executions) = runtime_with_counting_echo();
        let _window = open_page_window(&runtime);
        stage(&fixture, &[
            keyed_invoke("p1", 0),
            // A different key is a different stream: trimming clip-7 never
            // supersedes the playhead, and the playhead never supersedes it.
            r#"{"id":"t1","action":"perform","name":"echo","payload":{"position_ms":5},"supersede":"trim:clip-7"}"#.to_owned(),
            // An unkeyed command is never stale: the caller did not opt in.
            r#"{"id":"u1","action":"perform","name":"echo","payload":{"position_ms":7}}"#.to_owned(),
        ]);
        let handled = runtime.pump_pages(core::time::Duration::from_millis(100));
        assert_eq!(handled, 3);
        assert_eq!(
            executions.load(Ordering::Relaxed),
            3,
            "distinct keys and unkeyed commands all execute — supersede is opt-in per stream"
        );
        let scripts = fixture
            .scripts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            !scripts.join("\n").contains("superseded"),
            "nothing here is answered superseded"
        );
    }

    #[test]
    fn a_lone_keyed_command_executes_normally() {
        let (mut runtime, fixture, executions) = runtime_with_counting_echo();
        let _window = open_page_window(&runtime);
        stage(&fixture, &[keyed_invoke("p1", 0)]);
        runtime.pump_pages(core::time::Duration::from_millis(100));
        assert_eq!(
            executions.load(Ordering::Relaxed),
            1,
            "with nothing newer queued, the key costs nothing"
        );
    }
}
