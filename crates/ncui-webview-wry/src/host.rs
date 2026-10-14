//! Binding of `ncui-webview`'s policy to a real `wry`/`tao` engine.
//!
//! Unfolds in two parts: the `#[cfg(feature = "wry-engine")]` build creates a window and runs
//! it on a background thread; the default build (feature off) is a small, honest fallback so the
//! crate still compiles everywhere, including Linux CI without `webkit2gtk`.

use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

#[cfg(feature = "wry-engine")]
use std::thread::{self, JoinHandle};
#[cfg(feature = "wry-engine")]
use std::time::Duration;

use ncui_webview::WebViewBackend;

#[cfg(feature = "wry-engine")]
use tao::event::{Event, WindowEvent};
#[cfg(feature = "wry-engine")]
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
#[cfg(feature = "wry-engine")]
use tao::window::WindowBuilder;
#[cfg(feature = "wry-engine")]
use wry::WebViewBuilder;

#[cfg(feature = "wry-engine")]
#[derive(Debug)]
enum HostEvent {
    Eval(String),
    Close,
}

/// The payload carried on the in-process IPC channel bridged from the `wry` IPC handler.
///
/// Alias of [`String`] so callers may treat a received event as the request body text
/// posted by the page (the only content the seam cares about today).
pub type HostIpcEvent = String;

#[derive(Debug)]
pub struct WindowHost {
    alive: Arc<AtomicBool>,
    last_error: Arc<Mutex<Option<String>>>,
    /// Producer side retained so callers (and future pipeline wiring) can post into
    /// the same channel if needed; the `wry` handler holds its own clone.
    ipc_tx: Sender<HostIpcEvent>,
    /// Consumer side for IPC events forwarded from the `wry` IPC handler.
    ipc_rx: Receiver<HostIpcEvent>,
    #[cfg(feature = "wry-engine")]
    proxy: Mutex<Option<EventLoopProxy<HostEvent>>>,
    #[cfg(feature = "wry-engine")]
    #[allow(
        dead_code,
        reason = "kept for future join/handle management; today the thread is detached"
    )]
    thread: Option<JoinHandle<()>>,
}

impl Default for WindowHost {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowHost {
    /// Create (and start) a host with a default, empty page.
    ///
    /// The window runs on a background thread; this call returns once the engine has either come
    /// up (success) or reported a failure (error stored and returned via [`WebViewBackend`]).
    pub fn new() -> Self {
        #[cfg(feature = "wry-engine")]
        {
            spawn_real()
        }
        #[cfg(not(feature = "wry-engine"))]
        {
            let (ipc_tx, ipc_rx) = mpsc::channel::<HostIpcEvent>();
            Self {
                alive: Arc::new(AtomicBool::new(false)),
                last_error: Arc::new(Mutex::new(Some(
                    "wry-engine feature is disabled; this host cannot create a window".to_owned(),
                ))),
                ipc_tx,
                ipc_rx,
            }
        }
    }

    /// Send JavaScript to be evaluated in the page. No-op when the feature is disabled or the
    /// host has died.
    pub fn eval(&self, script: &str) {
        #[cfg(feature = "wry-engine")]
        {
            if let Ok(guard) = self.proxy.lock()
                && let Some(proxy) = guard.as_ref()
            {
                let _ = proxy.send_event(HostEvent::Eval(script.to_owned()));
            }
        }
        #[cfg(not(feature = "wry-engine"))]
        {
            let _ = script;
        }
    }

    /// Clone of the producer side of the IPC channel.
    ///
    /// Mostly useful for wiring: the `wry` handler already holds its own clone, but a
    /// caller that needs to inject synthetic events (tests, future pipeline glue) can use
    /// this to post into the same queue the host drains.
    pub fn ipc_sender(&self) -> Sender<HostIpcEvent> {
        self.ipc_tx.clone()
    }

    /// A reference to the consumer side of the IPC channel.
    ///
    /// Callers that prefer polling through the standard [`Receiver`] API (e.g.
    /// [`Receiver::try_recv`] or [`Receiver::recv_timeout`]) can use this instead of
    /// [`WindowHost::try_recv_ipc`].
    pub fn ipc_receiver(&self) -> &Receiver<HostIpcEvent> {
        &self.ipc_rx
    }

    /// Non-blocking receive of the next IPC event forwarded from the page.
    ///
    /// Returns [`TryRecvError::Empty`] when nothing has arrived yet and
    /// [`TryRecvError::Disconnected`] if the producer side was dropped.
    pub fn try_recv_ipc(&self) -> Result<HostIpcEvent, TryRecvError> {
        self.ipc_rx.try_recv()
    }
}

impl WebViewBackend for WindowHost {
    fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|g| g.clone())
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
}

