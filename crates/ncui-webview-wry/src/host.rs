//! Binding of `ncui-webview`'s policy to a real `wry`/`tao` engine.
//!
//! The engine does not decide anything. It is given the window's [`HostSeam`] and asks it:
//!
//! * a navigation is offered to the seam, and the seam's verdict is what the engine obeys;
//! * a message from the page is forwarded verbatim, and the runtime evaluates it through
//!   the same seam.
//!
//! That is the whole point of this binding. A host that kept its own copy of the rules
//! would be a host whose rules could drift from the ones the runtime enforces, and the
//! drift would be invisible until a page exploited it.
//!
//! # One event loop, many windows
//!
//! `tao` owns a single event loop per process, and a second one is not a second loop — it
//! is a crash. The first version of this binding created a loop per window, which worked
//! for exactly one window and took the process down on the second.
//!
//! So the loop is process-wide and created once. A window is a message to it: `Create`
//! builds the window and its webview on the loop's own thread, `Eval` runs a script in one
//! window, and `Close` drops one. That is how an application opens a second window, and it
//! is why the engine is a service rather than a thread per window.
//!
//! The `#[cfg(feature = "wry-engine")]` build creates the loop and runs it on a background
//! thread; the default build (feature off) is a small, honest fallback so the crate still
//! compiles everywhere, including Linux CI without `webkit2gtk`.

use std::collections::HashMap;
#[cfg(all(feature = "wry-engine", target_os = "macos"))]
use std::sync::atomic::AtomicI32;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
#[cfg(feature = "wry-engine")]
use std::sync::{OnceLock, atomic::AtomicU64};
#[cfg(feature = "wry-engine")]
use std::thread;
use std::time::Duration;
#[cfg(any(target_os = "linux", test))]
use std::time::Instant;

#[cfg(feature = "wry-engine")]
use crate::cdp_target::PageTargetBinding;
#[cfg(all(feature = "wry-engine", target_os = "windows"))]
use crate::cdp_target::{bind_page_target_after_window, page_targets_before_window};
#[cfg(feature = "wry-engine")]
use crate::child::{EngineChild, build_child};
#[cfg(feature = "wry-engine")]
use crate::geometry::{Geometry, WindowOptions};
#[cfg(feature = "wry-engine")]
use crate::host_scripts::{DRAG_REGION_SCRIPT, dropped_files_script, moved_script, resized_script};
use ncui_core::WindowId;
use ncui_webview::{ChildPageMessage, HostSeam, PageMessage, WebViewBackend};
#[cfg(feature = "wry-engine")]
use ncui_webview_child::ChildWebviewError;
use ncui_webview_child::ChildWebviewRequest;

#[cfg(feature = "wry-engine")]
use tao::event::{Event, WindowEvent};
#[cfg(feature = "wry-engine")]
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget};
#[cfg(feature = "wry-engine")]
use tao::window::{Window, WindowBuilder};
#[cfg(feature = "wry-engine")]
use wry::{WebView, WebViewBuilder};

/// The payload carried on the in-process IPC channel bridged from the `wry` IPC handler.
///
/// Alias of [`String`] so callers may treat a received event as the request body text
/// posted by the page (the only content the seam cares about today).
pub type HostIpcEvent = String;

/// The script injected into every page so it has a bridge to the host.
///
/// `wry` installs `window.ipc.postMessage`; the bridge adds a promise-based API on top of
/// it, so an application writes `window.ncui.invoke("action", payload)` rather than knowing
/// which platform's message channel it is talking to.
///
/// Every request carries an id minted by the bridge. The host echoes it, which lets a page
/// keep several calls in flight and match each answer to the call that produced it. The
/// host still decides what every action means; the script is a transport, not a policy.
///
/// The file lives in the client package because it is the page's half of the contract, and
/// the runtime injects the same bytes the package ships. One file, so the two halves
/// cannot drift.
#[cfg(feature = "wry-engine")]
pub(super) const BRIDGE_SCRIPT: &str = include_str!("../../../packages/ncui-client/src/bridge.js");

/// One instruction to the process-wide engine.
/// How long a host waits for the engine to report a window's geometry.
///
/// Bounded because the caller is usually an application shutting down: a host that waited forever
/// for an engine that has stopped answering would hang the shutdown it is part of. The budget is the
/// same as any other request to the engine rather than a shorter one, because the engine is
/// process-wide and shared — a geometry read can queue behind another window's platform call, and a
/// budget tight enough to expire under that load reports "no geometry" for a window that has one.
/// An application's shutdown waiting two seconds rather than half of one is still bounded.
#[cfg(feature = "wry-engine")]
const GEOMETRY_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a host waits for the engine to answer a request about a window's presentation or shape.
///
/// Longer than the geometry's wait because the engine is process-wide and shared: a request can
/// queue behind another window's platform call, and a platform call — resizing a window, making it
/// fullscreen — takes longer than a frame. A budget too short would answer "the engine cannot do
/// that" for a window whose engine was merely busy, and that is the one answer a caller must not be
/// given wrongly: it reads as a platform limitation rather than as contention.
#[cfg(feature = "wry-engine")]
const ENGINE_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a Linux window request waits for the window manager to report the applied state.
#[cfg(all(feature = "wry-engine", target_os = "linux"))]
const LINUX_WINDOW_SETTLE_TIMEOUT: Duration = Duration::from_millis(1_500);

/// How often a deferred Linux window request re-reads the platform's state.
#[cfg(all(feature = "wry-engine", target_os = "linux"))]
const LINUX_WINDOW_POLL_INTERVAL: Duration = Duration::from_millis(16);

/// How long a host waits for the engine to answer a child-webview request.
///
/// Building a child starts another browser surface, which can take substantially longer than a
/// window presentation request. The wait remains bounded so a stopped engine cannot hang a caller.
#[cfg(feature = "wry-engine")]
const CHILD_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// A page exchange attaches, sends, and detaches, with a 30-second bound on each exchange.
#[cfg(feature = "wry-engine")]
const CDP_REPLY_TIMEOUT: Duration = Duration::from_secs(95);

/// First-start target discovery can include a cold browser launch and a 90-second readiness wait.
#[cfg(all(feature = "wry-engine", target_os = "windows"))]
const WINDOW_CREATE_TIMEOUT: Duration = Duration::from_secs(150);
#[cfg(all(feature = "wry-engine", not(target_os = "windows")))]
const WINDOW_CREATE_TIMEOUT: Duration = Duration::from_secs(15);

/// How long a factory waits for the engine to report its displays.
///
/// Bounded for the same reason the geometry wait is: the caller is an application starting up,
/// and a factory that waited forever for an engine that has stopped answering would hang the
/// launch it is part of.
#[cfg(feature = "wry-engine")]
const MONITORS_TIMEOUT: Duration = Duration::from_millis(500);

/// How many times a factory asks the engine for its displays before giving up.
///
/// More than one because the engine is shared by every window in the process: the first request
/// can arrive while another caller's window is being built, and a launch that reported "no
/// displays" because it asked at the wrong moment would put a remembered window where it cannot
/// be seen.
#[cfg(feature = "wry-engine")]
const MONITORS_ATTEMPTS: usize = 6;

/// How long to wait between those attempts.
#[cfg(feature = "wry-engine")]
const MONITORS_RETRY_DELAY: Duration = Duration::from_millis(100);

#[cfg(feature = "wry-engine")]
#[derive(Debug)]
pub enum HostEvent {
    /// Build a window and its webview.
    #[allow(
        private_interfaces,
        reason = "the event is the engine's internal command bus; the binding's public \
                  constructor names the type because it hands out the proxy that carries it"
    )]
    Create(Box<CreateRequest>),
    /// Run a script in one window's page.
    Eval {
        /// The window the script belongs to.
        window: u64,
        /// The script.
        script: String,
    },
    /// Close one window and drop its webview.
    Close {
        /// The window to close.
        window: u64,
    },
    /// Initiate a window drag (titlebar drag region).
    DragWindow {
        /// The window to drag.
        window: u64,
    },
    /// Build a child webview inside a parent window's frame.
    CreateChild {
        /// The parent window's internal engine identity.
        parent: u64,
        /// The caller-issued child identity, reserved before the platform build.
        id: u64,
        /// The child's starting URL and rectangle.
        request: Box<ChildWebviewRequest>,
        /// The isolated seam the child's page speaks through.
        seam: Arc<HostSeam>,
        /// Where the child's tagged IPC messages are delivered.
        ipc_tx: SyncSender<ChildPageMessage>,
        /// Where child IPC is delivered when the caller's receiver has disconnected.
        host_ipc_tx: SyncSender<ChildPageMessage>,
        /// Where the child's engine identity, or the failure, is delivered.
        reply: Sender<Result<u64, ChildWebviewError>>,
    },
    /// Move a live child webview to a new rectangle.
    ChildBounds {
        /// The parent window's internal engine identity.
        parent: u64,
        /// The child's engine identity.
        child: u64,
        /// The new rectangle.
        request: Box<ChildWebviewRequest>,
        /// Where the acceptance is delivered.
        reply: Sender<Result<(), ChildWebviewError>>,
    },
    /// Close a live child webview.
    CloseChild {
        /// The parent window's internal engine identity.
        parent: u64,
        /// The child's engine identity.
        child: u64,
        /// Where the acceptance is delivered.
        reply: Sender<Result<(), ChildWebviewError>>,
    },
    /// Run a script in one child webview's page.
    ChildEval {
        /// The parent window's internal engine identity.
        parent: u64,
        /// The child's engine identity.
        child: u64,
        /// The script.
        script: String,
    },
    /// Run one CDP command against a window's `DevTools` endpoint.
    CdpRequest {
        /// This window's identity inside the engine, never a runtime `WindowId`.
        window: u64,
        /// The CDP command (id/method/params).
        command: serde_json::Value,
        /// Where the page target's answer, or the typed failure, is delivered.
        reply: Sender<Result<serde_json::Value, crate::cdp::CdpError>>,
    },
    /// Scale one window's page by a zoom factor.
    Zoom {
        /// The window whose page is scaled.
        window: u64,
        /// The zoom factor, where `1.0` is the platform default.
        factor: f64,
        /// Where the acceptance is reported.
        reply: Sender<bool>,
    },
    /// Reads WebKit's applied zoom level for the live host regression.
    #[cfg(all(test, target_os = "linux"))]
    ZoomLevel {
        /// The window whose page is scaled.
        window: u64,
        /// Where the platform's applied level is reported.
        reply: Sender<Option<f64>>,
    },
    /// Open, close, or query the platform's web inspector for one window.
    DevTools {
        /// The window whose inspector is meant.
        window: u64,
        /// What to do to it.
        action: DevToolsAction,
        /// Where the acceptance (or the queried state) is reported.
        reply: Sender<bool>,
    },
    /// Stop the engine's event loop, ending the process with the code the
    /// application finished with.
    ///
    /// macOS only: there the engine's loop owns the main thread (ADR-018), so it
    /// is the loop that ends the process with the application's exit code.
    #[cfg(all(feature = "wry-engine", target_os = "macos"))]
    Quit,
    /// Show the application's tray icon, or remove it, reporting whether the platform agreed.
    #[cfg(feature = "tray-icon")]
    Tray {
        /// The tray to show, or `None` to remove the one that is there.
        request: Option<ncui_webview::TrayRequest>,
        /// Where the platform's answer goes.
        reply: Sender<Result<(), String>>,
    },
    /// The user clicked the tray icon.
    #[cfg(feature = "tray-icon")]
    TrayClicked,
    /// Attach or remove one window's manifest-declared native menu.
    #[cfg(feature = "native-menu")]
    SetMenu {
        /// The window whose menu is changing.
        window: u64,
        /// The menu to attach, or `None` to remove the current one.
        request: Option<ncui_webview::MenuRequest>,
        /// Where the platform's answer goes.
        reply: Sender<Result<(), String>>,
    },
    /// The user selected an item in a native menu.
    #[cfg(feature = "native-menu")]
    MenuActivated {
        /// The engine-scoped native item identity.
        id: String,
    },
    /// Report whether the platform is drawing its interface light or dark.
    Theme {
        /// The window whose platform theme is wanted.
        window: u64,
        /// Where the answer goes.
        reply: Sender<Option<ncui_webview::Theme>>,
    },
    /// Report the displays the engine is on.
    Monitors {
        /// Where the answer goes.
        reply: Sender<Vec<ncui_webview::MonitorRect>>,
    },
    /// Report one window's geometry.
    ///
    /// The engine thread owns the window, so this is the only place the answer exists: a host
    /// that guessed would be writing a guess into the application's state, and the next run
    /// would put the user's window there.
    Geometry {
        /// The window whose geometry is wanted.
        window: u64,
        /// Where the answer goes.
        reply: Sender<Option<Geometry>>,
    },
    /// Report how one window is presented, optionally changing it first.
    ///
    /// One event for reading and changing, because the answer a caller wants is always the state
    /// *after* the request: a caller that asked for fullscreen and was told "fullscreen: false"
    /// has learned something the platform knows and the runtime does not.
    WindowState {
        /// The window whose presentation is wanted.
        window: u64,
        /// The change to make first, when one was asked for.
        change: Option<ncui_webview::WindowChange>,
        /// Where the answer goes.
        reply: Sender<Option<ncui_webview::WindowState>>,
    },
    /// Ask the platform to make one window take a shape.
    WindowShape {
        /// The window to reshape.
        window: u64,
        /// What it should become.
        shape: Box<ncui_webview::WindowShape>,
        /// Where the answer goes: whether a window was there to reshape.
        reply: Sender<bool>,
    },
}

/// Everything needed to build one window.
#[cfg(feature = "wry-engine")]
#[derive(Debug)]
pub(crate) struct CreateRequest {
    /// The window's shape, or `None` for the default empty page.
    request: Option<ncui_webview::WindowRequest>,
    /// The policy the page in it obeys.
    seam: Arc<HostSeam>,
    /// Where the page's messages are delivered.
    ipc_tx: Sender<HostIpcEvent>,
    /// Where native menu selections for this window are delivered.
    menu_tx: Sender<ncui_webview::MenuEvent>,
    /// Set while the window is alive.
    alive: Arc<AtomicBool>,
    /// Where the engine's own failure is recorded.
    error: Arc<Mutex<Option<String>>>,
    /// Where the window's identity, or the failure, is delivered.
    reply: Sender<Result<u64, String>>,
}

/// What to do to a window's web inspector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevToolsAction {
    /// Open the inspector.
    Open,
    /// Close the inspector.
    Close,
    /// Report whether the inspector is open.
    Query,
}

/// The number every window created by the engine is known by.
#[cfg(feature = "wry-engine")]
static NEXT_WINDOW: AtomicU64 = AtomicU64::new(1);

/// The number every child webview created by the engine is known by.
#[cfg(feature = "wry-engine")]
static NEXT_CHILD: AtomicU64 = AtomicU64::new(1);

