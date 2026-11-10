//! Running an application: manifest, assets, window, and the page loop.
//!
//! An application is a directory with a manifest in it. This module is what turns that
//! directory into a running program: it reads the manifest, serves the application's
//! assets over loopback HTTP, opens a window at the entry page, and hands back something
//! the caller can pump.
//!
//! The pump is the part that is easy to leave out and impossible to do without. A page
//! that calls the runtime sends a message; the runtime answers it. Between the two there
//! is a loop — someone has to read the messages and dispatch them — and a framework that
//! opens a window without running that loop has opened a window whose native calls go
//! nowhere.
//!
//! So the loop is here, not in the caller: [`Application::pump`] serves whatever the pages
//! have sent, and [`Application::is_running`] reports whether the window is still there.
//! The CLI runs them in a loop; a test runs them until a condition holds.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ncui_app::{AppManifest, AssetServer, DirectoryWatcher};
use ncui_core::{ErrorKind, Limits, NcError, Result, WindowId};
use ncui_webview::{Origin, Scheme, WindowRequest};

use crate::Runtime;

/// A running application.
///
/// Owns the runtime, the asset server and the window, and is stopped explicitly. Dropping
/// the application tears the window down and stops the server; the order matters, because
/// a page that is still loading while its server has gone will fail its own fetches.
#[derive(Debug)]
pub struct Application {
    runtime: Runtime,
    server: AssetServer,
    window: WindowId,
    url: String,
    name: String,
    version: String,
    /// The watcher over the application's assets, when reload-on-change is on.
    watcher: Option<DirectoryWatcher>,
    /// When the watcher was last polled.
    ///
    /// The page loop runs every 16 ms; scanning a tree on each turn would spend more time
    /// looking for a change than the change is worth. Four scans a second is slower than a
    /// person can edit and faster than they can notice.
    last_watch_poll: Option<std::time::Instant>,
    /// How many times the page has been reloaded because a file changed.
    reloads: usize,
    /// The native commands registered for this application.
    ///
    /// Held rather than discarded, so shutting the application down unwinds them. A
    /// registration that is dropped without being disposed is a command the host still
    /// answers for a page that no longer exists.
    native_commands: Vec<crate::commands::CommandRegistration>,
    /// The component commands the manifest's grant covers, disposed on shutdown.
    component_commands: Vec<crate::commands::CommandRegistration>,
    /// The handle for the `app.link` command, disposed on shutdown.
    link_command: crate::commands::CommandRegistration,
    /// The handles for the state commands, disposed on shutdown.
    state_commands: Vec<crate::commands::CommandRegistration>,
    /// This process's claim to be the instance for this application, when it has one.
    ///
    /// Held for the application's lifetime: dropping it releases the lock, which is what makes
    /// the *next* launch the instance rather than a hand-off.
    instance: Option<ncui_native::InstanceGuard>,
    /// How many second launches this instance has received.
    handoffs: usize,
    /// Where this application's window state is remembered.
    window_state: PathBuf,
    /// The link this application was opened with, if it was opened by one.
    ///
    /// Shared with the `app.link` command, which is registered while the application is built: a
    /// page reads it through that command, because a deep link arrives as an argument and a page
    /// that cannot ask what it was has no way to act on the thing the user clicked.
    link: Arc<Mutex<Option<String>>>,
    /// The global shortcuts this application holds, when this build can hold any.
    #[cfg(feature = "global-shortcuts")]
    shortcuts: Option<HeldShortcuts>,
    /// How many asset responses have already been counted into the metrics registry.
    ///
    /// The server counts every response it sends; the registry is told about the ones
    /// that have not been reported yet, so the counter is a total rather than a rate that
    /// a restart would reset.
    reported_responses: u64,
}

impl Application {
    /// Launches the application in `directory`.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::Validation`] when the manifest is missing or unusable, when
    /// the entry page does not exist, or when this build has no window host — a runtime
    /// that could not open a window must say so rather than serve assets nobody sees.
    pub fn launch(directory: &Path, limits: Limits) -> Result<Self> {
        Self::launch_with(directory, limits, None, |_| Ok(()))
    }