#[cfg(feature = "wry-engine")]
#[allow(
    clippy::too_many_lines,
    clippy::match_same_arms,
    reason = "the native initialiser has to report and branch on every failure mode in one place"
)]
fn spawn_real() -> WindowHost {
    let alive = Arc::new(AtomicBool::new(false));
    let last_error = Arc::new(Mutex::new(None));

    let (tx, rx) = mpsc::channel::<Result<EventLoopProxy<HostEvent>, String>>();

    // In-process IPC channel bridged from the wry IPC handler.
    let (ipc_tx, ipc_rx) = mpsc::channel::<HostIpcEvent>();
    let ipc_tx_handler = ipc_tx.clone();

    let alive_thread = alive.clone();
    let error_thread = last_error.clone();
    let handle = thread::spawn(move || {
        let mut builder = EventLoopBuilder::<HostEvent>::with_user_event();
        #[cfg(target_os = "windows")]
        {
            use tao::platform::windows::EventLoopBuilderExtWindows as _;
            builder.with_any_thread(true);
        }
        let event_loop = builder.build();
        let proxy = event_loop.create_proxy();

        let window = match WindowBuilder::new().with_title("NC-UI").build(&event_loop) {
            Ok(window) => window,
            Err(err) => {
                if let Ok(mut guard) = error_thread.lock() {
                    *guard = Some(err.to_string());
                }
                let _ = tx.send(Err(err.to_string()));
                return;
            }
        };

        let webview = {
            #[cfg(any(
                target_os = "windows",
                target_os = "macos",
                target_os = "ios",
                target_os = "android"
            ))]
            {
                WebViewBuilder::new()
                    .with_url("about:blank")
                    .with_ipc_handler(move |request: wry::http::Request<String>| {
                        // Bridge the page's IPC payload into the in-process channel so the
                        // NC-UI pipeline (connected later) can drain it.
                        let _ = ipc_tx_handler.send(request.body().to_owned());
                    })
                    .build(&window)
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
                let vbox = window.default_vbox();
                match vbox {
                    Some(vbox) => WebViewBuilder::new()
                        .with_url("about:blank")
                        .build_gtk(vbox),
                    None => {
                        return;
                    }
                }
            }
        };

        let webview = match webview {
            Ok(webview) => webview,
            Err(err) => {
                if let Ok(mut guard) = error_thread.lock() {
                    *guard = Some(err.to_string());
                }
                let _ = tx.send(Err(err.to_string()));
                return;
            }
        };

        alive_thread.store(true, Ordering::SeqCst);
        let _ = tx.send(Ok(proxy.clone()));

        event_loop.run(move |event, _target, control_flow| {
            *control_flow = ControlFlow::Wait;
            match event {
                Event::UserEvent(HostEvent::Eval(script)) => {
                    let _ = webview.evaluate_script(&script);
                }
                Event::UserEvent(HostEvent::Close) => {
                    *control_flow = ControlFlow::Exit;
                }
                Event::WindowEvent {
                    event: WindowEvent::CloseRequested,
                    ..
                } => {
                    *control_flow = ControlFlow::Exit;
                }
                _ => {}
            }
        });
    });

    let proxy = match rx.recv_timeout(Duration::from_secs(15)) {
        Ok(Ok(proxy)) => Some(proxy),
        Ok(Err(message)) => {
            if let Ok(mut guard) = last_error.lock() {
                *guard = Some(message);
            }
            None
        }
        Err(_) => {
            if let Ok(mut guard) = last_error.lock() {
                *guard = Some("timed out waiting for host".to_owned());
            }
            None
        }
    };

    WindowHost {
        alive,
        last_error,
        ipc_tx,
        ipc_rx,
        proxy: Mutex::new(proxy),
        thread: Some(handle),
    }
}

#[cfg(feature = "wry-engine")]
impl Drop for WindowHost {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.proxy.lock()
            && let Some(proxy) = guard.take()
        {
            let _ = proxy.send_event(HostEvent::Close);
        }
        // The event loop thread is left to exit on its own; joining would block the caller.
    }
}

#[cfg(all(test, feature = "wry-engine"))]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn host_receives_a_message_posted_by_the_page() {
        let host = WindowHost::new();
        assert!(
            host.is_alive(),
            "host did not come up: {:?}",
            host.last_error()
        );

        // Give the engine a moment to finish initialising the page/ipc hook.
        thread::sleep(Duration::from_millis(250));

        host.eval("window.ipc.postMessage('hello from page')");

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

        assert_eq!(
            received.as_deref(),
            Some("hello from page"),
            "host did not receive the posted IPC message"
        );
        drop(host);
    }
}