/// The process-wide engine, created on first use.
///
/// `OnceLock` rather than a `Mutex<Option<..>>`: the loop can only be created once, and a
/// lock would suggest it could be replaced.
#[cfg(feature = "wry-engine")]
static ENGINE: OnceLock<Option<EventLoopProxy<HostEvent>>> = OnceLock::new();

/// The exit code the application finished with, which the engine's loop exits with.
///
/// On macOS the engine's loop owns the main thread (ADR-018), so the process ends
/// inside that loop: the application's worker writes its exit code here and asks
/// the loop to quit.
#[cfg(all(feature = "wry-engine", target_os = "macos"))]
static QUIT_CODE: AtomicI32 = AtomicI32::new(0);

/// What a page hears when the tray icon is clicked.
///
/// A click is delivered as an event rather than as an action the runtime chose: what a tray click
/// *means* is the application's decision, and the page is the part of the application that knows.
#[cfg(feature = "tray-icon")]
const TRAY_CLICKED_SCRIPT: &str = "window.ncui && window.ncui.__dispatch({\"event\":\"tray\",\"payload\":{\"action\":\"clicked\"}});";

/// Whether the thread that carries tray events into the engine has been started.
///
/// The platform's channel of tray events is process-wide, so a second reader would take half of
/// them: the watcher is started once and reads for the life of the process.
#[cfg(feature = "tray-icon")]
static TRAY_WATCHER: AtomicBool = AtomicBool::new(false);

/// Shows the tray the request names, or removes the one that is there.
///
/// Dropping the tray is how it is removed: the platform takes it away when the value that owns it
/// goes, so there is nothing else to do for `None` beyond letting it fall out of scope.
#[cfg(feature = "tray-icon")]
fn show_tray(
    request: Option<ncui_webview::TrayRequest>,
    tray: &mut Option<tray_icon::TrayIcon>,
    proxy: EventLoopProxy<HostEvent>,
) -> Result<(), String> {
    match request {
        Some(request) => {
            *tray = Some(build_tray(&request, proxy)?);
            Ok(())
        }
        None => {
            *tray = None;
            Ok(())
        }
    }
}

/// Brings the application forward for a tray click, and tells its page.
///
/// A click on a tray is a request to see the application, which is why the window it opened is
/// brought forward as well as told: an application that only told its page would leave the user
/// looking at whatever was in front of them. A click that arrives after the tray was removed is
/// ignored rather than acted on.
#[cfg(feature = "tray-icon")]
fn tray_was_clicked(windows: &mut HashMap<u64, EngineWindow>, tray_is_shown: bool) {
    if !tray_is_shown {
        return;
    }
    // The application's own window is the first one it opened, which is the lowest identity: a
    // page that opened a second window is not the one a tray click belongs to.
    let Some((_, window)) = windows.iter_mut().min_by_key(|(id, _)| **id) else {
        return;
    };
    window.window.set_visible(true);
    window.window.set_minimized(false);
    window.window.set_focus();
    let _ = window.webview.evaluate_script(TRAY_CLICKED_SCRIPT);
}

/// Builds a tray icon and arranges for its clicks to reach the engine.
///
/// The tray must be built on the thread that owns the platform's event loop, which is the engine's
/// thread: a tray icon registered anywhere else would have no loop to belong to.
#[cfg(feature = "tray-icon")]
fn build_tray(
    request: &ncui_webview::TrayRequest,
    proxy: EventLoopProxy<HostEvent>,
) -> Result<tray_icon::TrayIcon, String> {
    let icon = tray_icon::Icon::from_rgba(
        request.icon.rgba.clone(),
        request.icon.width,
        request.icon.height,
    )
    .map_err(|error| format!("the tray's image cannot be used: {error}"))?;
    let tray = tray_icon::TrayIconBuilder::new()
        .with_icon(icon)
        .with_tooltip(&request.tooltip)
        .build()
        .map_err(|error| format!("the tray icon could not be created: {error}"))?;
    // The platform delivers tray events on a channel of its own, and the engine's loop waits rather
    // than polls, so a thread carries them across: without it a click would sit in the channel until
    // some window event happened to wake the loop, which for an application with one quiet window is
    // never.
    if !TRAY_WATCHER.swap(true, Ordering::SeqCst) {
        let started = thread::Builder::new()
            .name("ncui-tray-events".to_owned())
            .spawn(move || {
                let receiver = tray_icon::TrayIconEvent::receiver();
                while let Ok(event) = receiver.recv() {
                    let clicked = matches!(
                        event,
                        tray_icon::TrayIconEvent::Click {
                            button: tray_icon::MouseButton::Left,
                            button_state: tray_icon::MouseButtonState::Up,
                            ..
                        }
                    );
                    if clicked && proxy.send_event(HostEvent::TrayClicked).is_err() {
                        return;
                    }
                }
            });
        if started.is_err() {
            // The tray would be a picture that does nothing when it is clicked, so it is not kept.
            TRAY_WATCHER.store(false, Ordering::SeqCst);
            return Err("the tray's clicks could not be watched".to_owned());
        }
    }
    Ok(tray)
}

/// One live window inside the engine.
#[cfg(feature = "wry-engine")]
pub(super) struct EngineWindow {
    /// The immutable page-target binding, including a typed failure if binding was ambiguous.
    pub(super) cdp: PageTargetBinding,
    /// The native menu, dropped before the window so Windows detaches it from a live HWND.
    #[cfg(feature = "native-menu")]
    pub(super) menu: Option<crate::menu::NativeMenuState>,
    /// The platform window. Kept alive for as long as the entry is.
    pub(super) window: Window,
    /// The page inside it.
    webview: WebView,
    /// Set while the window has not been closed.
    alive: Arc<AtomicBool>,
    /// Where this window's native menu selections are delivered.
    #[cfg(feature = "native-menu")]
    pub(super) menu_tx: Sender<ncui_webview::MenuEvent>,
}

/// The engine's proxy, installed before the application's work begins.
///
/// On macOS this does not start anything: `AppKit` requires the event loop on the
/// main thread and `tao` offers no any-thread switch there (ADR-018), so the loop
/// is started by [`run_on_main_thread`] before the application's work begins and
/// this only reads the proxy that was installed then.
#[cfg(all(feature = "wry-engine", target_os = "macos"))]
fn engine() -> Option<&'static EventLoopProxy<HostEvent>> {
    ENGINE.get().and_then(Option::as_ref)
}

/// The engine's proxy, for bindings that drive the loop from outside this crate.
///
/// `None` when the loop could not be started. The proxy is clone-and-send, so a
/// binding can hold it and post events from its own thread.
#[cfg(feature = "wry-engine")]
#[must_use]
pub fn engine_proxy() -> Option<EventLoopProxy<HostEvent>> {
    engine().cloned()
}

/// The engine's proxy, starting the loop on first use.
#[cfg(all(feature = "wry-engine", not(target_os = "macos")))]
fn engine() -> Option<&'static EventLoopProxy<HostEvent>> {
    ENGINE
        .get_or_init(|| {
            let (ready_tx, ready_rx) = mpsc::channel::<EventLoopProxy<HostEvent>>();
            let spawned = thread::Builder::new()
                .name("ncui-webview-loop".to_owned())
                .spawn(move || {
                    let mut builder = EventLoopBuilder::<HostEvent>::with_user_event();
                    #[cfg(target_os = "windows")]
                    {
                        use tao::platform::windows::EventLoopBuilderExtWindows as _;
                        builder.with_any_thread(true);
                    }
                    // tao's Unix backend asserts the main thread unless the builder is told
                    // otherwise. The switch exists on X11/Wayland only. macOS (AppKit)
                    // requires the main thread and tao exposes no any-thread option, which is
                    // why that target starts its loop in `run_on_main_thread` instead.
                    #[cfg(any(
                        target_os = "linux",
                        target_os = "dragonfly",
                        target_os = "freebsd",
                        target_os = "netbsd",
                        target_os = "openbsd"
                    ))]
                    {
                        use tao::platform::unix::EventLoopBuilderExtUnix as _;
                        builder.with_any_thread(true);
                    }
                    let event_loop = builder.build();
                    let proxy = event_loop.create_proxy();
                    if ready_tx.send(proxy.clone()).is_err() {
                        return;
                    }
                    run_engine(event_loop, proxy);
                });
            match spawned {
                Ok(_) => ready_rx.recv_timeout(Duration::from_secs(15)).ok(),
                Err(_) => None,
            }
        })
        .as_ref()
}

/// Where this application's `WebView2` data lives: under the local data root, in a directory
/// named for the running executable, so two applications do not share one browser state.
///
/// The callers sit behind `feature = "wry-engine"` — one in `create_window`, one in the
/// transport test — and the cfg below mirrors that, so a build without the engine does not
/// carry (and dead-code-deny) the helpers.
#[cfg(feature = "wry-engine")]
fn webview_data_root() -> std::path::PathBuf {
    let name = std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "ncui".to_owned());
    dirs_local().join(name).join("webview")
}

/// The platform's per-user local data root.
#[cfg(all(feature = "wry-engine", target_os = "windows"))]
fn dirs_local() -> std::path::PathBuf {
    std::env::var_os("LOCALAPPDATA").map_or_else(std::env::temp_dir, std::path::PathBuf::from)
}

/// The platform's per-user local data root.
#[cfg(all(feature = "wry-engine", not(target_os = "windows")))]
fn dirs_local() -> std::path::PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".local/share"))
        })
        .unwrap_or_else(std::env::temp_dir)
}

/// Runs the engine's event loop on the main thread, and the application on a worker.
///
/// macOS (`AppKit`) requires the event loop to be created and run on the main
/// thread, and `tao` offers no any-thread switch there — the host's usual
/// worker-thread loop panics at startup (ADR-018). So on macOS the two roles the
/// other platforms use are exchanged: the engine loop owns the main thread, and
/// the application's work — the runtime pump that opens windows and serves
/// pages — runs on a worker that reaches the loop through the same proxy every
/// other thread uses.
///
/// When `app` returns, the loop is asked to quit and the process ends with the
/// code `app` returned. This must be the first thing `main` does on macOS: a
/// window created before the loop exists has no engine to be created in.
#[cfg(all(feature = "wry-engine", target_os = "macos"))]
pub fn run_on_main_thread(app: impl FnOnce() -> i32 + Send + 'static) -> ! {
    let event_loop = EventLoopBuilder::<HostEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let _ = ENGINE.set(Some(proxy.clone()));
    let worker = thread::Builder::new()
        .name("ncui-application".to_owned())
        .spawn(move || {
            let code = app();
            QUIT_CODE.store(code, Ordering::SeqCst);
        });
    // The joiner needs its own handle: the loop's proxy is consumed by `run_engine`.
    let quit_proxy = proxy.clone();
    if let Ok(worker) = worker {
        // Detached: it ends the loop when the application finishes, and the main
        // thread is inside the loop by then and cannot wait here.
        let _ = thread::Builder::new()
            .name("ncui-application-quit".to_owned())
            .spawn(move || {
                let _ = worker.join();
                let _ = quit_proxy.send_event(HostEvent::Quit);
            });
    }
    run_engine(event_loop, proxy)
}