    /// Launches the application in `directory`, after `setup` has prepared the runtime.
    ///
    /// The setup runs before the window is opened, which is when an application registers
    /// the native commands its page may call. A command registered after the window
    /// opened would be registered too late for the page's first call — and a page that
    /// calls a command the host has not declared yet is refused.
    ///
    /// # Errors
    ///
    /// Returns whatever `setup` returns, and the same errors as [`Application::launch`].
    #[allow(
        clippy::too_many_lines,
        reason = "launching an application is one ordered composition — read the manifest, grant \
                  what it asks for, start the server, register the surfaces, open the window — \
                  and splitting it would hide the order, which is the part that matters"
    )]
    pub fn launch_with(
        directory: &Path,
        limits: Limits,
        window_state: Option<PathBuf>,
        setup: impl FnOnce(&mut Runtime) -> Result<()>,
    ) -> Result<Self> {
        let manifest = AppManifest::load(directory).map_err(|error| {
            NcError::new(ErrorKind::Validation {
                field: "application".to_owned(),
                reason: error.to_string(),
            })
            .operation("application::launch")
        })?;

        let assets = manifest.assets_directory(directory);
        // The GPU island surface, when this build has one. The route lives on the page's own origin,
        // so a page fetches a rendered island the way it fetches its own files: same origin, no
        // per-platform URL scheme, and no second server to trust.
        #[cfg(feature = "gpu-islands")]
        let routes = vec![crate::islands::Islands::new().route()];
        #[cfg(not(feature = "gpu-islands"))]
        let routes = Vec::new();
        let server = AssetServer::start_with(&assets, routes).map_err(|error| {
            NcError::new(ErrorKind::Validation {
                field: "assets".to_owned(),
                reason: format!("the application's assets cannot be served: {error}"),
            })
            .operation("application::launch")
        })?;

        // The entry must be a file that exists. A window opened at a 404 is not an
        // application running, and the failure is far easier to diagnose here than in a
        // window the user has to look at.
        let entry_path = manifest.entry_path(directory).map_err(|error| {
            NcError::new(ErrorKind::Validation {
                field: "entry".to_owned(),
                reason: error.to_string(),
            })
            .operation("application::launch")
        })?;
        if !entry_path.is_file() {
            return Err(NcError::new(ErrorKind::NotFound {
                kind: "entry_page".to_owned(),
                id: manifest.entry.clone(),
            })
            .operation("application::launch"));
        }

        let url = server.url_for(&manifest.entry);
        let origin = Origin::new(Scheme::Http, "127.0.0.1", Some(server.local_addr().port()));

        let mut runtime = Runtime::start(limits)?;
        let granted: Vec<ncui_core::Capability> = manifest.capabilities.iter().collect();
        runtime.grant_page_capabilities(&granted);
        setup(&mut runtime)?;
        // The native commands the manifest's grant covers. Registered before the window
        // opens, because a page's first call is made as soon as it loads.
        let link: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let native_commands =
            crate::native::register(&runtime, directory, manifest.identity.as_deref())?;
        // The link this launch was opened with, answerable by the page.
        //
        // It requires no capability: the page *is* the application the link was addressed to, and
        // a link the user clicked is not authority over anything. Registering it here rather than
        // in the native surface keeps it true for every application, including one that holds no
        // native capability at all.
        let link_cell = Arc::clone(&link);
        // Where this application's state lives: the caller's choice when it has one, because
        // everything below — the remembered geometry and the page's own preferences — is read
        // and written through it.
        let window_state = window_state.unwrap_or_else(|| window_state_path(&manifest.name));

        // What the page remembers between runs, beside the window's geometry: one parameter
        // decides where an application's state lives, and the two files sit together in it.
        let page_state = Arc::new(Mutex::new(crate::page_state::PageState::open(
            &window_state.with_file_name("state.json"),
        )));
        let state_commands = register_state_commands(&runtime, &page_state)?;

        let link_command = runtime
            .register_page_command(
                "app.link",
                ncui_core::CapabilitySet::empty(),
                "The link this application was opened with, if it was opened by one.",
                move |_payload| {
                    let link = link_cell
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone();
                    Ok(serde_json::json!({
                        "opened_by_link": link.is_some(),
                        "link": link,
                    }))
                },
            )
            .map_err(|error| {
                NcError::new(ErrorKind::Validation {
                    field: "app.link".to_owned(),
                    reason: format!("`app.link` could not be registered: {error}"),
                })
                .operation("application::launch")
            })?;

        // The component surface, rooted at the same directory the pages already reach: an
        // artifact is named relative to the application, and `run.components` is what decides
        // whether the commands exist at all.
        let component_commands = crate::components::register(&runtime, directory)?;
        let runtime = runtime.with_host(&[origin])?;
        // Where the places of windows a page names are kept, beside the application's own window
        // state: one parameter decides where an application's state lives.
        runtime
            .windows
            .remember_windows_in(&window_state.with_file_name("windows.json"));

        // What the window was last left as, when the application has been run before. The
        // manifest says what a *first* run gets; a remembered geometry is what the user chose by
        // moving and resizing the window, and it wins.
        let remembered = read_window_state(&window_state);

        let mut request = WindowRequest::new(url.clone()).with_title(
            manifest
                .window
                .title
                .clone()
                .unwrap_or_else(|| manifest.name.clone()),
        );
        let mut width = request.width;
        let mut height = request.height;
        if let Some(configured) = manifest.window.width {
            width = configured;
        }
        if let Some(configured) = manifest.window.height {
            height = configured;
        }
        request = request.with_size(width, height);
        if let Some(remembered) = remembered {
            // A place remembered while a second display was attached puts the window where that
            // display used to be: off-screen, and unreachable for a user who cannot drag what
            // they cannot see. The clamp moves it onto a display that exists and keeps the size
            // the user chose.
            let monitors = runtime.windows.monitors();
            let geometry = remembered.clamped_to(&monitors);
            if geometry.x != remembered.x || geometry.y != remembered.y {
                tracing::info!(
                    application = %manifest.name,
                    from_x = remembered.x,
                    from_y = remembered.y,
                    x = geometry.x,
                    y = geometry.y,
                    "the remembered window place is on no display, so it was moved onto one"
                );
            }
            request = request.with_size(geometry.width, geometry.height);
            request = request.with_position(geometry.x, geometry.y);
            let _ = runtime.metrics.counter_inc(
                "app.window_restores",
                &ncui_observability::metrics::Labels::new(),
                1,
            );
            tracing::info!(
                application = %manifest.name,
                width = geometry.width,
                height = geometry.height,
                x = geometry.x,
                y = geometry.y,
                "the window is where it was left"
            );
        }

        // What the manifest asks the window to be, applied once the size is final: a floor is
        // checked against the window's own size, so a remembered size below a floor the manifest
        // added afterwards would refuse to launch — and a window smaller than its own floor is not
        // a window the manifest allows.
        if let (Some(min_width), Some(min_height)) =
            (manifest.window.min_width, manifest.window.min_height)
        {
            let width = request.width.max(min_width);
            let height = request.height.max(min_height);
            request = request.with_size(width, height);
            request = request.with_min_size(min_width, min_height);
        }
        request = request
            .with_always_on_top(manifest.window.always_on_top)
            .with_decorations(manifest.window.decorations)
            .with_transparent(manifest.window.transparent)
            .with_resizable(manifest.window.resizable)
            .with_fullscreen(manifest.window.fullscreen)
            .with_maximized(manifest.window.maximized);

        // The window's icon, when the manifest names one. The file is read and decoded here rather
        // than in the host, so a file that is missing or in a format this runtime cannot read is
        // reported to the person who named it — with the path they named — instead of leaving a
        // window whose icon silently never appears.
        let icon_path = manifest.icon_path(directory).map_err(|error| {
            NcError::new(ErrorKind::Validation {
                field: "icon".to_owned(),
                reason: error.to_string(),
            })
            .operation("application::launch")
        })?;
        if let Some(icon_path) = icon_path {
            let bytes = std::fs::read(&icon_path).map_err(|error| {
                NcError::new(ErrorKind::Validation {
                    field: "icon".to_owned(),
                    reason: format!("{}: {error}", icon_path.display()),
                })
                .operation("application::launch")
            })?;
            let icon = crate::icon::decode(&bytes).map_err(|error| {
                NcError::new(ErrorKind::Validation {
                    field: "icon".to_owned(),
                    reason: format!("{}: {error}", icon_path.display()),
                })
                .operation("application::launch")
            })?;
            request = request.with_icon(icon);
        }

        // The global shortcuts the manifest declares, taken before the window opens so a press that
        // arrives while the page is loading is not lost.
        #[cfg(feature = "global-shortcuts")]
        let shortcuts = register_shortcuts(&manifest)?;
        #[cfg(not(feature = "global-shortcuts"))]
        if !manifest.shortcuts.is_empty() {
            return Err(NcError::new(ErrorKind::Validation {
                field: "shortcuts".to_owned(),
                reason: "this build cannot register global shortcuts; rebuild with the \
                         `global-shortcuts` feature"
                    .to_owned(),
            })
            .operation("application::launch"));
        }

        let window = runtime.windows.open(&request, None).map_err(|error| {
            NcError::new(ErrorKind::Validation {
                field: "window".to_owned(),
                reason: error.to_string(),
            })
            .operation("application::launch")
        })?;

        // The tray icon, when the manifest shows one. Its image is read and decoded here rather than
        // in the host, so a file that is missing or in a format this runtime cannot read is reported
        // to the person who named it — with the path they named.
        #[cfg(feature = "tray-icon")]
        if manifest.tray.is_some() {
            let request = tray_request(directory, &manifest)?;
            runtime
                .windows
                .set_tray(window, Some(request))
                .map_err(|error| {
                    NcError::new(ErrorKind::Validation {
                        field: "tray".to_owned(),
                        reason: error.to_string(),
                    })
                    .operation("application::launch")
                })?;
            let _ = runtime.metrics.counter_inc(
                "app.trays_shown",
                &ncui_observability::metrics::Labels::new(),
                1,
            );
            tracing::info!(
                application = %manifest.name,
                "the application shows a tray icon"
            );
        }
        // A build that cannot show a tray refuses a manifest that declares one, rather than
        // launching an application whose tray silently never appears.
        #[cfg(not(feature = "tray-icon"))]
        if manifest.tray.is_some() {
            return Err(NcError::new(ErrorKind::Validation {
                field: "tray".to_owned(),
                reason: "this build cannot show a tray icon; rebuild with the `tray-icon` feature"
                    .to_owned(),
            })
            .operation("application::launch"));
        }

        tracing::info!(
            application = %manifest.name,
            version = %manifest.version,
            url = %url,
            "the application is running"
        );

        Ok(Self {
            runtime,
            server,
            window,
            url,
            window_state,
            name: manifest.name,
            version: manifest.version,
            reported_responses: 0,
            watcher: None,
            last_watch_poll: None,
            reloads: 0,
            native_commands,
            component_commands,
            link_command,
            state_commands,
            instance: None,
            handoffs: 0,
            link,
            #[cfg(feature = "global-shortcuts")]
            shortcuts,
        })
    }

    /// Serves whatever the pages have sent.
    ///
    /// Returns how many messages were answered. A page that is idle costs nothing: the
    /// call returns immediately when no page has anything to say.
    ///
    /// The counts are published to the runtime's metrics registry, so an operator can see
    /// that an application is serving and how much of it is traffic.
    pub fn pump(&mut self) -> usize {
        let now = self.runtime.elapsed();
        let served = self.runtime.pump_pages(now);
        self.reload_if_changed();
        self.receive_handoffs();
        #[cfg(feature = "global-shortcuts")]
        self.deliver_shortcuts();
        let labels = ncui_observability::metrics::Labels::new();
        if served > 0 {
            let _ =
                self.runtime
                    .metrics
                    .counter_inc("app.page_calls_served", &labels, served as u64);
        }
        let responses = self.server.responses_served();
        if responses > self.reported_responses {
            let delta = responses - self.reported_responses;
            self.reported_responses = responses;
            let _ = self
                .runtime
                .metrics
                .counter_inc("app.asset_responses_served", &labels, delta);
        }
        served
    }

    /// Waits for a page message, then pumps.
    ///
    /// The wait is the bridge's idle path: a page's call is answered as soon as the engine
    /// delivers it rather than after the loop's next sleep. The timeout bounds the wait so
    /// the loop still notices a window that has closed and a reload that is due.
    pub fn pump_blocking(&mut self, timeout: std::time::Duration) -> usize {
        let _ = self.runtime.windows.wait_page_message(timeout);
        self.pump()
    }

    /// Tells the page about a shortcut the user pressed, if one was.
    ///
    /// The page hears an event rather than having an action run for it: what a shortcut *does* is
    /// the application's decision, and the page is the part of the application that knows. The
    /// runtime's job is to deliver the press, which is the part that needs the platform.
    #[cfg(feature = "global-shortcuts")]
    fn deliver_shortcuts(&mut self) {
        {
            let Some(held) = self.shortcuts.as_ref() else {
                return;
            };
            let Some(accelerator) = held.manager.take_pressed() else {
                return;
            };
            let command = held
                .declared
                .iter()
                .find(|declared| declared.accelerator.eq_ignore_ascii_case(&accelerator))
                .map_or_else(String::new, |declared| declared.command.clone());
            let event = serde_json::json!({
                "event": "shortcut",
                "payload": { "accelerator": accelerator, "command": command },
            });
            let Ok(encoded) = serde_json::to_string(&event) else {
                tracing::warn!("a shortcut event could not be serialised");
                return;
            };
            let script = format!("window.ncui && window.ncui.__dispatch({encoded});");
            if !self
                .runtime
                .windows
                .post_to_page(self.window, &script)
                .unwrap_or(false)
            {
                tracing::warn!(
                    application = %self.name,
                    "a shortcut press could not be delivered to the page"
                );
                return;
            }
            let _ = self.runtime.metrics.counter_inc(
                "app.shortcuts_pressed",
                &ncui_observability::metrics::Labels::new(),
                1,
            );
            tracing::info!(
                application = %self.name,
                accelerator = %accelerator,
                command = %command,
                "a global shortcut was pressed"
            );
        }
    }

    /// Receives any second launch that is waiting, and reloads for it.
    ///
    /// The arguments are logged rather than acted on: what a second launch *means* is the
    /// application's decision, and the one thing every application wants is the page it already
    /// has rather than a second window. A page that wants to do more with the arguments can ask
    /// for them through the bridge; the runtime's job is to make sure they arrive.
    fn receive_handoffs(&mut self) {
        let Some(instance) = self.instance.as_ref() else {
            return;
        };
        match instance.try_receive() {
            Ok(Some(arguments)) => {
                self.handoffs += 1;
                let _ = self.runtime.metrics.counter_inc(
                    "app.handoffs",
                    &ncui_observability::metrics::Labels::new(),
                    1,
                );
                // A link that arrived with the second launch becomes *the* link. The page is
                // about to be reloaded and will ask what it was opened with; leaving the cell as
                // it was would answer with the first launch's link, which is a link the user has
                // already seen and possibly acted on.
                let arrived = arguments.iter().find(|argument| link_in(argument)).cloned();
                if let Some(link) = arrived.clone() {
                    *self
                        .link
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(link);
                }
                tracing::info!(
                    application = %self.name,
                    handoffs = self.handoffs,
                    arguments = ?arguments,
                    link = arrived.as_deref().unwrap_or("none"),
                    "a second launch reached the running instance"
                );
                let _ = self
                    .runtime
                    .windows
                    .post_to_page(self.window, "window.location.reload();");
            }
            Ok(None) => {}
            Err(error) => {
                // A hand-off that is not a hand-off is worth knowing about: something local
                // connected to the instance's port. It is not fatal, and it is not silent.
                tracing::warn!(
                    application = %self.name,
                    code = error.code(),
                    %error,
                    "a hand-off was refused"
                );
            }
        }
    }

    /// Writes down where the window was, so the next run can put it back there.
    ///
    /// A geometry the platform cannot report is not written: an application that recorded a
    /// guess would move the user's window to the guess on the next run.
    fn remember_window_state(&mut self) {
        let Some(geometry) = self.geometry() else {
            tracing::debug!(
                application = %self.name,
                "the window's geometry is not available to remember"
            );
            return;
        };
        if !geometry.is_usable() {
            tracing::warn!(
                application = %self.name,
                width = geometry.width,
                height = geometry.height,
                "the reported window geometry is not one a window may have, so it is not remembered"
            );
            return;
        }
        if let Err(error) = write_window_state(&self.window_state, geometry) {
            tracing::warn!(
                application = %self.name,
                path = %self.window_state.display(),
                %error,
                "the window's geometry could not be remembered"
            );
        }
    }

    /// Reloads the page when the application's files changed.
    fn reload_if_changed(&mut self) {
        const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
        if self.watcher.is_none() {
            return;
        }
        if self
            .last_watch_poll
            .is_some_and(|last| last.elapsed() < POLL_INTERVAL)
        {
            return;
        }
        self.last_watch_poll = Some(std::time::Instant::now());
        let changed = match self.watcher.as_mut() {
            Some(watcher) => watcher.changed(),
            None => return,
        };
        match changed {
            Ok(true) => {
                self.reloads += 1;
                let _ = self
                    .runtime
                    .windows
                    .post_to_page(self.window, "window.location.reload();");
                let labels = ncui_observability::metrics::Labels::new();
                let _ = self.runtime.metrics.counter_inc("app.reloads", &labels, 1);
                tracing::info!(
                    application = %self.name,
                    reloads = self.reloads,
                    "the page is reloading because the application's files changed"
                );
            }
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(
                    application = %self.name,
                    %error,
                    "the application's files could not be scanned"
                );
            }
        }
    }

    /// Whether the application's window is still open.
    ///
    /// The engine reports that it has gone when the user closes the window, and a window
    /// whose engine is gone no longer serves.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.runtime
            .windows
            .summaries()
            .is_ok_and(|summaries| summaries.iter().any(|summary| summary.serving))
    }

    /// The URL the application's page is served at.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The application's name, as its manifest gives it.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The application's version, as its manifest gives it.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The global shortcuts this application holds, by accelerator.
    ///
    /// Empty when it declares none, and empty in a build that cannot hold any — in which case a
    /// manifest that declares one is refused at launch rather than quietly ignored.
    #[must_use]
    pub fn shortcuts(&self) -> Vec<String> {
        #[cfg(feature = "global-shortcuts")]
        {
            self.shortcuts
                .as_ref()
                .map_or_else(Vec::new, |held| held.manager.registered())
        }
        #[cfg(not(feature = "global-shortcuts"))]
        {
            Vec::new()
        }
    }

    /// The window the application opened.
    #[must_use]
    pub fn window(&self) -> WindowId {
        self.window
    }

    /// The runtime the application is running on.
    #[must_use]
    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    /// Reloads the page whenever a file under the assets directory changes.
    ///
    /// This is the development loop: edit a file, and the window shows the change. The
    /// watcher polls, because `std` offers no file-system notification without a platform
    /// binding; at the size of an application's assets the poll costs nothing visible.
    ///
    /// # Errors
    ///
    /// Returns an error when the assets directory cannot be read.
    /// Where this application's window state is remembered.
    #[must_use]
    pub fn window_state_path(&self) -> &Path {
        &self.window_state
    }

    /// The window's geometry as the platform reports it, if the host can report one.
    #[must_use]
    pub fn geometry(&self) -> Option<ncui_webview::WindowGeometry> {
        self.runtime.windows.geometry(self.window)
    }

    /// Records the link this launch was started with.
    ///
    /// Called by the command line, which is where a registered scheme hands the URL over.
    #[must_use]
    pub fn with_link(self, link: Option<String>) -> Self {
        *self
            .link
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = link;
        self
    }

    /// Where this application's window state is remembered.
    /// The link this application was opened with, if it was opened by one.
    #[must_use]
    pub fn link(&self) -> Option<String> {
        self.link
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Claims this process as the instance for the application.
    ///
    /// A second launch does not start a twin: it hands its arguments to the instance that is
    /// running, and the instance reloads. That is what makes a deep link or a double-clicked
    /// file reach the window the user already has rather than open another one.
    #[must_use]
    pub fn with_instance(mut self, instance: ncui_native::InstanceGuard) -> Self {
        self.instance = Some(instance);
        self
    }

    /// How many second launches this instance has received.
    #[must_use]
    pub const fn handoffs(&self) -> usize {
        self.handoffs
    }

    pub fn watch(&mut self) -> Result<()> {
        let watcher = DirectoryWatcher::new(self.server.root()).map_err(|error| {
            NcError::new(ErrorKind::Validation {
                field: "watch".to_owned(),
                reason: format!("the assets directory cannot be watched: {error}"),
            })
            .operation("application::watch")
        })?;
        self.watcher = Some(watcher);
        self.last_watch_poll = Some(std::time::Instant::now());
        tracing::info!(application = %self.name, "watching the application's files for changes");
        Ok(())
    }

    /// How many times the page has been reloaded because a file changed.
    #[must_use]
    pub fn reloads(&self) -> usize {
        self.reloads
    }

    /// Stops the application.
    ///
    /// The window is closed first, so a page that is mid-request is not left talking to a
    /// server that has already gone.
    pub fn shutdown(&mut self) {
        tracing::info!(
            application = %self.name,
            "the application is stopping"
        );
        // The tray goes with the application: an icon left in the tray would be an icon for a
        // process that has stopped serving, and a user clicking it would be clicking nothing.
        #[cfg(feature = "tray-icon")]
        let _ = self.runtime.windows.set_tray(self.window, None);
        let _ = self.runtime.windows.close(self.window);
        for registration in &mut self.native_commands {
            registration.dispose();
        }
        for registration in &mut self.component_commands {
            registration.dispose();
        }
        self.link_command.dispose();
        for registration in &mut self.state_commands {
            registration.dispose();
        }
        // Where the windows were, written before any of them closes: closing the application's own
        // window above writes the place of every remembered window down while it still exists, and
        // the claim to be the instance is released afterwards.
        self.remember_window_state();

        // The shortcuts are released with the application: a key held by a process that has
        // stopped serving is a key no other application can take.
        #[cfg(feature = "global-shortcuts")]
        {
            self.shortcuts = None;
        }

        // The instance claim is released with the application: a lock that outlived the process
        // it described would be a stale lock for the next launch to work around, and the
        // instance is not running any more.
        self.instance = None;
        self.runtime.shutdown();
        self.server.stop();
    }
}