/// Runs the loop, creating, evaluating in, and closing windows.
///
/// The proxy is handed in rather than made here because the tray needs to send events to the loop it
/// is created on, and with the tray feature off nothing else does.
#[cfg(feature = "wry-engine")]
#[cfg_attr(
    not(any(feature = "tray-icon", feature = "native-menu")),
    allow(
        unused_variables,
        reason = "only tray and menu events need to send into the loop"
    )
)]
#[allow(
    clippy::too_many_lines,
    reason = "the event loop match arms are each a distinct window event; splitting them would obscure the event flow"
)]
#[allow(
    clippy::needless_pass_by_value,
    reason = "the proxy is retained by the process-lifetime menu handler and the event-loop closure; \
              taking it by reference would require a second owner with the same lifetime"
)]
fn run_engine(
    event_loop: tao::event_loop::EventLoop<HostEvent>,
    proxy: EventLoopProxy<HostEvent>,
) -> ! {
    let mut windows: HashMap<u64, EngineWindow> = HashMap::new();
    let mut children: HashMap<u64, EngineChild> = HashMap::new();
    // The tray belongs to the application rather than to a window, so it lives beside the windows
    // rather than inside one: an application whose window is closed is still an application a user
    // may need to reach.
    #[cfg(feature = "tray-icon")]
    let mut tray: Option<tray_icon::TrayIcon> = None;
    #[cfg(feature = "native-menu")]
    let menu_proxy = proxy.clone();
    #[cfg(feature = "native-menu")]
    muda::MenuEvent::set_event_handler(Some(move |event: muda::MenuEvent| {
        let _ = menu_proxy.send_event(HostEvent::MenuActivated {
            id: event.id.as_ref().to_owned(),
        });
    }));
    event_loop.run(move |event, target, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(HostEvent::Create(create)) => {
                create_window(target, *create, &mut windows);
            }
            Event::UserEvent(HostEvent::Eval { window, script }) => {
                if let Some(entry) = windows.get(&window) {
                    let _ = entry.webview.evaluate_script(&script);
                }
            }
            Event::UserEvent(HostEvent::Close { window }) => {
                children.retain(|_, child| child.parent != window);
                if let Some(entry) = windows.remove(&window) {
                    entry.alive.store(false, Ordering::SeqCst);
                }
            }
            Event::UserEvent(HostEvent::DragWindow { window }) => {
                if let Some(entry) = windows.get(&window) {
                    let _ = entry.window.drag_window();
                }
            }
            Event::UserEvent(HostEvent::CdpRequest {
                window,
                command,
                reply,
            }) => {
                let answer = windows.get(&window).map_or_else(
                    || {
                        Err(crate::cdp::CdpError::TargetBindFailed {
                            reason: format!("engine window {window} is not live"),
                        })
                    },
                    |entry| entry.cdp.send(&command),
                );
                let _ = reply.send(answer);
            }
            Event::UserEvent(HostEvent::CreateChild {
                parent,
                id,
                request,
                seam,
                ipc_tx,
                host_ipc_tx,
                reply,
            }) => {
                let answer = match windows.get(&parent) {
                    Some(entry) => {
                        // The caller reserved the identity before sending this event, so a timed-out
                        // build can be closed by a later event even before its reply arrives.
                        match build_child(
                            id,
                            parent,
                            &entry.window,
                            &request,
                            &seam,
                            ipc_tx,
                            host_ipc_tx,
                        ) {
                            Ok(child) => {
                                children.insert(id, child);
                                Ok(id)
                            }
                            Err(reason) => Err(ChildWebviewError::EngineFailed { reason }),
                        }
                    }
                    None => Err(ChildWebviewError::ParentUnavailable),
                };
                let _ = reply.send(answer);
            }
            Event::UserEvent(HostEvent::ChildBounds {
                parent,
                child,
                request,
                reply,
            }) => {
                let answer = match children.get_mut(&child) {
                    Some(surface) if surface.parent == parent => surface.set_bounds(&request),
                    Some(_) => Err(ChildWebviewError::EngineFailed {
                        reason: format!(
                            "child webview {child} belongs to a different parent than {parent}"
                        ),
                    }),
                    None => Err(ChildWebviewError::EngineFailed {
                        reason: format!("child webview {child} is not live"),
                    }),
                };
                let _ = reply.send(answer);
            }
            Event::UserEvent(HostEvent::CloseChild {
                parent,
                child,
                reply,
            }) => {
                let answer = match children.get(&child) {
                    Some(surface) if surface.parent == parent => {
                        let _ = children.remove(&child);
                        Ok(())
                    }
                    Some(_) => Err(ChildWebviewError::EngineFailed {
                        reason: format!(
                            "child webview {child} belongs to a different parent than {parent}"
                        ),
                    }),
                    None => Err(ChildWebviewError::EngineFailed {
                        reason: format!("child webview {child} is not live"),
                    }),
                };
                let _ = reply.send(answer);
            }
            Event::UserEvent(HostEvent::ChildEval {
                parent,
                child,
                script,
            }) => {
                if let Some(surface) = children.get(&child)
                    && surface.parent == parent
                {
                    let _ = surface.webview.evaluate_script(&script);
                }
            }
            Event::UserEvent(HostEvent::Zoom {
                window,
                factor,
                reply,
            }) => {
                let accepted = windows
                    .get(&window)
                    .is_some_and(|entry| entry.webview.zoom(factor).is_ok());
                let _ = reply.send(accepted);
            }
            #[cfg(all(test, target_os = "linux"))]
            Event::UserEvent(HostEvent::ZoomLevel { window, reply }) => {
                let level = windows.get(&window).and_then(|entry| {
                    #[cfg(target_os = "linux")]
                    {
                        use gtk::prelude::*;
                        use wry::WebViewExtUnix as _;

                        Some(entry.webview.webview().property::<f64>("zoom-level"))
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        let _ = entry;
                        None
                    }
                });
                let _ = reply.send(level);
            }
            Event::UserEvent(HostEvent::DevTools {
                window,
                action,
                reply,
            }) => {
                let answer = windows.get(&window).map(|entry| match action {
                    DevToolsAction::Open => {
                        entry.webview.open_devtools();
                        // wry's Windows backend opens the inspector but reports no state,
                        // so this is an acceptance, not a verified state.
                        true
                    }
                    DevToolsAction::Close => {
                        entry.webview.close_devtools();
                        true
                    }
                    DevToolsAction::Query => entry.webview.is_devtools_open(),
                });
                let _ = reply.send(answer.unwrap_or(false));
            }
            #[cfg(all(feature = "wry-engine", target_os = "macos"))]
            Event::UserEvent(HostEvent::Quit) => {
                *control_flow = ControlFlow::ExitWithCode(QUIT_CODE.load(Ordering::SeqCst));
            }
            #[cfg(feature = "tray-icon")]
            Event::UserEvent(HostEvent::Tray { request, reply }) => {
                let _ = reply.send(show_tray(request, &mut tray, proxy.clone()));
            }
            #[cfg(feature = "tray-icon")]
            Event::UserEvent(HostEvent::TrayClicked) => {
                tray_was_clicked(&mut windows, tray.is_some());
            }
            #[cfg(feature = "native-menu")]
            Event::UserEvent(HostEvent::SetMenu {
                window,
                request,
                reply,
            }) => {
                let _ = reply.send(crate::menu::set_window_menu(&mut windows, window, request));
            }
            #[cfg(feature = "native-menu")]
            Event::UserEvent(HostEvent::MenuActivated { id }) => {
                crate::menu::menu_was_activated(&windows, &id);
            }
            Event::UserEvent(HostEvent::Theme { window, reply }) => {
                answer_theme(&windows, window, &reply);
            }
            Event::UserEvent(HostEvent::Monitors { reply }) => answer_monitors(target, &reply),
            Event::UserEvent(HostEvent::Geometry { window, reply }) => {
                answer_geometry(&windows, window, &reply);
            }
            Event::UserEvent(HostEvent::WindowState {
                window,
                change,
                reply,
            }) => answer_window_state(&windows, window, change, &reply),
            Event::UserEvent(HostEvent::WindowShape {
                window,
                shape,
                reply,
            }) => {
                answer_window_shape(&windows, window, &shape, &reply);
            }
            // The user changed the machine's theme. The page is told rather than left to ask: it is
            // the user's setting for the whole machine, and a page that kept its own copy would keep
            // drawing the theme the user just left.
            Event::WindowEvent {
                window_id,
                event: WindowEvent::ThemeChanged(theme),
                ..
            } => theme_changed(&windows, window_id, theme),
            // The user resized the window. The page is told so it can lay itself out for the size it
            // now has rather than for the one it was built with, and while the user is still dragging.
            Event::WindowEvent {
                window_id,
                event: WindowEvent::Resized(size),
                ..
            } => {
                if let Some(entry) = window_entry(&windows, window_id) {
                    let logical = size.to_logical::<f64>(entry.window.scale_factor());
                    tell_page(&windows, window_id, &resized_script(logical));
                }
            }
            // The user moved the window. The counterpart of the resize, for a page that keeps
            // something anchored to where the window is.
            Event::WindowEvent {
                window_id,
                event: WindowEvent::Moved(position),
                ..
            } => {
                if let Some(entry) = window_entry(&windows, window_id) {
                    let logical = position.to_logical::<f64>(entry.window.scale_factor());
                    tell_page(&windows, window_id, &moved_script(logical));
                }
            }
            // The user dropped files onto the window. The page is told so it can
            // process them. Each path is a file the user dragged from the OS.
            Event::WindowEvent {
                window_id,
                event: WindowEvent::DroppedFile(path),
                ..
            } => {
                if let Some(_entry) = window_entry(&windows, window_id) {
                    tell_page(&windows, window_id, &dropped_files_script(&[path]));
                }
            }
            Event::WindowEvent {
                window_id,
                event: WindowEvent::CloseRequested,
                ..
            } => {
                // The user closed a window. The host is no longer alive, and a window that
                // reports itself as serving after its engine is gone is a window an
                // application would wait on forever.
                let closed = windows
                    .iter()
                    .find(|(_, entry)| entry.window.id() == window_id)
                    .map(|(id, _)| *id);
                if let Some(id) = closed {
                    children.retain(|_, child| child.parent != id);
                    if let Some(entry) = windows.remove(&id) {
                        entry.alive.store(false, Ordering::SeqCst);
                    }
                }
            }
            _ => {}
        }
    });
}

/// Answers a request for the displays the engine is on.
///
/// The event loop's own target is the only thing that knows which displays this process can put a
/// window on, and it is available here and nowhere else.
#[cfg(feature = "wry-engine")]
fn answer_monitors(
    target: &EventLoopWindowTarget<HostEvent>,
    reply: &Sender<Vec<ncui_webview::MonitorRect>>,
) {
    let monitor_rect = |monitor: tao::monitor::MonitorHandle| {
        let scale = monitor.scale_factor();
        let position = monitor.position().to_logical::<f64>(scale);
        let size = monitor.size().to_logical::<f64>(scale);
        ncui_webview::MonitorRect {
            x: position.x.round() as i32,
            y: position.y.round() as i32,
            width: size.width.round() as u32,
            height: size.height.round() as u32,
        }
    };
    let mut monitors: Vec<_> = target.available_monitors().map(&monitor_rect).collect();
    if monitors.is_empty() {
        // Windows can fail display enumeration while still identifying the primary display.
        // Leaving the list empty would keep a remembered off-screen window unreachable.
        monitors.extend(target.primary_monitor().map(&monitor_rect));
    }
    let _ = reply.send(monitors);
}

/// Answers a request for one window's geometry.
///
/// The size is the window's inner size and the position its outer position: what a user would
/// recognise as "where the window is", rather than the frame's origin, which moves with the
/// platform's decoration. Both are converted here, where the scale factor is known, so the port
/// only ever speaks logical pixels.
#[cfg(feature = "wry-engine")]
fn answer_geometry(
    windows: &HashMap<u64, EngineWindow>,
    window: u64,
    reply: &Sender<Option<Geometry>>,
) {
    let answer = windows.get(&window).map(|entry| {
        #[cfg(target_os = "linux")]
        {
            use tao::platform::unix::WindowExtUnix as _;

            gtk_geometry(entry.window.gtk_window())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let scale = entry.window.scale_factor();
            let size = entry.window.inner_size().to_logical::<f64>(scale);
            let position = entry
                .window
                .outer_position()
                .ok()
                .map(|point| point.to_logical::<f64>(scale));
            Geometry {
                x: position.map_or(0, |point| point.x.round() as i32),
                y: position.map_or(0, |point| point.y.round() as i32),
                width: size.width.round() as u32,
                height: size.height.round() as u32,
            }
        }
    });
    let _ = reply.send(answer);
}

/// Builds a window and its page, and reports its identity or why it could not be built.
///
/// The failure is recorded twice on purpose: on the host that asked, where the caller reads it, and
/// in the process-wide last error, where a caller with no handle to that host can find it.
#[cfg(feature = "wry-engine")]
fn create_window(
    target: &EventLoopWindowTarget<HostEvent>,
    create: CreateRequest,
    windows: &mut HashMap<u64, EngineWindow>,
) {
    let CreateRequest {
        request,
        seam,
        ipc_tx,
        menu_tx,
        alive,
        error,
        reply,
    } = create;
    #[cfg(not(feature = "native-menu"))]
    let _ = menu_tx;
    let id = NEXT_WINDOW.fetch_add(1, Ordering::SeqCst);
    match build_window(target, request.as_ref(), &seam, ipc_tx, &error) {
        Ok((window, webview, cdp)) => {
            windows.insert(
                id,
                EngineWindow {
                    #[cfg(feature = "native-menu")]
                    menu: None,
                    window,
                    webview,
                    cdp,
                    alive: Arc::clone(&alive),
                    #[cfg(feature = "native-menu")]
                    menu_tx,
                },
            );
            alive.store(true, Ordering::SeqCst);
            let _ = reply.send(Ok(id));
        }
        Err(message) => {
            if let Ok(mut guard) = error.lock() {
                *guard = Some(message.clone());
            }
            record_error(message.clone());
            let _ = reply.send(Err(message));
        }
    }
}

/// Tells a window's page that the machine's theme changed.
///
/// The page is told rather than left to ask: the theme is the user's setting for the whole machine,
/// and a page that kept its own copy would keep drawing the theme the user just left.
#[cfg(feature = "wry-engine")]
/// The window the platform named, when this host still has it.
///
/// A window event arrives with the platform's own id, and the host keys its windows by its own:
/// this is the one place that translates between them, so a window that has been closed is simply
/// not found rather than reported as an error.
#[cfg(feature = "wry-engine")]
fn window_entry(
    windows: &HashMap<u64, EngineWindow>,
    window_id: tao::window::WindowId,
) -> Option<&EngineWindow> {
    windows
        .values()
        .find(|entry| entry.window.id() == window_id)
}

/// Sends one script to the page in the window the platform named.
#[cfg(feature = "wry-engine")]
fn tell_page(windows: &HashMap<u64, EngineWindow>, window_id: tao::window::WindowId, script: &str) {
    if let Some(entry) = window_entry(windows, window_id) {
        let _ = entry.webview.evaluate_script(script);
    }
}

/// Tells one window's page which theme the machine is drawing.
#[cfg(feature = "wry-engine")]
fn theme_changed(
    windows: &HashMap<u64, EngineWindow>,
    window_id: tao::window::WindowId,
    theme: tao::window::Theme,
) {
    if let Some(script) = theme_script(theme) {
        tell_page(windows, window_id, &script);
    }
}

/// Reports the platform's theme for one window, if it has one.
#[cfg(feature = "wry-engine")]
fn answer_theme(
    windows: &HashMap<u64, EngineWindow>,
    window: u64,
    reply: &Sender<Option<ncui_webview::Theme>>,
) {
    let _ = reply.send(
        windows
            .get(&window)
            .and_then(|entry| theme_of(entry.window.theme())),
    );
}

/// The theme a page is told about, from the platform's own.
///
/// A pure mapping so it can be tested without a window: what the platform calls its themes is the
/// platform's business, and this is the one place that translates it.
///
/// `None` for a theme this runtime does not know. The platform's enum is open, so a theme added to
/// it would arrive here, and mapping an unknown one to "light" would tell a page to follow a setting
/// the user does not have. Saying nothing is the honest answer to a question this cannot read.
#[cfg(feature = "wry-engine")]
const fn theme_of(theme: tao::window::Theme) -> Option<ncui_webview::Theme> {
    match theme {
        tao::window::Theme::Dark => Some(ncui_webview::Theme::Dark),
        tao::window::Theme::Light => Some(ncui_webview::Theme::Light),
        _ => None,
    }
}

/// What a page hears when the machine's theme changes, or `None` when the new theme is not one this
/// runtime knows.
#[cfg(feature = "wry-engine")]
fn theme_script(theme: tao::window::Theme) -> Option<String> {
    let theme = theme_of(theme)?;
    Some(format!(
        "window.ncui && window.ncui.__dispatch({{\"event\":\"theme\",\"payload\":{{\"theme\":\"{}\"}}}});",
        theme.as_str()
    ))
}

/// Answers a request for one window's presentation, applying a change first when one was asked for.
#[cfg(feature = "wry-engine")]
fn answer_window_state(
    windows: &HashMap<u64, EngineWindow>,
    window: u64,
    change: Option<ncui_webview::WindowChange>,
    reply: &Sender<Option<ncui_webview::WindowState>>,
) {
    let Some(entry) = windows.get(&window) else {
        let _ = reply.send(None);
        return;
    };
    if let Some(change) = change {
        apply_window_change(&entry.window, change);
        #[cfg(target_os = "linux")]
        {
            use tao::platform::unix::WindowExtUnix as _;

            let gtk_window = (*entry.window.gtk_window()).clone();
            let alive = Arc::clone(&entry.alive);
            let reply = (*reply).clone();
            let deadline = Instant::now() + LINUX_WINDOW_SETTLE_TIMEOUT;
            let _ = gtk::glib::timeout_add_local(LINUX_WINDOW_POLL_INTERVAL, move || {
                use gtk::glib::ControlFlow;

                if !alive.load(Ordering::Acquire) {
                    let _ = reply.send(None);
                    return ControlFlow::Break;
                }
                let state = read_gtk_window_state(&gtk_window);
                if window_change_applied(state, change) || Instant::now() >= deadline {
                    let _ = reply.send(Some(state));
                    ControlFlow::Break
                } else {
                    ControlFlow::Continue
                }
            });
            return;
        }
    }
    let _ = reply.send(Some(read_window_state(&entry.window)));
}

/// Applies a shape and, on Linux, acknowledges it after the window manager has had a chance to
/// report the resulting geometry and state.
#[cfg(feature = "wry-engine")]
fn answer_window_shape(
    windows: &HashMap<u64, EngineWindow>,
    window: u64,
    shape: &ncui_webview::WindowShape,
    reply: &Sender<bool>,
) {
    let Some(entry) = windows.get(&window) else {
        let _ = reply.send(false);
        return;
    };
    apply_window_shape(&entry.window, shape);
    #[cfg(target_os = "linux")]
    {
        use tao::platform::unix::WindowExtUnix as _;

        let gtk_window = (*entry.window.gtk_window()).clone();
        let alive = Arc::clone(&entry.alive);
        let reply = (*reply).clone();
        let deadline = Instant::now() + LINUX_WINDOW_SETTLE_TIMEOUT;
        let shape = (*shape).clone();
        let _ = gtk::glib::timeout_add_local(LINUX_WINDOW_POLL_INTERVAL, move || {
            use gtk::glib::ControlFlow;

            if !alive.load(Ordering::Acquire) {
                let _ = reply.send(false);
                return ControlFlow::Break;
            }
            if window_shape_applied(&gtk_window, &shape) || Instant::now() >= deadline {
                let _ = reply.send(true);
                ControlFlow::Break
            } else {
                ControlFlow::Continue
            }
        });
        return;
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = reply.send(true);
    }
}

/// Asks the platform for one change to a window's presentation.
///
/// Borderless fullscreen, for the reason a created fullscreen window is borderless: an exclusive
/// fullscreen window takes the display away from the user's other windows.
#[cfg(feature = "wry-engine")]
fn apply_window_change(window: &Window, change: ncui_webview::WindowChange) {
    #[cfg(target_os = "linux")]
    {
        use gtk::prelude::*;
        use tao::platform::unix::WindowExtUnix as _;

        let window = window.gtk_window();
        match change {
            ncui_webview::WindowChange::Fullscreen(true) => window.fullscreen(),
            ncui_webview::WindowChange::Fullscreen(false) => window.unfullscreen(),
            ncui_webview::WindowChange::Maximized(maximized) => window.set_maximized(maximized),
            ncui_webview::WindowChange::Minimized(true) => window.iconify(),
            ncui_webview::WindowChange::Minimized(false) => window.deiconify(),
            ncui_webview::WindowChange::Visible(true) => window.show_all(),
            ncui_webview::WindowChange::Visible(false) => {
                window.hide();
            }
            ncui_webview::WindowChange::Focus => {
                window.present_with_time(gtk::gdk::ffi::GDK_CURRENT_TIME as _);
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    match change {
        ncui_webview::WindowChange::Fullscreen(true) => {
            window.set_fullscreen(Some(tao::window::Fullscreen::Borderless(None)));
        }
        ncui_webview::WindowChange::Fullscreen(false) => window.set_fullscreen(None),
        ncui_webview::WindowChange::Maximized(maximized) => window.set_maximized(maximized),
        ncui_webview::WindowChange::Minimized(minimized) => window.set_minimized(minimized),
        ncui_webview::WindowChange::Visible(visible) => window.set_visible(visible),
        ncui_webview::WindowChange::Focus => window.set_focus(),
    }
}

/// Asks the platform to make a window take a shape.
///
/// Only the fields the caller named are applied: a shape that asks about the title must not move a
/// window the user has moved. The position is set before the size, so a window that is being moved
/// and resized lands where it was asked for rather than being pushed by its own frame.
#[cfg(feature = "wry-engine")]
fn apply_window_shape(window: &Window, shape: &ncui_webview::WindowShape) {
    #[cfg(target_os = "linux")]
    {
        use gtk::prelude::*;
        use tao::platform::unix::WindowExtUnix as _;

        let gtk_window = window.gtk_window();
        if let Some(title) = &shape.title {
            gtk_window.set_title(title);
        }
        if let (Some(x), Some(y)) = (shape.x, shape.y) {
            gtk_window.move_(x, y);
        }
        if let (Some(width), Some(height)) = (shape.width, shape.height) {
            gtk_window.resize(
                i32::try_from(width).unwrap_or(i32::MAX),
                i32::try_from(height).unwrap_or(i32::MAX),
            );
        }
        if let Some(always_on_top) = shape.always_on_top {
            gtk_window.set_keep_above(always_on_top);
        }
        if let Some(resizable) = shape.resizable {
            gtk_window.set_resizable(resizable);
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        if let Some(title) = &shape.title {
            window.set_title(title);
        }
        if let (Some(x), Some(y)) = (shape.x, shape.y) {
            window.set_outer_position(tao::dpi::LogicalPosition::new(f64::from(x), f64::from(y)));
        }
        if let (Some(width), Some(height)) = (shape.width, shape.height) {
            window.set_inner_size(tao::dpi::LogicalSize::new(
                f64::from(width),
                f64::from(height),
            ));
        }
        if let Some(always_on_top) = shape.always_on_top {
            window.set_always_on_top(always_on_top);
        }
        if let Some(resizable) = shape.resizable {
            window.set_resizable(resizable);
        }
    }
}

/// Reads how the platform is presenting a window right now.
///
/// The platform's answer, not the runtime's request: the user maximises, minimises and focuses
/// windows by hand, and a runtime that reported what it asked for would disagree with the screen.
#[cfg(feature = "wry-engine")]
fn read_window_state(window: &Window) -> ncui_webview::WindowState {
    #[cfg(target_os = "linux")]
    {
        use tao::platform::unix::WindowExtUnix as _;

        read_gtk_window_state(window.gtk_window())
    }
    #[cfg(not(target_os = "linux"))]
    ncui_webview::WindowState {
        fullscreen: window.fullscreen().is_some(),
        maximized: window.is_maximized(),
        minimized: window.is_minimized(),
        visible: window.is_visible(),
        decorated: window.is_decorated(),
        resizable: window.is_resizable(),
        always_on_top: window.is_always_on_top(),
        focused: window.is_focused(),
    }
}

#[cfg(all(feature = "wry-engine", target_os = "linux"))]
fn read_gtk_window_state(window: &gtk::ApplicationWindow) -> ncui_webview::WindowState {
    use gtk::prelude::*;

    let state = window
        .window()
        .map_or_else(gtk::gdk::WindowState::empty, |window| window.state());
    ncui_webview::WindowState {
        fullscreen: state.contains(gtk::gdk::WindowState::FULLSCREEN),
        maximized: state.contains(gtk::gdk::WindowState::MAXIMIZED),
        minimized: state.contains(gtk::gdk::WindowState::ICONIFIED),
        visible: window.get_visible(),
        decorated: window.is_decorated(),
        resizable: window.is_resizable(),
        always_on_top: state.contains(gtk::gdk::WindowState::ABOVE),
        focused: window.is_active(),
    }
}

#[cfg(all(feature = "wry-engine", target_os = "linux"))]
fn gtk_geometry(window: &gtk::ApplicationWindow) -> Geometry {
    use gtk::prelude::*;

    let allocation = window.allocation();
    let position = window.position();
    Geometry {
        x: position.0,
        y: position.1,
        width: u32::try_from(allocation.width()).unwrap_or(0),
        height: u32::try_from(allocation.height()).unwrap_or(0),
    }
}

#[cfg(all(feature = "wry-engine", target_os = "linux"))]
fn window_change_applied(
    state: ncui_webview::WindowState,
    change: ncui_webview::WindowChange,
) -> bool {
    match change {
        ncui_webview::WindowChange::Fullscreen(value) => state.fullscreen == value,
        ncui_webview::WindowChange::Maximized(value) => state.maximized == value,
        ncui_webview::WindowChange::Minimized(value) => state.minimized == value,
        ncui_webview::WindowChange::Visible(value) => state.visible == value,
        ncui_webview::WindowChange::Focus => state.focused,
    }
}

#[cfg(all(feature = "wry-engine", target_os = "linux"))]
fn window_shape_applied(
    window: &gtk::ApplicationWindow,
    shape: &ncui_webview::WindowShape,
) -> bool {
    use gtk::prelude::*;

    if shape
        .title
        .as_ref()
        .is_some_and(|title| window.title().as_deref() != Some(title.as_str()))
    {
        return false;
    }
    let geometry = gtk_geometry(window);
    if let (Some(x), Some(y)) = (shape.x, shape.y)
        && (geometry.x != x || geometry.y != y)
    {
        return false;
    }
    if shape
        .width
        .is_some_and(|width| geometry.width.abs_diff(width) > 1)
        || shape
            .height
            .is_some_and(|height| geometry.height.abs_diff(height) > 1)
    {
        return false;
    }
    let state = read_gtk_window_state(window);
    shape
        .always_on_top
        .is_none_or(|value| state.always_on_top == value)
        && shape.resizable.is_none_or(|value| state.resizable == value)
}

/// Builds one window and its webview on the engine's thread.
#[cfg(feature = "wry-engine")]
#[allow(
    clippy::too_many_lines,
    reason = "the native initialiser reports every failure mode in one place"
)]
fn build_window(
    target: &EventLoopWindowTarget<HostEvent>,
    request: Option<&ncui_webview::WindowRequest>,
    seam: &Arc<HostSeam>,
    ipc_tx: Sender<HostIpcEvent>,
    error: &Arc<Mutex<Option<String>>>,
) -> Result<(Window, WebView, PageTargetBinding), String> {
    // The window's shape comes from the request, not from the host's own defaults. A host
    // that invented its own title and size would show the user something the application
    // never asked for.
    let title = request.map_or_else(
        || ncui_webview::DEFAULT_WINDOW_TITLE.to_owned(),
        |r| r.title.clone(),
    );
    let size = request.map_or(
        (
            ncui_webview::DEFAULT_WINDOW_WIDTH,
            ncui_webview::DEFAULT_WINDOW_HEIGHT,
        ),
        |r| (r.width, r.height),
    );
    let position = request.as_ref().and_then(|r| match (r.x, r.y) {
        (Some(x), Some(y)) => Some((x, y)),
        _ => None,
    });
    let options = request
        .as_ref()
        .map_or_else(WindowOptions::default, |r| WindowOptions {
            always_on_top: r.always_on_top,
            decorations: r.decorations,
            transparent: r.transparent,
            resizable: r.resizable,
            minimum: match (r.min_width, r.min_height) {
                (Some(width), Some(height)) => Some((width, height)),
                _ => None,
            },
            fullscreen: r.fullscreen,
            maximized: r.maximized,
        });

    let builder =
        WindowBuilder::new()
            .with_title(&title)
            .with_inner_size(tao::dpi::LogicalSize::new(
                f64::from(size.0),
                f64::from(size.1),
            ));
    // What the caller asked the window to be. Each of these is a property the platform applies
    // when the window is created; none of them is invented here, and a window that ignores one
    // would be a window the caller did not ask for.
    let builder = builder
        .with_always_on_top(options.always_on_top)
        .with_decorations(options.decorations)
        .with_transparent(options.transparent)
        .with_resizable(options.resizable);
    let builder = match options.minimum {
        Some((width, height)) => builder.with_min_inner_size(tao::dpi::LogicalSize::new(
            f64::from(width),
            f64::from(height),
        )),
        None => builder,
    };
    // A window asked for filling its display is made fullscreen by the platform at creation, so
    // the user never sees it in a frame first. Borderless rather than an exclusive mode: an
    // exclusive fullscreen window takes the display from the user's other windows, which is a
    // decision an application has no business making on its own.
    let builder = if options.fullscreen {
        builder.with_fullscreen(Some(tao::window::Fullscreen::Borderless(None)))
    } else {
        builder
    };
    let builder = builder.with_maximized(options.maximized);

    // The window's icon, when the request carries one. The pixels are handed to the platform as
    // they are: the request carries straight RGBA, which is exactly what the platform's own call
    // takes, so nothing is converted here and nothing can be lost in the conversion.
    let builder = match request.and_then(|request| request.icon.as_ref()) {
        Some(icon) => {
            let icon = tao::window::Icon::from_rgba(icon.rgba.clone(), icon.width, icon.height)
                .map_err(|error| format!("the window's icon cannot be used: {error}"))?;
            builder.with_window_icon(Some(icon))
        }
        None => builder,
    };

    // A window the caller asked to place is placed; one it did not is left to the platform,
    // which puts it where the user's window manager thinks a new window belongs.
    let builder = match position {
        Some((x, y)) => {
            builder.with_position(tao::dpi::LogicalPosition::new(f64::from(x), f64::from(y)))
        }
        None => builder,
    };
    let window = builder.build(target).map_err(|err| err.to_string())?;

    // The engine asks the seam. It does not keep a copy of the rules, so the answer the
    // engine obeys and the answer the runtime gives a page cannot disagree.
    //
    // The one navigation that is not the page's is the window's own initial load: the
    // host asked for that URL, and the allow-list governs what a *page* may reach, not
    // what the host may open. Refusing it would leave the window blank and the policy
    // would be answering a question nobody asked.
    let initial = request.map_or_else(|| "about:blank".to_owned(), |request| request.url.clone());
    let navigation_seam = Arc::clone(seam);
    let navigation_handler = move |target: String| -> bool {
        if target == initial {
            return true;
        }
        let Some(request) = ncui_webview::NavigationRequest::from_url(&target, true) else {
            // A URL the policy cannot read is refused. Guessing would mean guessing about
            // what the page is allowed to reach.
            return false;
        };
        navigation_seam.evaluate_navigation(&request).is_allowed()
    };

    // One user-data directory for the process's browsers, under the runtime's own data
    // root. A directory the runtime owns is what makes the CDP port findable: WebView2
    // writes `DevToolsActivePort` into it, and the transport reads it from there rather
    // than guessing. It also keeps this application's browser state separate from every
    // other WebView2 consumer on the machine, which a shared default would not. The
    // context outlives the builder because the builder only borrows it.
    let dir = webview_data_root();
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(target_os = "windows")]
    let before_targets = page_targets_before_window(&dir);
    let mut web_context = wry::WebContext::new(Some(dir.clone()));
    let builder = WebViewBuilder::new_with_web_context(&mut web_context)
        .with_ipc_handler(move |request: wry::http::Request<String>| {
            // Bridge the page's IPC payload into the in-process channel so the runtime can
            // evaluate it through the seam.
            let _ = ipc_tx.send(request.body().to_owned());
        })
        .with_navigation_handler(navigation_handler)
        .with_initialization_script(format!(
            "{BRIDGE_SCRIPT}
{DRAG_REGION_SCRIPT}"
        ));

    // Remote debugging (ADR-017 Phase 2): an ephemeral port keeps two instances of the
    // same application from colliding, and the chosen port is published by the runtime in
    // the user-data directory's `DevToolsActivePort` file, which the CDP transport reads.
    // The default arguments are repeated because supplying any argument replaces wry's
    // defaults rather than adding to them.
    #[cfg(target_os = "windows")]
    let builder = {
        use wry::WebViewBuilderExtWindows as _;
        builder.with_additional_browser_args(
            "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection \
             --remote-debugging-port=0",
        )
    };

    let builder = match request {
        Some(request) => builder.with_url(request.url.clone()),
        None => builder.with_url("about:blank"),
    };

    let webview = {
        #[cfg(any(
            target_os = "windows",
            target_os = "macos",
            target_os = "ios",
            target_os = "android"
        ))]
        {
            builder.build(&window)
        }
        #[cfg(not(any(
            target_os = "windows",
            target_os = "macos",
            target_os = "ios",
            target_os = "android"
        )))]
        {
            use tao::platform::unix::WindowExtUnix;
            use wry::WebViewBuilderExtUnix;
            let Some(vbox) = window.default_vbox() else {
                // No visual box means no webview, and saying so is more useful than waiting
                // for a timeout to report nothing.
                return Err("the window has no visual box, so no webview can be created".to_owned());
            };
            builder.build_gtk(vbox)
        }
    }
    .map_err(|err| {
        let message = err.to_string();
        if let Ok(mut guard) = error.lock() {
            *guard = Some(message.clone());
        }
        message
    })?;

    // The browser endpoint is shared; the page identity is not. Bind this window to the
    // exactly-one page target that appeared while the event loop was serialized in this build.
    #[cfg(target_os = "windows")]
    let cdp = match before_targets {
        Ok(before) => bind_page_target_after_window(&dir, &before),
        Err(error) => PageTargetBinding::Failed(error),
    };
    #[cfg(not(target_os = "windows"))]
    let cdp = PageTargetBinding::Failed(crate::cdp::platform_unsupported());

    // Adapter half of the composition boundary: create a wgpu surface from the window and
    // present one frame, proving the renderer can draw into the window the page lives in.
    // The DOM side (telling the page which pixels the GPU owns) is a separate step; this
    // only establishes that the surface exists and accepts a frame.
    //
    // Once per process, not once per window: what it establishes — that a window's surface accepts
    // a frame on this machine — is a fact about the machine, and repeating it for every window
    // would bring up a `wgpu` device per window on the engine's single thread. An application that
    // opens several windows would pay that on each one, and every request queued behind it would
    // wait. A surface a window *keeps* belongs to the DOM-compositing step, which is not built.
    #[cfg(feature = "gpu-surface")]
    present_one_gpu_frame_once(&window);

    Ok((window, webview, cdp))
}