/// The shortcuts an application holds while it runs.
///
/// The manager and the declarations are kept together because a press has to be turned back into
/// the command the manifest named, and the two are useless apart.
#[cfg(feature = "global-shortcuts")]
#[derive(Debug)]
struct HeldShortcuts {
    /// The platform's registration table.
    manager: ncui_native::hotkey::Shortcuts,
    /// What the manifest declared, so a press can be answered with the command it stands for.
    declared: Vec<ncui_app::ShortcutConfig>,
}

/// Registers the shortcuts a manifest declares.
///
/// A shortcut the platform refuses is a launch failure that names the command: an application whose
/// shortcut silently does nothing is worse than one that says it could not take the key.
#[cfg(feature = "global-shortcuts")]
fn register_shortcuts(manifest: &AppManifest) -> Result<Option<HeldShortcuts>, NcError> {
    if manifest.shortcuts.is_empty() {
        return Ok(None);
    }
    let manager = ncui_native::hotkey::Shortcuts::new().map_err(|error| {
        NcError::new(ErrorKind::Validation {
            field: "shortcuts".to_owned(),
            reason: error.to_string(),
        })
        .operation("application::launch")
    })?;
    for declared in &manifest.shortcuts {
        manager.register(&declared.accelerator).map_err(|error| {
            NcError::new(ErrorKind::Validation {
                field: "shortcuts".to_owned(),
                reason: format!("`{}`: {error}", declared.command),
            })
            .operation("application::launch")
        })?;
    }
    tracing::info!(
        application = %manifest.name,
        count = manifest.shortcuts.len(),
        "the application holds global shortcuts"
    );
    Ok(Some(HeldShortcuts {
        manager,
        declared: manifest.shortcuts.clone(),
    }))
}