/// Presents one frame into the first window this process creates, and no other.
///
/// Guarded by a process-wide flag rather than by anything about the window, because the thing being
/// established is a property of the machine and the driver: whether a window's surface here accepts
/// a frame at all.
#[cfg(all(feature = "wry-engine", feature = "gpu-surface"))]
fn present_one_gpu_frame_once(window: &Window) {
    static PRESENTED: AtomicBool = AtomicBool::new(false);
    if PRESENTED.swap(true, Ordering::SeqCst) {
        return;
    }
    present_one_gpu_frame(window);
}

/// Presents one frame into a window through `wgpu`.
#[cfg(all(feature = "wry-engine", feature = "gpu-surface"))]
#[allow(
    clippy::too_many_lines,
    reason = "the device setup reports each failure by doing nothing, and is read as one unit"
)]
fn present_one_gpu_frame(window: &Window) {
    let instance = wgpu::Instance::default();
    if let Ok(surface) = instance.create_surface(window) {
        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }));
        if let Ok(adapter) = adapter {
            let device_result = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("ncui-webview-wry-gpu"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::default(),
                trace: wgpu::Trace::Off,
                ..Default::default()
            }));
            if let Ok((device, queue)) = device_result {
                let size = window.inner_size();
                let format = surface.get_capabilities(&adapter).formats[0];
                let config = wgpu::SurfaceConfiguration {
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    format,
                    width: size.width.max(1),
                    height: size.height.max(1),
                    present_mode: wgpu::PresentMode::Fifo,
                    alpha_mode: wgpu::CompositeAlphaMode::Auto,
                    view_formats: vec![],
                    desired_maximum_frame_latency: 2,
                };
                surface.configure(&device, &config);
                if let wgpu::CurrentSurfaceTexture::Success(frame)
                | wgpu::CurrentSurfaceTexture::Suboptimal(frame) = surface.get_current_texture()
                {
                    let view = frame
                        .texture
                        .create_view(&wgpu::TextureViewDescriptor::default());
                    let mut encoder =
                        device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                            label: Some("ncui-gpu-clear"),
                        });
                    {
                        let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                            label: Some("ncui-gpu-clear-pass"),
                            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                view: &view,
                                resolve_target: None,
                                ops: wgpu::Operations {
                                    load: wgpu::LoadOp::Clear(wgpu::Color {
                                        r: 0.0,
                                        g: 0.0,
                                        b: 0.0,
                                        a: 0.0,
                                    }),
                                    store: wgpu::StoreOp::Store,
                                },
                                depth_slice: None,
                            })],
                            depth_stencil_attachment: None,
                            occlusion_query_set: None,
                            timestamp_writes: None,
                            multiview_mask: None,
                        });
                    }
                    queue.submit(std::iter::once(encoder.finish()));
                    frame.present();
                }
            }
        }
    }
}

/// One window's engine handle.
///
/// A host is one window: it holds the seam the page obeys and the channel the page talks
/// on. The engine behind it is shared with every other window in the process.
#[derive(Debug)]
pub struct WindowHost {
    alive: Arc<AtomicBool>,
    last_error: Arc<Mutex<Option<String>>>,
    /// Producer side retained so callers can post into the same channel if needed.
    ipc_tx: Sender<HostIpcEvent>,
    /// Consumer side for IPC events forwarded from the `wry` IPC handler.
    ///
    /// Behind a mutex because the trait requires `Send` and a `Receiver` is `Send` but not
    /// `Sync`; the lock is only ever held for the length of one `try_recv`.
    ipc_rx: Mutex<Receiver<HostIpcEvent>>,
    /// Child-page receivers, keyed by the canonical identities issued by the engine.
    ///
    /// Each child has its own channel so a parent can poll one child without consuming another's
    /// messages. The mutex supplies the `Sync` required by the backend trait.
    child_ipc_rx: Mutex<HashMap<u64, Receiver<ChildPageMessage>>>,
    /// Producer retained so the menu channel stays connected while the engine owns the window.
    menu_tx: Sender<ncui_webview::MenuEvent>,
    /// Consumer for native selections routed to this window by the engine.
    menu_rx: Mutex<Receiver<ncui_webview::MenuEvent>>,
    /// The seam the page speaks through, shared with the window that owns this host.
    seam: Arc<HostSeam>,
    /// The process-wide engine this window lives in.
    #[cfg(feature = "wry-engine")]
    engine: Option<EventLoopProxy<HostEvent>>,
    /// This window's identity inside the engine.
    #[cfg(feature = "wry-engine")]
    window: Option<u64>,
}

impl Default for WindowHost {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowHost {
    /// Create (and start) a host with a default, empty page.
    ///
    /// The window is created by the process-wide engine; this call returns once the engine
    /// has either created it (success) or reported a failure (stored and returned via
    /// [`WebViewBackend::last_error`]).
    pub fn new() -> Self {
        #[cfg(feature = "wry-engine")]
        {
            let seam = Arc::new(HostSeam::disabled());
            spawn_real(None, seam)
        }
        #[cfg(not(feature = "wry-engine"))]
        {
            let (ipc_tx, ipc_rx) = mpsc::channel::<HostIpcEvent>();
            let (menu_tx, menu_rx) = mpsc::channel::<ncui_webview::MenuEvent>();
            Self {
                alive: Arc::new(AtomicBool::new(false)),
                last_error: Arc::new(Mutex::new(Some(
                    "wry-engine feature is disabled; this host cannot create a window".to_owned(),
                ))),
                ipc_tx,
                ipc_rx: Mutex::new(ipc_rx),
                child_ipc_rx: Mutex::new(HashMap::new()),
                menu_tx,
                menu_rx: Mutex::new(menu_rx),
                seam: Arc::new(HostSeam::disabled()),
            }
        }
    }

    /// The seam this host's page speaks through.
    #[must_use]
    pub fn seam(&self) -> &Arc<HostSeam> {
        &self.seam
    }

    /// Send JavaScript to be evaluated in the page. No-op when the feature is disabled or the
    /// host has died.
    pub fn eval(&self, script: &str) {
        #[cfg(feature = "wry-engine")]
        {
            if let (Some(engine), Some(window)) = (&self.engine, self.window) {
                let _ = engine.send_event(HostEvent::Eval {
                    window,
                    script: script.to_owned(),
                });
            }
        }
        #[cfg(not(feature = "wry-engine"))]
        {
            let _ = script;
        }
    }

    /// Initiate a window drag (titlebar drag region).
    ///
    /// Sends a `HostEvent::DragWindow` to the process-wide engine event loop,
    /// which calls `tao::window::Window::drag_window()`. This is called by the
    /// page when the user mousedowns on a `[data-ncui-drag-region]` element.
    #[cfg(feature = "wry-engine")]
    pub fn drag_window(&self) -> bool {
        if let (Some(engine), Some(window)) = (&self.engine, self.window) {
            engine.send_event(HostEvent::DragWindow { window }).is_ok()
        } else {
            false
        }
    }

    /// Whether the platform is drawing its interface light or dark, as it reports it.
    ///
    /// `None` when the engine has gone or has stopped answering: a window whose host cannot say is a
    /// window whose theme nobody knows, which is a different answer from "light".
    #[must_use]
    pub fn window_theme(&self) -> Option<ncui_webview::Theme> {
        #[cfg(feature = "wry-engine")]
        {
            let (engine, window) = (self.engine.as_ref()?, self.window?);
            let (reply, answers) = mpsc::channel();
            engine.send_event(HostEvent::Theme { window, reply }).ok()?;
            answers.recv_timeout(ENGINE_REQUEST_TIMEOUT).ok()?
        }
        #[cfg(not(feature = "wry-engine"))]
        {
            None
        }
    }

    /// Shows the application's tray icon, or removes it, reporting whether the platform agreed.
    ///
    /// The answer comes back from the engine thread, which owns the platform's event loop: the tray
    /// is created there and lives there, because a tray icon registered anywhere else has no loop to
    /// belong to. `false` means the platform refused it or this build cannot show one at all.
    #[cfg(feature = "tray-icon")]
    #[must_use]
    pub fn tray(&self, request: Option<ncui_webview::TrayRequest>) -> bool {
        let Some(engine) = self.engine.as_ref() else {
            return false;
        };
        let (reply, answers) = mpsc::channel();
        if engine
            .send_event(HostEvent::Tray { request, reply })
            .is_err()
        {
            return false;
        }
        matches!(answers.recv_timeout(ENGINE_REQUEST_TIMEOUT), Ok(Ok(())))
    }

    /// Attaches or removes this window's native menu, reporting whether the engine accepted it.
    #[cfg(feature = "native-menu")]
    #[must_use]
    pub fn menu(&self, request: Option<ncui_webview::MenuRequest>) -> bool {
        let (Some(engine), Some(window)) = (&self.engine, self.window) else {
            return false;
        };
        let (reply, answers) = mpsc::channel();
        if engine
            .send_event(HostEvent::SetMenu {
                window,
                request,
                reply,
            })
            .is_err()
        {
            return false;
        }
        matches!(answers.recv_timeout(ENGINE_REQUEST_TIMEOUT), Ok(Ok(())))
    }

    /// Takes the next native menu selection routed to this window.
    #[must_use]
    pub fn poll_menu_event(&self) -> Option<ncui_webview::MenuEvent> {
        #[cfg(feature = "native-menu")]
        {
            let _ = &self.menu_tx;
            self.menu_rx
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .try_recv()
                .ok()
        }
        #[cfg(not(feature = "native-menu"))]
        {
            let _ = (&self.menu_tx, &self.menu_rx);
            None
        }
    }

    /// Where this window is and how big it is, as the platform reports it.
    ///
    /// The answer comes back from the engine thread, which owns the window. The wait is bounded:
    /// a host whose engine has stopped answering reports no geometry rather than blocking the
    /// caller, which is usually an application in the middle of closing.
    #[must_use]
    pub fn window_geometry(&self) -> Option<ncui_webview::WindowGeometry> {
        #[cfg(feature = "wry-engine")]
        {
            let (engine, window) = (self.engine.as_ref()?, self.window?);
            let (reply, answers) = mpsc::channel();
            engine
                .send_event(HostEvent::Geometry { window, reply })
                .ok()?;
            let reported = answers.recv_timeout(GEOMETRY_TIMEOUT).ok().flatten()?;
            Some(ncui_webview::WindowGeometry {
                x: reported.x,
                y: reported.y,
                width: reported.width,
                height: reported.height,
            })
        }
        #[cfg(not(feature = "wry-engine"))]
        {
            None
        }
    }

    /// How this window is being presented, as the platform reports it.
    ///
    /// The answer comes back from the engine thread, which owns the window, with the same bounded
    /// wait the geometry uses: an engine that has stopped answering reports nothing rather than
    /// blocking a caller that is usually closing the application.
    #[must_use]
    pub fn window_state(&self) -> Option<ncui_webview::WindowState> {
        self.ask_window_state(None)
    }

    /// Asks the platform for one change to this window's presentation, reporting the result.
    pub fn set_window_state(
        &self,
        change: ncui_webview::WindowChange,
    ) -> Option<ncui_webview::WindowState> {
        self.ask_window_state(Some(change))
    }

    /// Asks the platform to make this window take `shape`, reporting whether it did.
    ///
    /// `false` when the engine could not be reached or has no such window, which is the answer a
    /// caller needs: a window that is gone cannot be reshaped, and a caller told otherwise would
    /// believe a title was set on a window that no longer exists.
    pub fn window_shape(&self, shape: &ncui_webview::WindowShape) -> bool {
        #[cfg(feature = "wry-engine")]
        {
            let (Some(engine), Some(window)) = (&self.engine, self.window) else {
                return false;
            };
            let (reply, answers) = mpsc::channel();
            if engine
                .send_event(HostEvent::WindowShape {
                    window,
                    shape: Box::new(shape.clone()),
                    reply,
                })
                .is_err()
            {
                return false;
            }
            answers
                .recv_timeout(ENGINE_REQUEST_TIMEOUT)
                .unwrap_or(false)
        }
        #[cfg(not(feature = "wry-engine"))]
        {
            let _ = (self, shape);
            false
        }
    }

    /// Sends one presentation request to the engine and waits, bounded, for its answer.
    fn ask_window_state(
        &self,
        change: Option<ncui_webview::WindowChange>,
    ) -> Option<ncui_webview::WindowState> {
        #[cfg(feature = "wry-engine")]
        {
            let (engine, window) = (self.engine.as_ref()?, self.window?);
            let (reply, answers) = mpsc::channel();
            engine
                .send_event(HostEvent::WindowState {
                    window,
                    change,
                    reply,
                })
                .ok()?;
            answers.recv_timeout(ENGINE_REQUEST_TIMEOUT).ok().flatten()
        }
        #[cfg(not(feature = "wry-engine"))]
        {
            // Neither argument means anything without the engine: there is no window to ask and no
            // change to make.
            let _ = (self, change);
            None
        }
    }

    /// Sends one CDP command through this window's engine identity (ADR-017 Phase 2).
    ///
    /// The engine owns the immutable page-target binding established during serialized window
    /// creation. A caller never supplies an engine id, and the process-wide browser endpoint is
    /// not treated as a page identity. The reply wait is bounded because a stopped engine must
    /// not hang an application request.
    pub fn cdp_send(
        &self,
        command: &serde_json::Value,
    ) -> Result<serde_json::Value, crate::cdp::CdpError> {
        crate::cdp::command_id(command)?;
        #[cfg(feature = "wry-engine")]
        {
            let (Some(engine), Some(window)) = (&self.engine, self.window) else {
                return Err(crate::cdp::CdpError::TargetBindFailed {
                    reason: "this host has no live engine window".to_owned(),
                });
            };
            let (reply, answers) = mpsc::channel();
            engine
                .send_event(HostEvent::CdpRequest {
                    window,
                    command: command.clone(),
                    reply,
                })
                .map_err(|error| crate::cdp::CdpError::TransportFailed {
                    reason: format!("cannot post the CDP request to the engine: {error}"),
                })?;
            answers.recv_timeout(CDP_REPLY_TIMEOUT).map_err(|error| {
                crate::cdp::CdpError::TransportFailed {
                    reason: format!("the engine did not answer the CDP request in time: {error}"),
                }
            })?
        }
        #[cfg(not(feature = "wry-engine"))]
        {
            let _ = self;
            Err(crate::cdp::CdpError::UnsupportedPlatform {
                reason: "this build has no WebView engine, so there is no DevTools endpoint"
                    .to_owned(),
            })
        }
    }

    /// Scales this window's page by `factor`, where `1.0` is the platform default.
    ///
    /// `false` when the engine could not be reached or has no such window. The result is
    /// not read back: the platform applies the scale and the next paint shows it.
    pub fn set_zoom(&self, factor: f64) -> bool {
        #[cfg(feature = "wry-engine")]
        {
            let (Some(engine), Some(window)) = (&self.engine, self.window) else {
                return false;
            };
            let (reply, answers) = mpsc::channel();
            if engine
                .send_event(HostEvent::Zoom {
                    window,
                    factor,
                    reply,
                })
                .is_err()
            {
                return false;
            }
            answers
                .recv_timeout(ENGINE_REQUEST_TIMEOUT)
                .unwrap_or(false)
        }
        #[cfg(not(feature = "wry-engine"))]
        {
            let _ = (self, factor);
            false
        }
    }

    #[cfg(all(test, target_os = "linux", feature = "wry-engine"))]
    fn applied_zoom_level(&self) -> Option<f64> {
        let (Some(engine), Some(window)) = (&self.engine, self.window) else {
            return None;
        };
        let (reply, answers) = mpsc::channel();
        engine
            .send_event(HostEvent::ZoomLevel { window, reply })
            .ok()?;
        answers.recv_timeout(ENGINE_REQUEST_TIMEOUT).ok().flatten()
    }

    /// Opens the platform's web inspector for this window.
    ///
    /// `false` when the engine cannot open one, or cannot answer whether it did — which on
    /// some platforms is the honest answer (wry's Windows backend opens the inspector but
    /// cannot report its state), and is reported as such rather than claimed as success.
    pub fn open_devtools(&self) -> bool {
        self.devtools_request(DevToolsAction::Open)
    }

    /// Closes the platform's web inspector for this window. See [`WindowHost::open_devtools`].
    pub fn close_devtools(&self) -> bool {
        self.devtools_request(DevToolsAction::Close)
    }

    /// Whether the platform's web inspector is open for this window.
    ///
    /// `None` when the engine cannot say — a platform that opens the inspector without a
    /// state query is reported as unknown, not as closed.
    #[must_use]
    pub fn is_devtools_open(&self) -> Option<bool> {
        self.devtools_query()
    }

    fn devtools_request(&self, action: DevToolsAction) -> bool {
        #[cfg(feature = "wry-engine")]
        {
            let (Some(engine), Some(window)) = (&self.engine, self.window) else {
                return false;
            };
            let (reply, answers) = mpsc::channel();
            if engine
                .send_event(HostEvent::DevTools {
                    window,
                    action,
                    reply,
                })
                .is_err()
            {
                return false;
            }
            answers
                .recv_timeout(ENGINE_REQUEST_TIMEOUT)
                .unwrap_or(false)
        }
        #[cfg(not(feature = "wry-engine"))]
        {
            let _ = (self, action);
            false
        }
    }

    fn devtools_query(&self) -> Option<bool> {
        #[cfg(feature = "wry-engine")]
        {
            let (engine, window) = (self.engine.as_ref()?, self.window?);
            let (reply, answers) = mpsc::channel();
            engine
                .send_event(HostEvent::DevTools {
                    window,
                    action: DevToolsAction::Query,
                    reply,
                })
                .ok()?;
            answers.recv_timeout(ENGINE_REQUEST_TIMEOUT).ok()
        }
        #[cfg(not(feature = "wry-engine"))]
        {
            let _ = self;
            None
        }
    }

    /// Clone of the producer side of the IPC channel.
    ///
    /// Mostly useful for wiring: the `wry` handler already holds its own clone, but a
    /// caller that needs to inject synthetic events can post into the same queue the host
    /// drains.
    pub fn ipc_sender(&self) -> Sender<HostIpcEvent> {
        self.ipc_tx.clone()
    }

    /// A reference to the consumer side of the IPC channel.
    ///
    /// Callers that prefer polling through the standard [`Receiver`] API (e.g.
    /// [`Receiver::try_recv`] or [`Receiver::recv_timeout`]) can use this instead of
    /// [`WindowHost::try_recv_ipc`].
    pub fn ipc_receiver(&self) -> &Mutex<Receiver<HostIpcEvent>> {
        &self.ipc_rx
    }

    /// Non-blocking receive of the next IPC event forwarded from the page.
    ///
    /// Returns [`TryRecvError::Empty`] when nothing has arrived yet and
    /// [`TryRecvError::Disconnected`] if the producer side was dropped.
    pub fn try_recv_ipc(&self) -> Result<HostIpcEvent, TryRecvError> {
        self.ipc_rx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .try_recv()
    }
}

#[cfg(feature = "wry-engine")]
impl WindowHost {
    fn create_child_webview_with_timeout(
        &self,
        id: u64,
        request: &ChildWebviewRequest,
        seam: &Arc<HostSeam>,
        ipc_tx: SyncSender<ChildPageMessage>,
        timeout: Duration,
    ) -> Result<u64, String> {
        let (Some(engine), Some(window)) = (&self.engine, self.window) else {
            return Err("this window's engine is not running".to_owned());
        };
        let (host_ipc_tx, child_ipc_rx) = mpsc::sync_channel(ncui_webview::MAX_CHILD_IPC_MESSAGES);
        let (reply, answers) = mpsc::channel();
        engine
            .send_event(HostEvent::CreateChild {
                parent: window,
                id,
                request: Box::new(request.clone()),
                seam: Arc::clone(seam),
                ipc_tx,
                host_ipc_tx,
                reply,
            })
            .map_err(|error| error.to_string())?;
        match answers.recv_timeout(timeout) {
            Ok(Ok(child)) => {
                self.child_ipc_rx
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(child, child_ipc_rx);
                Ok(child)
            }
            Ok(Err(error)) => Err(error.to_string()),
            Err(error) => {
                // The engine processes events in order. This close runs after the timed-out
                // build and removes a child that completed after its caller stopped waiting.
                let (reply, _answers) = mpsc::channel();
                let _ = engine.send_event(HostEvent::CloseChild {
                    parent: window,
                    child: id,
                    reply,
                });
                Err(error.to_string())
            }
        }
    }
}

impl WebViewBackend for WindowHost {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn cdp_send(
        &self,
        command: &serde_json::Value,
    ) -> Result<serde_json::Value, crate::cdp::CdpError> {
        WindowHost::cdp_send(self, command)
    }

    #[cfg(feature = "wry-engine")]
    fn create_child_webview(
        &self,
        request: &ChildWebviewRequest,
        seam: &Arc<HostSeam>,
        ipc_tx: SyncSender<ChildPageMessage>,
    ) -> Result<u64, String> {
        let id = NEXT_CHILD.fetch_add(1, Ordering::SeqCst);
        self.create_child_webview_with_timeout(id, request, seam, ipc_tx, CHILD_REQUEST_TIMEOUT)
    }

    #[cfg(not(feature = "wry-engine"))]
    fn create_child_webview(
        &self,
        _request: &ChildWebviewRequest,
        _seam: &Arc<HostSeam>,
        _ipc_tx: SyncSender<ChildPageMessage>,
    ) -> Result<u64, String> {
        Err("this build has no window engine".to_owned())
    }

    #[cfg(feature = "wry-engine")]
    fn set_child_webview_bounds(
        &self,
        child: u64,
        request: &ChildWebviewRequest,
    ) -> Result<(), String> {
        let (Some(engine), Some(window)) = (&self.engine, self.window) else {
            return Err("this window's engine is not running".to_owned());
        };
        let (reply, answers) = mpsc::channel();
        engine
            .send_event(HostEvent::ChildBounds {
                parent: window,
                child,
                request: Box::new(request.clone()),
                reply,
            })
            .map_err(|error| error.to_string())?;
        answers
            .recv_timeout(CHILD_REQUEST_TIMEOUT)
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())
    }

    #[cfg(not(feature = "wry-engine"))]
    fn set_child_webview_bounds(
        &self,
        _child: u64,
        _request: &ChildWebviewRequest,
    ) -> Result<(), String> {
        Err("this build has no window engine".to_owned())
    }

    #[cfg(feature = "wry-engine")]
    fn close_child_webview(&self, child: u64) -> Result<(), String> {
        let (Some(engine), Some(window)) = (&self.engine, self.window) else {
            return Err("this window's engine is not running".to_owned());
        };
        let (reply, answers) = mpsc::channel();
        engine
            .send_event(HostEvent::CloseChild {
                parent: window,
                child,
                reply,
            })
            .map_err(|error| error.to_string())?;
        answers
            .recv_timeout(CHILD_REQUEST_TIMEOUT)
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        self.child_ipc_rx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&child);
        Ok(())
    }

    #[cfg(not(feature = "wry-engine"))]
    fn close_child_webview(&self, _child: u64) -> Result<(), String> {
        Err("this build has no window engine".to_owned())
    }

    fn poll_child_webview_message(&self, child: u64) -> Option<ChildPageMessage> {
        self.child_ipc_rx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&child)?
            .try_recv()
            .ok()
    }

    fn wait_child_webview_message(
        &self,
        child: u64,
        timeout: Duration,
    ) -> Option<ChildPageMessage> {
        self.child_ipc_rx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&child)?
            .recv_timeout(timeout)
            .ok()
    }

    #[cfg(feature = "wry-engine")]
    fn evaluate_child_script(&self, child: u64, script: &str) -> bool {
        let (Some(engine), Some(window)) = (&self.engine, self.window) else {
            return false;
        };
        if !self.alive.load(Ordering::SeqCst)
            || !self
                .child_ipc_rx
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(&child)
        {
            return false;
        }
        engine
            .send_event(HostEvent::ChildEval {
                parent: window,
                child,
                script: script.to_owned(),
            })
            .is_ok()
    }

    #[cfg(not(feature = "wry-engine"))]
    fn evaluate_child_script(&self, _child: u64, _script: &str) -> bool {
        false
    }

    fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|g| g.clone())
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    fn poll_page_message(&self) -> Option<PageMessage> {
        self.try_recv_ipc().ok().map(PageMessage::new)
    }

    fn wait_page_message(&self, timeout: Duration) -> Option<PageMessage> {
        self.ipc_rx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .recv_timeout(timeout)
            .ok()
            .map(PageMessage::new)
    }

    fn geometry(&self) -> Option<ncui_webview::WindowGeometry> {
        self.window_geometry()
    }

    fn window_state(&self) -> Option<ncui_webview::WindowState> {
        self.window_state()
    }

    fn set_window_state(
        &self,
        change: ncui_webview::WindowChange,
    ) -> Option<ncui_webview::WindowState> {
        self.set_window_state(change)
    }

    fn set_window_shape(&self, shape: &ncui_webview::WindowShape) -> bool {
        self.window_shape(shape)
    }

    #[cfg(feature = "tray-icon")]
    fn set_tray(&self, tray: Option<ncui_webview::TrayRequest>) -> bool {
        self.tray(tray)
    }

    #[cfg(feature = "native-menu")]
    fn set_menu(&self, menu: Option<ncui_webview::MenuRequest>) -> bool {
        self.menu(menu)
    }

    fn poll_menu_event(&self) -> Option<ncui_webview::MenuEvent> {
        self.poll_menu_event()
    }

    fn window_theme(&self) -> Option<ncui_webview::Theme> {
        self.window_theme()
    }

    fn set_zoom(&self, factor: f64) -> bool {
        self.set_zoom(factor)
    }

    fn open_devtools(&self) -> bool {
        self.open_devtools()
    }

    fn close_devtools(&self) -> bool {
        self.close_devtools()
    }

    fn is_devtools_open(&self) -> Option<bool> {
        self.is_devtools_open()
    }

    fn evaluate_script(&self, script: &str) -> bool {
        #[cfg(feature = "wry-engine")]
        {
            let (Some(engine), Some(window)) = (&self.engine, self.window) else {
                return false;
            };
            engine
                .send_event(HostEvent::Eval {
                    window,
                    script: script.to_owned(),
                })
                .is_ok()
        }
        #[cfg(not(feature = "wry-engine"))]
        {
            let _ = script;
            false
        }
    }
}

/// Creates one `wry`-backed window per call.
///
/// This is the adapter between NC-UI's port and the platform's embedding layer: the
/// registry asks for a host, and this is what hands back one.
#[derive(Debug, Default)]
pub struct WryHostFactory;

impl ncui_webview::HostFactory for WryHostFactory {
    fn monitors(&self) -> Vec<ncui_webview::MonitorRect> {
        #[cfg(feature = "wry-engine")]
        {
            let Some(engine) = engine() else {
                return Vec::new();
            };
            // One engine serves every window in this process, so a reply can be behind another
            // caller's work. The attempts are bounded, and a factory that never hears back
            // reports no displays rather than holding up the launch that asked — a launch with no
            // display list leaves a remembered window place alone, which is the safe answer.
            for attempt in 0..MONITORS_ATTEMPTS {
                let (reply, answers) = mpsc::channel();
                if engine.send_event(HostEvent::Monitors { reply }).is_err() {
                    return Vec::new();
                }
                if let Ok(monitors) = answers.recv_timeout(MONITORS_TIMEOUT)
                    && !monitors.is_empty()
                {
                    return monitors;
                }
                if attempt + 1 < MONITORS_ATTEMPTS {
                    thread::sleep(MONITORS_RETRY_DELAY);
                }
            }
            Vec::new()
        }
        #[cfg(not(feature = "wry-engine"))]
        {
            Vec::new()
        }
    }