/// The tray request a manifest declares, with its image read and decoded.
///
/// The same decoder the window's icon uses, because a platform's tray and its window frame want the
/// same thing: straight RGBA, which is what the file is turned into here.
#[cfg(feature = "tray-icon")]
fn tray_request(
    directory: &Path,
    manifest: &AppManifest,
) -> Result<ncui_webview::TrayRequest, NcError> {
    let Some(tray) = manifest.tray.as_ref() else {
        return Err(NcError::new(ErrorKind::Validation {
            field: "tray".to_owned(),
            reason: "the manifest declares no tray icon".to_owned(),
        })
        .operation("application::launch"));
    };
    let icon_path = manifest
        .tray_icon_path(directory)
        .map_err(|error| {
            NcError::new(ErrorKind::Validation {
                field: "tray".to_owned(),
                reason: error.to_string(),
            })
            .operation("application::launch")
        })?
        .ok_or_else(|| {
            NcError::new(ErrorKind::Validation {
                field: "tray".to_owned(),
                reason: "the manifest names no tray icon".to_owned(),
            })
            .operation("application::launch")
        })?;
    let bytes = std::fs::read(&icon_path).map_err(|error| {
        NcError::new(ErrorKind::Validation {
            field: "tray".to_owned(),
            reason: format!("{}: {error}", icon_path.display()),
        })
        .operation("application::launch")
    })?;
    let icon = crate::icon::decode(&bytes).map_err(|error| {
        NcError::new(ErrorKind::Validation {
            field: "tray".to_owned(),
            reason: format!("{}: {error}", icon_path.display()),
        })
        .operation("application::launch")
    })?;
    let request = ncui_webview::TrayRequest::new(icon, tray.tooltip.clone());
    if !request.is_usable() {
        return Err(NcError::new(ErrorKind::Validation {
            field: "tray".to_owned(),
            reason: format!(
                "{} is not an image or tooltip a platform will show: the tooltip must be between                  one and {} characters",
                icon_path.display(),
                ncui_webview::MAX_TRAY_TOOLTIP
            ),
        })
        .operation("application::launch"));
    }
    Ok(request)
}