    fn spawn(
        &self,
        _window: WindowId,
        request: &ncui_webview::WindowRequest,
        seam: &Arc<HostSeam>,
    ) -> Option<Box<dyn WebViewBackend>> {
        #[cfg(feature = "wry-engine")]
        {
            let host = spawn_real(Some(request.clone()), Arc::clone(seam));
            if !host.is_alive() {
                return None;
            }
            Some(Box::new(host))
        }
        #[cfg(not(feature = "wry-engine"))]
        {
            let _ = (request, seam);
            None
        }
    }

    fn last_error(&self) -> Option<String> {
        #[cfg(feature = "wry-engine")]
        {
            WRY_LAST_ERROR
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
        #[cfg(not(feature = "wry-engine"))]
        {
            Some("wry-engine feature is disabled; no window can be created".to_owned())
        }
    }
}

/// The last failure any `wry` host reported.
///
/// The factory returns `None` when a host cannot be created, which is the shape
/// [`ncui_webview::HostFactory`] uses. Without somewhere to put the engine's own
/// diagnostics, the refusal would reach an agent as "the engine could not be created"
/// with no reason — the failure would be visible but not diagnosable.
#[cfg(feature = "wry-engine")]
static WRY_LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

#[cfg(feature = "wry-engine")]
fn record_error(message: String) {
    if let Ok(mut guard) = WRY_LAST_ERROR.lock() {
        *guard = Some(message);
    }
}

#[cfg(feature = "gpu-surface")]
#[allow(unused_qualifications, reason = "Future is not imported at this scope")]
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::task::{Context, Poll};
    let waker = std::task::Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut future = Box::pin(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => thread::yield_now(),
        }
    }
}

/// Creates one window through the process-wide engine.
///
/// Returns a host even when the window could not be created: the failure is carried in
/// `last_error`, which is what the port reports. A host that panicked instead would take
/// the process down for a failure the registry is designed to report.
#[cfg(feature = "wry-engine")]
fn spawn_real(request: Option<ncui_webview::WindowRequest>, seam: Arc<HostSeam>) -> WindowHost {
    let alive = Arc::new(AtomicBool::new(false));
    let last_error = Arc::new(Mutex::new(None));
    let (ipc_tx, ipc_rx) = mpsc::channel::<HostIpcEvent>();
    let (menu_tx, menu_rx) = mpsc::channel::<ncui_webview::MenuEvent>();

    let Some(engine) = engine() else {
        if let Ok(mut guard) = last_error.lock() {
            *guard = Some("the window engine could not be started".to_owned());
        }
        return WindowHost {
            alive,
            last_error,
            ipc_tx,
            ipc_rx: Mutex::new(ipc_rx),
            child_ipc_rx: Mutex::new(HashMap::new()),
            menu_tx,
            menu_rx: Mutex::new(menu_rx),
            seam,
            engine: None,
            window: None,
        };
    };

    let (reply_tx, reply_rx) = mpsc::channel();
    let create = CreateRequest {
        request,
        seam: Arc::clone(&seam),
        ipc_tx: ipc_tx.clone(),
        menu_tx: menu_tx.clone(),
        alive: Arc::clone(&alive),
        error: Arc::clone(&last_error),
        reply: reply_tx,
    };
    if engine
        .send_event(HostEvent::Create(Box::new(create)))
        .is_err()
    {
        if let Ok(mut guard) = last_error.lock() {
            *guard = Some("the window engine is not accepting work".to_owned());
        }
        return WindowHost {
            alive,
            last_error,
            ipc_tx,
            ipc_rx: Mutex::new(ipc_rx),
            child_ipc_rx: Mutex::new(HashMap::new()),
            menu_tx,
            menu_rx: Mutex::new(menu_rx),
            seam,
            engine: None,
            window: None,
        };
    }

    let window = match reply_rx.recv_timeout(WINDOW_CREATE_TIMEOUT) {
        Ok(Ok(id)) => Some(id),
        Ok(Err(message)) => {
            if let Ok(mut guard) = last_error.lock() {
                *guard = Some(message);
            }
            None
        }
        Err(_) => {
            if let Ok(mut guard) = last_error.lock() {
                *guard = Some("timed out waiting for the window engine".to_owned());
            }
            None
        }
    };

    WindowHost {
        alive,
        last_error,
        ipc_tx,
        ipc_rx: Mutex::new(ipc_rx),
        child_ipc_rx: Mutex::new(HashMap::new()),
        menu_tx,
        menu_rx: Mutex::new(menu_rx),
        seam,
        engine: Some(engine.clone()),
        window,
    }
}

#[cfg(feature = "wry-engine")]
impl Drop for WindowHost {
    fn drop(&mut self) {
        if let (Some(engine), Some(window)) = (&self.engine, self.window) {
            let _ = engine.send_event(HostEvent::Close { window });
        }
        // The engine's loop is process-wide and outlives this window; nothing is joined.
    }
}

#[cfg(test)]
mod bridge_contract {
    /// The bridge the client package ships, which the host injects.
    ///
    /// Read from the same path the feature-gated build injects, so this test fails if the
    /// file moves or the contract is renamed on one side only.
    const BRIDGE: &str = include_str!("../../../packages/ncui-client/src/bridge.js");

    #[test]
    fn the_injected_bridge_exposes_every_entry_point_the_runtime_uses() {
        for entry in [
            "__dispatch",
            "invoke",
            "describe",
            "probeNavigation",
            "on",
            "once",
        ] {
            assert!(
                BRIDGE.contains(entry),
                "the bridge must expose `{entry}` to the runtime and the page"
            );
        }
    }

    #[test]
    fn the_injected_bridge_correlates_requests_and_bounds_them() {
        for marker in ["nextId", "pending", "backpressure", "timeout"] {
            assert!(
                BRIDGE.contains(marker),
                "the bridge must carry `{marker}`: a page must be able to correlate its \
                 calls and must not queue without bound"
            );
        }
    }
}

// macOS requires the `EventLoop` on the main thread — tao panics with "On macOS,
// `EventLoop` must be created on the main thread!" and its `PlatformSpecificEventLoopAttributes`
// is an empty struct, so no any-thread switch exists. This host creates the loop on a worker
// thread by design, because the runtime's own pump thread must not be the one blocking in
// `run`. Until the host runs its loop on the main thread, these tests cannot pass on macOS,
// and `docs/known-gaps.md` carries that as the macOS blocker rather than leaving a panic in
// the suite. The tests that open a window also skip on a machine with no display server
// (`require_display`, below): a headless Linux runner cannot create a GTK window, and the
// display is a fact about the machine rather than a defect in the host.
#[cfg(all(test, feature = "wry-engine", not(target_os = "macos")))]
mod tests {
    use super::*;
    use ncui_webview::{CapabilityGrant, NavigationPolicy, Origin, Scheme};

    /// Whether this machine can open a real window; `false` skips a test that opens one.
    ///
    /// Windows ships a window manager on every consumer build, so it is always yes. On
    /// Linux the display server answers: no `DISPLAY` and no `WAYLAND_DISPLAY` means GTK
    /// cannot initialise, which is what the runner's log said.
    pub(super) fn require_display() -> bool {
        if cfg!(target_os = "linux")
            && std::env::var("DISPLAY").is_err()
            && std::env::var("WAYLAND_DISPLAY").is_err()
        {
            return false;
        }
        true
    }

    fn seam() -> Arc<HostSeam> {
        let policy = NavigationPolicy::new(&[Origin::new(Scheme::Https, "app.example.com", None)]);
        Arc::new(HostSeam::new(
            ncui_webview::WindowLifecycle::new(),
            CapabilityGrant::none(),
            policy,
        ))
    }

    #[test]
    fn a_host_comes_up_and_reports_itself_alive() {
        if !require_display() {
            return;
        }
        let host = WindowHost::new();
        assert!(
            host.is_alive(),
            "host did not come up: {:?}",
            host.last_error()
        );
        drop(host);
    }

    #[test]
    fn a_resize_and_a_move_are_sent_to_the_page_as_events() {
        // The page is told how big the window became and where it went, in logical pixels, through the
        // same `__dispatch` the other pushed events use. The shape is pinned here because it is the
        // wire contract a page reads: a page that listened for a different name would simply never
        // hear anything.
        let resized = resized_script(tao::dpi::LogicalSize::new(800.0, 600.0));
        assert!(resized.contains("\"event\":\"resized\""), "got {resized}");
        assert!(resized.contains("\"width\":800"), "got {resized}");
        assert!(resized.contains("\"height\":600"), "got {resized}");
        let moved = moved_script(tao::dpi::LogicalPosition::new(120.0, 40.0));
        assert!(moved.contains("\"event\":\"moved\""), "got {moved}");
        assert!(moved.contains("\"x\":120"), "got {moved}");
        assert!(moved.contains("\"y\":40"), "got {moved}");
    }

    #[test]
    fn a_navigation_the_policy_forbids_is_refused_by_the_engine() {
        // The acceptance test the whole binding exists for: the policy is not a model, it
        // is the thing that stops the engine.
        let seam = seam();
        let forbidden = ncui_webview::NavigationRequest::from_url("https://evil.test/steal", true)
            .expect("a readable url");
        assert!(
            !seam.evaluate_navigation(&forbidden).is_allowed(),
            "a forbidden origin must be refused"
        );
        let permitted =
            ncui_webview::NavigationRequest::from_url("https://app.example.com/next", true)
                .expect("a readable url");
        assert!(
            seam.evaluate_navigation(&permitted).is_allowed(),
            "an allowed origin must be permitted"
        );
    }

    #[test]
    fn a_message_the_page_posts_reaches_the_host() {
        if !require_display() {
            return;
        }
        let host = WindowHost::new();
        assert!(
            host.is_alive(),
            "host did not come up: {:?}",
            host.last_error()
        );

        // Give the engine a moment to finish initialising the page/ipc hook.
        thread::sleep(Duration::from_millis(250));

        host.eval("window.ncui.invoke('describe', { deep: true })");

        let mut received: Option<HostIpcEvent> = None;
        for _ in 0..100 {
            match host.try_recv_ipc() {
                Ok(msg) => {
                    received = Some(msg);
                    break;
                }
                Err(TryRecvError::Empty) => {
                    thread::sleep(Duration::from_millis(50));
                }
                Err(other) => {
                    // Record the channel failure without aborting; the assertion below fails.
                    let _ = other;
                    received = None;
                    break;
                }
            }
        }

        let body = received.expect("host did not receive the posted IPC message");
        let message: serde_json::Value =
            serde_json::from_str(&body).expect("the bridge must post valid JSON");
        assert_eq!(message["action"], "perform");
        assert_eq!(message["name"], "describe");
        assert_eq!(message["payload"], serde_json::json!({ "deep": true }));
        assert!(
            message["id"].as_str().is_some_and(|id| !id.is_empty()),
            "every bridge request carries a correlation id: {message}"
        );
        drop(host);
    }

    #[test]
    fn a_window_with_an_icon_comes_up() {
        if !require_display() {
            return;
        }
        // The icon is handed to the platform's own window-creation call — on Windows that sends
        // WM_SETICON to the new window — so a window that comes up has been given one.
        let request = ncui_webview::WindowRequest::new("https://app.example.com").with_icon(
            ncui_webview::Icon {
                width: 2,
                height: 2,
                rgba: vec![
                    255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
                ],
            },
        );
        let host = spawn_real(Some(request), seam());
        assert!(
            host.is_alive(),
            "a window with an icon did not come up: {:?}",
            host.last_error()
        );
        drop(host);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_live_webkit_inspector_opens_reports_and_closes() {
        if !require_display() {
            return;
        }
        let host = WindowHost::new();
        assert!(
            host.is_alive(),
            "the window did not come up: {:?}",
            host.last_error()
        );
        assert!(host.open_devtools(), "the inspector open was refused");
        let deadline = Instant::now() + Duration::from_secs(15);
        while host.is_devtools_open() != Some(true) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(host.is_devtools_open(), Some(true), "the inspector is open");
        assert!(host.close_devtools(), "the inspector close was refused");
        let deadline = Instant::now() + Duration::from_secs(15);
        while host.is_devtools_open() != Some(false) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(
            host.is_devtools_open(),
            Some(false),
            "the inspector is closed"
        );
    }

    fn running_child_seam() -> Arc<HostSeam> {
        let child_lifecycle = ncui_webview::WindowLifecycle::new();
        for state in [
            ncui_webview::HostState::Created,
            ncui_webview::HostState::Attaching,
            ncui_webview::HostState::Running,
        ] {
            child_lifecycle
                .transition(state)
                .expect("the child lifecycle reaches running");
        }
        assert!(child_lifecycle.is_serving(), "the child seam is running");
        let child_policy =
            NavigationPolicy::new(&[Origin::new(Scheme::Https, "example.com", None)]);
        let child_parent = HostSeam::new(
            ncui_webview::WindowLifecycle::new(),
            CapabilityGrant::none(),
            child_policy,
        );
        Arc::new(child_parent.isolated_child(child_lifecycle))
    }

    fn live_child() -> (
        WindowHost,
        ChildWebviewRequest,
        u64,
        Receiver<ChildPageMessage>,
    ) {
        let host = spawn_real(
            Some(ncui_webview::WindowRequest::new("about:blank")),
            seam(),
        );
        assert!(
            host.is_alive(),
            "the parent window did not come up: {:?}",
            host.last_error()
        );
        let child_seam = running_child_seam();
        let (ipc_tx, ipc_rx) =
            mpsc::sync_channel::<ChildPageMessage>(ncui_webview::MAX_CHILD_IPC_MESSAGES);
        let request = ChildWebviewRequest {
            url: "https://example.com/".to_owned(),
            x: 10,
            y: 10,
            width: 320,
            height: 240,
        };
        let initial_navigation =
            ncui_webview::NavigationRequest::from_url(&request.url, true).expect("a readable URL");
        assert!(
            child_seam
                .evaluate_navigation(&initial_navigation)
                .is_allowed(),
            "the test URL is allowed by the child's isolated policy"
        );
        let child = host
            .create_child_webview(&request, &child_seam, ipc_tx)
            .expect("the child webview was embedded");
        (host, request, child, ipc_rx)
    }

    fn assert_other_parent_cannot_reach_child(child: u64, request: &ChildWebviewRequest) {
        let other = WindowHost::new();
        assert!(
            other.is_alive(),
            "the second parent did not come up: {:?}",
            other.last_error()
        );
        assert!(
            other.set_child_webview_bounds(child, request).is_err(),
            "a child cannot be moved through a different parent"
        );
        assert!(
            other.close_child_webview(child).is_err(),
            "a child cannot be closed through a different parent"
        );
        assert!(
            !other.evaluate_child_script(child, "window.ipc.postMessage('wrong parent')"),
            "a child cannot be evaluated through a different parent"
        );
    }

    #[test]
    fn a_timed_out_child_build_is_closed_before_later_requests() {
        if !require_display() {
            return;
        }
        let host = spawn_real(
            Some(ncui_webview::WindowRequest::new("about:blank")),
            seam(),
        );
        assert!(host.is_alive(), "the parent window did not come up");
        let request = ChildWebviewRequest {
            url: "https://example.com/".to_owned(),
            x: 10,
            y: 10,
            width: 320,
            height: 240,
        };
        let child_seam = running_child_seam();
        let (ipc_tx, _ipc_rx) =
            mpsc::sync_channel::<ChildPageMessage>(ncui_webview::MAX_CHILD_IPC_MESSAGES);
        let id = u64::MAX;
        let timeout = host
            .create_child_webview_with_timeout(id, &request, &child_seam, ipc_tx, Duration::ZERO)
            .expect_err("the zero budget times out while the engine builds the child");
        assert!(timeout.contains("timed out"), "{timeout}");

        // This request is queued after the timeout cleanup. Seeing `not live` proves the engine
        // processed the late build and its close instead of retaining an invisible orphan.
        let closed = host
            .set_child_webview_bounds(id, &request)
            .expect_err("the timed-out child is no longer live");
        assert!(closed.contains("not live"), "{closed}");
    }

    #[test]
    fn a_child_webview_embeds_into_a_live_parent_and_stays_live() {
        if !require_display() {
            return;
        }
        let (host, request, child_id, ipc_rx) = live_child();
        assert_other_parent_cannot_reach_child(child_id, &request);

        // The child is a live surface: moving it must succeed.
        host.set_child_webview_bounds(
            child_id,
            &ChildWebviewRequest {
                x: 20,
                y: 20,
                ..request
            },
        )
        .expect("the child webview was moved");

        // The injected bridge and the child eval route carry real IPC, tagged with the canonical
        // engine id. The caller's channel is primary; eval is retried until the new document is
        // ready rather than assuming a cold navigation finishes within one fixed delay.
        let deadline = Instant::now() + CHILD_REQUEST_TIMEOUT;
        let message = loop {
            assert!(
                host.evaluate_child_script(
                    child_id,
                    "window.ncui.invoke('describe', { from: 'child' });",
                ),
                "the child eval was accepted"
            );
            if let Ok(message) = ipc_rx.recv_timeout(Duration::from_millis(250)) {
                break message;
            }
            assert!(Instant::now() < deadline, "the child page posted IPC");
            thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(message.child, child_id);
        let body: serde_json::Value =
            serde_json::from_str(&message.body).expect("child IPC must be valid JSON");
        assert_eq!(body["action"], "perform");
        assert_eq!(body["name"], "describe");
        assert_eq!(body["payload"], serde_json::json!({ "from": "child" }));

        // Once the caller's receiver disconnects, the parent's retained receiver becomes the
        // fallback instead of allowing an unbounded duplicate queue to accumulate.
        drop(ipc_rx);
        let deadline = Instant::now() + CHILD_REQUEST_TIMEOUT;
        let fallback = loop {
            assert!(
                host.evaluate_child_script(
                    child_id,
                    "window.ncui.invoke('describe', { from: 'host-poll' });",
                ),
                "the fallback child eval was accepted"
            );
            if let Some(message) =
                host.wait_child_webview_message(child_id, Duration::from_millis(250))
            {
                let body: serde_json::Value =
                    serde_json::from_str(&message.body).expect("child IPC must be valid JSON");
                if body["payload"] == serde_json::json!({ "from": "host-poll" }) {
                    break message;
                }
            }
            assert!(
                Instant::now() < deadline,
                "the child page posted fallback IPC"
            );
            thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(fallback.child, child_id);
        let body: serde_json::Value =
            serde_json::from_str(&fallback.body).expect("fallback IPC must be valid JSON");
        assert_eq!(body["action"], "perform");
        assert_eq!(body["name"], "describe");
        assert_eq!(body["payload"], serde_json::json!({ "from": "host-poll" }));

        host.close_child_webview(child_id)
            .expect("the child webview was closed");
        assert!(host.is_alive(), "the parent must stay alive");
        assert!(
            host.close_child_webview(child_id).is_err(),
            "a closed child is no longer live"
        );
        assert!(
            host.poll_child_webview_message(child_id).is_none(),
            "closing removes the child's receiver"
        );
        assert!(
            !host.evaluate_child_script(child_id, "window.ipc.postMessage('closed')"),
            "a closed child cannot be evaluated"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_live_engine_zoom_changes_the_rendered_page() {
        if !require_display() {
            return;
        }
        let host = spawn_real(
            Some(ncui_webview::WindowRequest::new("about:blank")),
            seam(),
        );
        assert!(host.is_alive(), "the window did not come up");
        host.eval("document.body.innerHTML='<div style=\"width:200px;height:200px;background:rgb(255,0,0)\"></div>'; window.ipc.postMessage('ready');");
        let ready_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if host.try_recv_ipc().is_ok_and(|body| body == "ready") {
                break;
            }
            assert!(
                Instant::now() < ready_deadline,
                "the static zoom probe page became ready"
            );
            thread::sleep(Duration::from_millis(25));
        }
        let capture = || -> String {
            host.cdp_send(&serde_json::json!({
                "id": 1,
                "method": "Page.captureScreenshot",
            }))
            .expect("the live page captures")["result"]["data"]
                .as_str()
                .expect("the screenshot is base64 data")
                .to_owned()
        };
        let before = capture();
        assert!(host.set_zoom(2.0), "the engine accepted the zoom request");
        let deadline = Instant::now() + Duration::from_secs(5);
        let after = loop {
            let screenshot = capture();
            if screenshot != before {
                break screenshot;
            }
            assert!(Instant::now() < deadline, "zoom changes the rendered page");
            thread::sleep(Duration::from_millis(50));
        };
        assert_ne!(after, before, "zoom changes the rendered pixels");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_live_engine_zoom_changes_webkits_applied_level() {
        if !require_display() {
            return;
        }
        let host = spawn_real(
            Some(ncui_webview::WindowRequest::new("about:blank")),
            seam(),
        );
        assert!(host.is_alive(), "the window did not come up");
        let before = host
            .applied_zoom_level()
            .expect("WebKit reports its zoom level");
        assert!(host.set_zoom(2.0), "the engine accepted the zoom request");
        let deadline = Instant::now() + Duration::from_secs(5);
        let after = loop {
            let level = host
                .applied_zoom_level()
                .expect("WebKit keeps reporting its zoom level");
            if level > before {
                break level;
            }
            assert!(
                Instant::now() < deadline,
                "WebKit applies the requested zoom level"
            );
            thread::sleep(Duration::from_millis(50));
        };
        assert!(
            (after - 2.0).abs() < 0.001,
            "WebKit applied 2x zoom: {after}"
        );
    }

    #[cfg(feature = "native-menu")]
    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one live test proves attachment, replacement routing, refusal retention and removal"
    )]
    fn a_native_menu_attaches_routes_replaces_and_removes_on_a_live_window() {
        if !require_display() {
            return;
        }
        let host = WindowHost::new();
        assert!(
            host.is_alive(),
            "the window did not come up: {:?}",
            host.last_error()
        );
        let request = ncui_webview::MenuRequest {
            menus: vec![ncui_webview::MenuSubmenu {
                label: "File".to_owned(),
                enabled: true,
                items: vec![
                    ncui_webview::MenuEntry::Item {
                        label: "Open".to_owned(),
                        command: "open".to_owned(),
                        enabled: true,
                    },
                    ncui_webview::MenuEntry::Separator,
                    ncui_webview::MenuEntry::Submenu(ncui_webview::MenuSubmenu {
                        label: "More".to_owned(),
                        enabled: false,
                        items: vec![ncui_webview::MenuEntry::Item {
                            label: "Close".to_owned(),
                            command: "close".to_owned(),
                            enabled: false,
                        }],
                    }),
                ],
            }],
        };
        assert!(host.set_menu(Some(request.clone())), "the menu attaches");
        let window = host.window.expect("the live window has an identity");
        let selected_id = format!("ncui.menu.item.{window}.root.0.0");
        host.engine
            .as_ref()
            .expect("the live window has an engine")
            .send_event(HostEvent::MenuActivated {
                id: selected_id.clone(),
            })
            .expect("the engine accepts a native selection");
        let deadline = Instant::now() + Duration::from_secs(2);
        let selected = loop {
            if let Some(event) = host.poll_menu_event() {
                break event;
            }
            assert!(
                Instant::now() < deadline,
                "the selection reached its window"
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(selected.command, "open");
        let replacement = ncui_webview::MenuRequest {
            menus: vec![
                ncui_webview::MenuSubmenu {
                    label: "Empty".to_owned(),
                    enabled: true,
                    items: vec![ncui_webview::MenuEntry::Separator],
                },
                ncui_webview::MenuSubmenu {
                    label: "Edit".to_owned(),
                    enabled: true,
                    items: vec![ncui_webview::MenuEntry::Item {
                        label: "Replace".to_owned(),
                        command: "replace".to_owned(),
                        enabled: true,
                    }],
                },
            ],
        };
        assert!(
            host.set_menu(Some(replacement)),
            "the menu is replaced by a different command layout"
        );
        host.engine
            .as_ref()
            .expect("the live window has an engine")
            .send_event(HostEvent::MenuActivated { id: selected_id })
            .expect("the engine accepts the old item identity");
        thread::sleep(Duration::from_millis(100));
        assert!(
            host.poll_menu_event().is_none(),
            "an item removed by replacement must no longer route"
        );
        let replacement_id = format!("ncui.menu.item.{window}.root.1.0");
        host.engine
            .as_ref()
            .expect("the live window has an engine")
            .send_event(HostEvent::MenuActivated {
                id: replacement_id.clone(),
            })
            .expect("the engine accepts the replacement item identity");
        let deadline = Instant::now() + Duration::from_secs(2);
        let replaced = loop {
            if let Some(event) = host.poll_menu_event() {
                break event;
            }
            assert!(
                Instant::now() < deadline,
                "the replacement selection reached its window"
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(replaced.command, "replace");

        let duplicate = ncui_webview::MenuRequest {
            menus: vec![ncui_webview::MenuSubmenu {
                label: "Invalid".to_owned(),
                enabled: true,
                items: vec![
                    ncui_webview::MenuEntry::Item {
                        label: "One".to_owned(),
                        command: "duplicate".to_owned(),
                        enabled: true,
                    },
                    ncui_webview::MenuEntry::Item {
                        label: "Two".to_owned(),
                        command: "duplicate".to_owned(),
                        enabled: true,
                    },
                ],
            }],
        };
        assert!(!host.set_menu(Some(duplicate)), "duplicates are refused");
        host.engine
            .as_ref()
            .expect("the live window has an engine")
            .send_event(HostEvent::MenuActivated { id: replacement_id })
            .expect("the retained menu accepts a selection after refusal");
        let deadline = Instant::now() + Duration::from_secs(2);
        let retained = loop {
            if let Some(event) = host.poll_menu_event() {
                break event;
            }
            assert!(
                Instant::now() < deadline,
                "the retained selection reached its window"
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(retained.command, "replace");
        assert!(
            host.set_menu(Some(request)),
            "a valid menu can follow a refusal"
        );
        assert!(host.set_menu(None), "the menu is removed");
        assert!(host.is_alive(), "menu changes leave the window alive");
    }

    #[test]
    fn a_second_window_opens_in_the_same_process() {
        if !require_display() {
            return;
        }
        // The defect this test exists for: a window per event loop worked exactly once and
        // took the process down on the second. The engine owns one loop for the process,
        // so the second window is a message rather than a second loop.
        let first = WindowHost::new();
        assert!(
            first.is_alive(),
            "the first window did not come up: {:?}",
            first.last_error()
        );
        let second = WindowHost::new();
        assert!(
            second.is_alive(),
            "the second window did not come up: {:?}",
            second.last_error()
        );
        assert_ne!(
            first.window, second.window,
            "two windows must have two identities"
        );

        // Each window has its own page and its own bridge: a message in one does not
        // appear in the other's channel.
        thread::sleep(Duration::from_millis(250));
        second.eval("window.ncui.invoke('describe', { from: 'second' })");
        let mut received = None;
        for _ in 0..100 {
            match second.try_recv_ipc() {
                Ok(message) => {
                    received = Some(message);
                    break;
                }
                Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(50)),
                Err(_) => break,
            }
        }
        let body = received.expect("the second window must reach its own page");
        assert!(body.contains("second"), "got: {body}");
        assert!(
            first.try_recv_ipc().is_err(),
            "a message from the second window must not arrive in the first"
        );

        drop(first);
        drop(second);
    }
}

#[cfg(all(test, feature = "wry-engine", not(target_os = "macos")))]
mod cdp_live {
    //! Windows exchanges a real CDP command; other engines must refuse by name.

    use super::*;
    #[cfg(target_os = "windows")]
    use std::time::Instant;

    #[cfg(target_os = "windows")]
    #[test]
    fn a_live_engine_answers_a_cdp_command_over_the_transport() {
        if !tests::require_display() {
            return;
        }
        let host = WindowHost::new();
        assert!(
            host.is_alive(),
            "the window did not come up: {:?}",
            host.last_error()
        );

        // The engine writes the port file while starting its browser, inside the
        // `EBWebView` profile directory of the user-data directory the host named. It is
        // polled rather than assumed, because "not yet" is a normal state during a cold
        // start, which on a busy machine is tens of seconds.
        let port_file = webview_data_root()
            .join("EBWebView")
            .join(crate::cdp::DEVTOOLS_PORT_FILE);
        let deadline = Instant::now() + Duration::from_secs(90);
        while !port_file.is_file() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(200));
        }
        assert!(
            port_file.is_file(),
            "the engine never published {} under {}",
            crate::cdp::DEVTOOLS_PORT_FILE,
            port_file.display()
        );

        let answer = host
            .cdp_send(&serde_json::json!({
                "id": 1,
                "method": "Browser.getVersion",
            }))
            .expect("the live engine answered the CDP command");
        let version = answer
            .get("result")
            .and_then(|result| result.get("product"))
            .and_then(serde_json::Value::as_str)
            .expect("Browser.getVersion answers with a product string");
        assert!(
            version.contains("Edg/") || version.contains("Chrome/"),
            "the product string should name the engine: {version}"
        );
        // The product string is asserted below; nothing needs printing.
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn a_live_engine_without_cdp_refuses_by_name() {
        if !tests::require_display() {
            return;
        }
        let host = WindowHost::new();
        assert!(
            host.is_alive(),
            "the window did not come up: {:?}",
            host.last_error()
        );
        let error = host
            .cdp_send(&serde_json::json!({
                "id": 1,
                "method": "Browser.getVersion",
            }))
            .expect_err("this engine has no CDP endpoint");
        assert_eq!(error.code(), "cdp_platform_unsupported");
        assert!(error.to_string().contains("does not expose"));
        assert!(host.is_alive(), "the refusal must not stop the engine");
    }
}