/// Registers the commands a page stores its own preferences with.
///
/// They require no capability: what a page remembers about itself is its own business, and the
/// file it writes belongs to the application it is. Asking a page to hold a capability in order
/// to remember which column was widest would make every application request an authority it does
/// not want and the user cannot evaluate.
fn register_state_commands(
    runtime: &Runtime,
    state: &Arc<Mutex<crate::page_state::PageState>>,
) -> Result<Vec<crate::commands::CommandRegistration>> {
    let mut registrations = Vec::new();
    let metrics = Arc::clone(&runtime.metrics);
    for (name, summary) in [
        (
            "state.save",
            "Store a value under a key, for the next time this application runs.",
        ),
        (
            "state.load",
            "Read the value stored under a key, or learn that nothing is stored under it.",
        ),
        ("state.remove", "Forget the value stored under a key."),
    ] {
        let state = Arc::clone(state);
        let metrics = Arc::clone(&metrics);
        let registration = runtime
            .register_page_command(
                name,
                ncui_core::CapabilitySet::empty(),
                summary,
                move |payload| {
                    let key = payload
                        .get("key")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| {
                            ncui_webview::SeamError::new("invalid_params", "`key` must be a string")
                        })?;
                    let mut store = state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if name == "state.save" {
                        let _ = metrics.counter_inc(
                            "app.state_writes",
                            &ncui_observability::metrics::Labels::new(),
                            1,
                        );
                    }
                    let answer = match name {
                        "state.save" => {
                            let value = payload
                                .get("value")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null);
                            store
                                .save(key, value)
                                .map_err(|error| state_error(&error))?
                        }
                        "state.remove" => store.remove(key).map_err(|error| state_error(&error))?,
                        _ => store.load(key),
                    };
                    Ok(answer)
                },
            )
            .map_err(|error| {
                NcError::new(ErrorKind::Validation {
                    field: name.to_owned(),
                    reason: format!("`{name}` could not be registered: {error}"),
                })
                .operation("application::launch")
            })?;
        registrations.push(registration);
    }
    Ok(registrations)
}

/// Maps a state failure onto the bridge's error shape.
fn state_error(error: &NcError) -> ncui_webview::SeamError {
    ncui_webview::SeamError::new("state_refused", error.to_string())
}

/// Whether an argument a second launch carried names something the platform would open.
///
/// The hand-off carries the second launch's argument vector, which is what the operating system
/// gave it: for a deep link that is `run <directory> <link>`. The link is found by asking which
/// argument names a scheme, rather than by a position that would be wrong for every other way of
/// launching the application.
fn link_in(argument: &str) -> bool {
    ncui_native::scheme_of(argument).is_some()
}

/// Where an application's window state is remembered.
///
/// Named after the application, under this user's state directory: two applications that happen
/// to share a name are two entries, and a name that cannot be a directory is not used at all.
fn window_state_path(name: &str) -> PathBuf {
    let safe: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '-'
            }
        })
        .collect();
    ncui_native::state_root().join(safe).join("window.json")
}

/// The window geometry an application last left behind, if it left a usable one.
///
/// A file that cannot be read, or that holds something a window cannot be, is treated as no
/// memory at all: refusing to start because a state file is damaged would turn a preference into
/// a failure.
fn read_window_state(path: &Path) -> Option<ncui_webview::WindowGeometry> {
    let encoded = std::fs::read(path).ok()?;
    let geometry: ncui_webview::WindowGeometry = serde_json::from_slice(&encoded).ok()?;
    if geometry.is_usable() {
        Some(geometry)
    } else {
        tracing::warn!(
            path = %path.display(),
            width = geometry.width,
            height = geometry.height,
            "the remembered window geometry is not one a window may have, so it is ignored"
        );
        None
    }
}

/// Writes down a window's geometry.
fn write_window_state(path: &Path, geometry: ncui_webview::WindowGeometry) -> Result<(), NcError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            NcError::new(ErrorKind::Validation {
                field: "window_state".to_owned(),
                reason: format!("{}: {error}", parent.display()),
            })
            .operation("application::remember_window_state")
        })?;
    }
    let encoded = serde_json::to_vec_pretty(&geometry).map_err(|error| {
        NcError::new(ErrorKind::Internal {
            operation: "application::remember_window_state".to_owned(),
            reason: error.to_string(),
        })
    })?;
    std::fs::write(path, encoded).map_err(|error| {
        NcError::new(ErrorKind::Validation {
            field: "window_state".to_owned(),
            reason: format!("{}: {error}", path.display()),
        })
        .operation("application::remember_window_state")
    })
}

impl Drop for Application {
    fn drop(&mut self) {
        self.shutdown();
    }
}
