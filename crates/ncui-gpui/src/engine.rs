//! The engine thread: the one thread gpui's Windows platform may live on.
//!
//! gpui's Windows backend captures its constructing thread as the main thread and pumps a
//! `GetMessageW` loop on it. The engine is therefore one dedicated thread, named
//! `ncui-gpui-loop`, on which the `gpui::Application` is both constructed and run — and
//! every operation a host performs is a command sent into that thread with a reply
//! channel. No gpui object is ever touched off the engine thread, because gpui's `App`
//! is a `RefCell` behind an `Rc`: the borrow rules are the threading rules.
//!
//! The bridge is a foreground future draining the command channel. Foreground futures
//! are polled on gpui's main thread, so inside the bridge every `AsyncApp` method is a
//! legal borrow, and the windows map can be local state rather than shared state —
//! there is exactly one thread in the engine's world, and the bridge is it.
//!
//! # What gpui 0.2.2 cannot answer
//!
//! The platform integration is honest about three gaps rather than papering over them:
//!
//! * There is no programmatic repositioning — a window's place is fixed at creation.
//! * There is no always-on-top — `WindowKind::PopUp` is a tool window, not a topmost one.
//! * Minimized and hidden state cannot be read back — `IsIconic` is used inside gpui but
//!   never exposed — so `window_state` reports nothing rather than a state that is right
//!   except for one field, because a state right-except-one-field is a lie about one
//!   field. The changes themselves — minimise, maximise, fullscreen, focus — are real
//!   and are performed; it is the read-back that does not exist.

use std::borrow::Cow;
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use ncui_core::WindowId;
use ncui_webview::{MonitorRect, Theme, WindowGeometry};

use crate::GpuiHostError;
use crate::provider::{NativeEventSink, NativeViewInstance};

/// The type-erased asset source as gpui's builder needs it: a concrete `impl AssetSource`.
///
/// gpui's `Application::with_assets` takes the source by value as a concrete type, and
/// gpui implements the trait only for `()` — there is no implementation for
/// `Arc<dyn AssetSource>`, because the trait is the framework's and so is the choice of
/// how sources compose. The composition root hands this crate a type-erased `Arc` — it
/// cannot name a concrete type it does not know — so this newtype is the bridge between
/// the two shapes: one concrete type whose whole behaviour is delegating to the shared
/// source it carries.
struct SharedAssetSource(Arc<dyn gpui::AssetSource>);

impl gpui::AssetSource for SharedAssetSource {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        self.0.load(path)
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<gpui::SharedString>> {
        self.0.list(path)
    }
}

/// How long a host waits for the engine to answer before the answer is a timeout.
///
/// Five seconds is long enough that a healthy engine never trips it — commands are
/// messages in a queue, not work — and short enough that a dead engine is reported as
/// dead rather than hanging the runtime's pump.
pub(crate) const ENGINE_REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// What a window-open command carries into the engine.
#[derive(Debug)]
pub(crate) struct OpenWindowSpec {
    /// The registry's identity for the window.
    pub window: WindowId,
    /// The provider the content asked for.
    pub provider: String,
    /// The props the view is built with.
    pub props: serde_json::Value,
    /// The frame the window was asked for: title, size, position and presentation flags.
    pub request: ncui_webview::WindowRequest,
}

/// What a successful window-open hands back to the host.
#[derive(Debug)]
pub(crate) struct OpenWindowOutcome {
    /// The view's event sink, shared with the host.
    pub events: Arc<NativeEventSink>,
    /// The shared liveness flag the engine flips when the window goes.
    pub alive: Arc<AtomicBool>,
}

/// The engine-half bookkeeping for one open window.
pub(crate) struct EngineWindow {
    /// The gpui window handle, type-erased because the root is always the host's wrapper.
    pub(crate) handle: gpui::AnyWindowHandle,
    /// The provider's live instance: the half that knows how its own view rebuilds.
    pub(crate) instance: Box<dyn NativeViewInstance>,
    /// The shared liveness flag. Flipped here when the window is found gone — and read
    /// by the host at any time, which is why a host never round-trips for liveness.
    pub(crate) alive: Arc<AtomicBool>,
    /// The provider the window's view came from, so a refusal can name who refused.
    pub(crate) provider: String,
}

/// One command the engine thread executes.
///
/// Every variant carries its own reply channel, because the host is synchronous and the
/// engine is a loop: the reply is how a command with an answer gets the answer home.
/// The variants are data rather than closures so the channel's contents are inspectable
/// — a tracer can say what was asked without executing anything.
#[derive(Debug)]
pub(crate) enum EngineCommand {
    /// Open one native window.
    OpenWindow {
        /// Everything the open needs.
        spec: Box<OpenWindowSpec>,
        /// Where the outcome goes.
        reply: flume::Sender<Result<OpenWindowOutcome, GpuiHostError>>,
    },
    /// Close one window. Closing a window that is already gone is not an error — the
    /// caller asked for a world in which the window is closed, and that world is here.
    CloseWindow {
        /// Which window.
        window: WindowId,
        /// Where confirmation goes.
        reply: flume::Sender<()>,
    },
    /// Replace one window's view props.
    UpdateProps {
        /// Which window.
        window: WindowId,
        /// The new props.
        props: serde_json::Value,
        /// Where the outcome goes.
        reply: flume::Sender<Result<(), GpuiHostError>>,
    },
    /// Invoke a provider verb on one window's view.
    Invoke {
        /// Which window.
        window: WindowId,
        /// The verb.
        action: String,
        /// Its arguments.
        payload: serde_json::Value,
        /// Where the outcome goes.
        reply: flume::Sender<Result<(), GpuiHostError>>,
    },
    /// Read one window's geometry back from the platform.
    Geometry {
        /// Which window.
        window: WindowId,
        /// Where the answer goes.
        reply: flume::Sender<Result<WindowGeometry, GpuiHostError>>,
    },
    /// Ask for one change to a window's presentation.
    SetWindowState {
        /// Which window.
        window: WindowId,
        /// The change.
        change: ncui_webview::WindowChange,
        /// Where confirmation goes. The payload is unit because the state cannot be
        /// honestly read back — see the module documentation.
        reply: flume::Sender<Result<(), GpuiHostError>>,
    },
    /// Read the platform's light/dark appearance.
    WindowTheme {
        /// Which window.
        window: WindowId,
        /// Where the answer goes.
        reply: flume::Sender<Result<Theme, GpuiHostError>>,
    },
    /// Apply a window shape: the parts of it gpui can perform.
    SetShape {
        /// Which window.
        window: WindowId,
        /// The shape, already vetted by the host so every field it carries is one gpui
        /// can perform — a shape asking for more never reaches the engine.
        shape: ncui_webview::WindowShape,
        /// Where confirmation goes.
        reply: flume::Sender<Result<(), GpuiHostError>>,
    },
    /// The displays this machine has, in logical pixels.
    Monitors {
        /// Where the answer goes.
        reply: flume::Sender<Vec<MonitorRect>>,
    },
    /// gpui reported that a window was dropped. The observer cannot say which one — it
    /// fires after the window is gone, carrying no identity — so the bridge reconciles:
    /// every window it tracks is checked against the application's live windows, and the
    /// ones no longer there are the ones that went.
    ReconcileWindows,
    /// Shut the loop down: close every window and quit the application.
    Shutdown,
}

impl EngineCommand {
    /// The operation name instrumentation uses for the variant.
    ///
    /// The variants carry data too large and too private to print into a log line — a
    /// window-open spec holds the caller's props — so the name is a static string rather
    /// than a `Debug` rendering: a trace can say *what* was asked without quoting
    /// everything the asking carried.
    fn operation(&self) -> &'static str {
        match self {
            Self::OpenWindow { .. } => "open_window",
            Self::CloseWindow { .. } => "close_window",
            Self::UpdateProps { .. } => "update_props",
            Self::Invoke { .. } => "invoke",
            Self::Geometry { .. } => "geometry",
            Self::SetWindowState { .. } => "set_window_state",
            Self::WindowTheme { .. } => "window_theme",
            Self::SetShape { .. } => "set_window_shape",
            Self::Monitors { .. } => "monitors",
            Self::ReconcileWindows => "reconcile_windows",
            Self::Shutdown => "shutdown",
        }
    }
}

/// The engine's sender half, handed to the factory and through it to every host.
pub(crate) type EngineSender = flume::Sender<EngineCommand>;

/// The process-wide engine state: the sender, plus the death report when the thread is
/// gone. A host never panics because the engine died — it reports the death.
struct EngineState {
    /// The command channel into the engine thread.
    sender: EngineSender,
    /// Why the engine is unavailable, when it is. Written once, at death or at a failed
    /// birth, and read by every factory and host that needs to say why.
    death: Arc<Mutex<Option<String>>>,
    /// The asset source the application was constructed with, when it carries one.
    ///
    /// The application owns its own copy and uses it; this one exists for the identity
    /// comparison alone. gpui fixes the source at construction, so a factory that asks
    /// for assets after the engine is running can only be answered honestly by comparing
    /// what it asks for against what the engine has — and type-erased trait objects
    /// cannot be compared for anything deeper than the allocation they share.
    assets: Option<Arc<dyn gpui::AssetSource>>,
}

static ENGINE: OnceLock<EngineState> = OnceLock::new();

/// Whether the engine thread has been asked to start in this process.
static ENGINE_STARTED: AtomicBool = AtomicBool::new(false);

/// Whether the asset source a factory asks for can honestly be promised, given the
/// source the running engine was constructed with.
///
/// gpui fixes the application's asset source at construction, so once the engine runs
/// there are exactly two honourable answers: the factory asks for nothing and accepts
/// whatever the engine has, or it asks for the very source the engine carries — the
/// same `Arc` allocation, because two type-erased trait objects cannot be compared for
/// anything deeper than the allocation they share. Everything else is an ordering
/// mistake in the composition, reported typed rather than silently dropped: a source
/// quietly discarded is svg assets that never load with no evidence anywhere of why.
fn assets_compatible(
    installed: Option<&Arc<dyn gpui::AssetSource>>,
    requested: Option<&Arc<dyn gpui::AssetSource>>,
) -> Result<(), GpuiHostError> {
    let Some(requested) = requested else {
        return Ok(());
    };
    if let Some(installed) = installed
        && Arc::ptr_eq(installed, requested)
    {
        return Ok(());
    }
    Err(GpuiHostError::AssetSourceConflict {
        engine_has_assets: installed.is_some(),
    })
}

/// Starts the engine thread if it is not already running, returning the command sender.
///
/// The engine is process-wide because gpui is: one process has one `Application`, and a
/// second one would be a second main loop claiming the same name. Starting twice is not
/// an error — the first engine is the only one — because the alternative is a race
/// between whoever asks first and whoever asks second.
///
/// The asset source travels with the start because gpui fixes it at application
/// construction: the caller whose ask starts the engine is the caller whose source the
/// application carries. Every later caller is answered by [`assets_compatible`] — the
/// running engine cannot be reconfigured, and pretending otherwise is how svg assets go
/// missing in silence.
pub(crate) fn engine(
    assets: Option<Arc<dyn gpui::AssetSource>>,
) -> Result<EngineSender, GpuiHostError> {
    if let Some(state) = ENGINE.get() {
        if let Some(death) = state.death.lock().ok().and_then(|slot| slot.clone()) {
            return Err(GpuiHostError::EngineUnavailable { reason: death });
        }
        assets_compatible(state.assets.as_ref(), assets.as_ref())?;
        tracing::debug!("the gpui engine is already running; the existing engine answers");
        return Ok(state.sender.clone());
    }
    if ENGINE_STARTED.swap(true, Ordering::SeqCst) {
        // Another thread is starting the engine right now. Wait for it rather than
        // starting a second one: startup is once per process either way.
        for _ in 0..100 {
            if ENGINE.get().is_some() {
                return engine(assets);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        return Err(GpuiHostError::EngineUnavailable {
            reason: "the engine did not finish starting in time".to_owned(),
        });
    }

    tracing::info!(
        asset_source = assets.is_some(),
        "the gpui engine thread is starting"
    );

    let (sender, receiver) = flume::unbounded::<EngineCommand>();
    let death: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let thread_death = Arc::clone(&death);
    let (ready_tx, ready_rx) = flume::bounded::<Result<(), String>>(1);

    let builder = std::thread::Builder::new().name("ncui-gpui-loop".to_owned());
    let close_sender = sender.clone();
    let thread_assets = assets.clone();
    let spawned = builder.spawn(move || {
        run_engine(
            receiver,
            close_sender,
            ready_tx,
            &thread_death,
            thread_assets,
        );
    });
    if let Err(error) = spawned {
        let reason = format!("the engine thread could not be spawned: {error}");
        tracing::error!(%error, "the gpui engine thread could not be spawned");
        if let Ok(mut slot) = death.lock() {
            *slot = Some(reason.clone());
        }
        return Err(GpuiHostError::EngineUnavailable { reason });
    }

    match ready_rx.recv_timeout(ENGINE_REPLY_TIMEOUT) {
        Ok(Ok(())) => {}
        Ok(Err(reason)) => {
            // The failure was already logged at its source, on the engine thread, where
            // the construction happened — repeating it here would report one failure twice.
            if let Ok(mut slot) = death.lock() {
                *slot = Some(reason.clone());
            }
            return Err(GpuiHostError::EngineUnavailable { reason });
        }
        Err(_) => {
            // The engine thread neither reported readiness nor refused it: construction
            // is stuck inside the platform, and there is no source-side log because the
            // source is the part that hung. This line is the only record of the hang.
            tracing::error!(
                timeout_ms = ENGINE_REPLY_TIMEOUT.as_millis() as u64,
                "the gpui engine did not report readiness in time"
            );
            let reason = "the engine did not report readiness in time".to_owned();
            if let Ok(mut slot) = death.lock() {
                *slot = Some(reason.clone());
            }
            return Err(GpuiHostError::EngineUnavailable { reason });
        }
    }

    let state = EngineState {
        sender,
        death,
        assets,
    };
    let _ = ENGINE.set(state);
    engine(None)
}

/// Asks the engine to shut down: close every window and end the loop.
///
/// Shutdown is a request rather than a guarantee — the send is non-blocking, and a process
/// exiting does not wait for the answer. That is the honest shape of it: the alternative,
/// blocking the caller until the loop ends, is a deadlock the moment anyone asks from the
/// engine thread itself. When the application exits the thread's process ends with it in
/// any case; this exists so a runtime that tears down in order can ask in order.
#[cfg(feature = "gpui-engine")]
pub fn shutdown() {
    if let Some(state) = ENGINE.get() {
        let _ = state.sender.send(EngineCommand::Shutdown);
    }
}

/// The engine thread's whole life: construct the application, hand the bridge its
/// channel, and pump the loop until it ends.
///
/// `Application::new` must run on this thread — gpui captures the constructing thread as
/// its main thread, and the message-only window the dispatcher posts to belongs to
/// whichever thread creates it. Constructing anywhere else would make every dispatch a
/// message to a thread nobody pumps.
///
/// The asset source is construction input rather than a command because gpui fixes it
/// at construction: `with_assets` exists on the application builder and nowhere else.
/// When no source is configured the application is built bare, which is gpui's own
/// default — the `()` source that answers every load with absence.
fn run_engine(
    receiver: flume::Receiver<EngineCommand>,
    close_sender: EngineSender,
    ready: flume::Sender<Result<(), String>>,
    death: &Arc<Mutex<Option<String>>>,
    assets: Option<Arc<dyn gpui::AssetSource>>,
) {
    let has_assets = assets.is_some();
    // `AssertUnwindSafe` is honest here rather than a convenience: the unwind-safety
    // contract guards against observing invariants a panic left broken, and nothing is
    // observed either way — a panic means construction failed, the failure is reported,
    // and the thread ends without the application ever existing.
    let application = catch_unwind(AssertUnwindSafe(move || match assets {
        Some(source) => gpui::Application::new().with_assets(SharedAssetSource(source)),
        None => gpui::Application::new(),
    }));
    let Ok(application) = application else {
        tracing::error!(
            "gpui application construction panicked: the platform refused a device or a display"
        );
        let _ = ready.send(Err(
            "gpui's application construction failed: the platform refused a device or a display"
                .to_owned(),
        ));
        return;
    };
    tracing::info!(
        asset_source = has_assets,
        "the gpui application is constructed; the message loop is starting"
    );

    // Whether the loop ends because the engine was asked to end it. The bridge writes
    // the flag when the shutdown command arrives; the thread reads it after `run`
    // returns, because an ending the engine asked for and an ending that simply happened
    // are different facts — the second means every window died unexpectedly.
    let shutdown_requested = Arc::new(AtomicBool::new(false));
    let bridge_shutdown = Arc::clone(&shutdown_requested);
    application.run(move |cx| {
        // Window close notification: gpui drops closed windows at the end of their
        // update cycle and tells the application afterwards, without saying which one.
        // The observer cannot reach the bridge's state — they are different owners — so
        // it asks the bridge to reconcile through the same channel every other command
        // uses, and the reconciliation happens in order with everything else.
        cx.on_window_closed(move |_cx| {
            let _ = close_sender.send(EngineCommand::ReconcileWindows);
        })
        .detach();

        let _ = ready.send(Ok(()));

        cx.spawn(async move |cx| {
            // The anchor window: one hidden window the engine opens before anything else
            // and closes only when it shuts down. gpui's Windows platform posts
            // `PostQuitMessage` when its last window closes — a rule that makes sense for
            // an application whose windows are its life, and none for an engine whose
            // windows come and go at the runtime's pace. Without the anchor, closing the
            // last native window would end the message loop for good, and every window
            // after that would be refused with a loop that no longer runs. The anchor is
            // never shown, never focused and never tracked in the windows map, so no
            // operation the engine performs can reach it.
            let anchor = cx
                .open_window(
                    gpui::WindowOptions {
                        window_bounds: Some(gpui::WindowBounds::Windowed(gpui::Bounds {
                            origin: gpui::point(gpui::px(0.0), gpui::px(0.0)),
                            size: gpui::size(gpui::px(1.0), gpui::px(1.0)),
                        })),
                        titlebar: None,
                        focus: false,
                        show: false,
                        kind: gpui::WindowKind::Normal,
                        ..Default::default()
                    },
                    |_window, cx| crate::ops::empty_root(cx),
                )
                .ok()
                .map(gpui::WindowHandle::into);
            // An anchor that could not open is an engine without its keepalive — degraded
            // rather than dead, because the failure that refused the anchor will refuse
            // every window after it, and those failures carry their own reasons.
            let anchor: Option<gpui::AnyWindowHandle> = anchor;
            if anchor.is_none() {
                tracing::warn!(
                    "the gpui anchor window could not open: the message loop will end when \
                     the last native window closes"
                );
            }

            // The bridge owns every window the engine ever opens. It is local state
            // rather than shared state because everything that touches it runs here, on
            // gpui's main thread — the one thread in the engine's world.
            let mut windows: HashMap<WindowId, EngineWindow> = HashMap::new();
            while let Ok(command) = receiver.recv_async().await {
                if matches!(command, EngineCommand::Shutdown) {
                    bridge_shutdown.store(true, Ordering::SeqCst);
                    tracing::info!(
                        windows = windows.len(),
                        "gpui engine shutdown requested; closing every window"
                    );
                    for (_, window) in windows.drain() {
                        window.alive.store(false, Ordering::SeqCst);
                        let _ = window.handle.update(cx, |_, window, _| {
                            window.remove_window();
                        });
                    }
                    if let Some(anchor) = anchor {
                        let _ = anchor.update(cx, |_, window, _| {
                            window.remove_window();
                        });
                    }
                    let _ = cx.update(|cx| cx.quit());
                    break;
                }
                execute(command, cx, &mut windows);
            }
        })
        .detach();
    });

    // `run` returns only when the loop is over. Every host still holding a liveness flag
    // must learn the engine is gone, which is what the death slot is for: the next time
    // anything asks, it hears why rather than hanging.
    if shutdown_requested.load(Ordering::SeqCst) {
        tracing::info!("the gpui engine thread has exited");
    } else {
        tracing::error!(
            "the gpui message loop ended without a shutdown request: every native window \
             is gone with it"
        );
    }
    if let Ok(mut slot) = death.lock()
        && slot.is_none()
    {
        *slot = Some("the gpui message loop has ended".to_owned());
    }
}

/// Executes one command inside the bridge, with the panic of one command never becoming
/// the death of the loop.
///
/// Providers are trusted code, but trusted is not infallible: a view that panics while
/// its props update should lose its command, not the engine every other window lives
/// on. The catch is around the dispatch rather than inside each branch, because the
/// panic boundary is the same everywhere: one command, one catch.
fn execute(
    command: EngineCommand,
    cx: &mut gpui::AsyncApp,
    windows: &mut HashMap<WindowId, EngineWindow>,
) {
    let operation = command.operation();
    let caught = catch_unwind(AssertUnwindSafe(|| {
        execute_inner(command, cx, windows);
    }));
    // When the catch fires, the command's reply is simply never sent — the host's
    // timeout reports the engine as unresponsive for that operation, which is the
    // truth: the answer was lost with the panic. The loop survives to serve the next.
    // The panic itself is a defect in trusted code and is logged as one: a swallowed
    // panic with no record is how a provider's bug becomes a mystery timeout on a
    // different thread.
    if caught.is_err() {
        tracing::error!(
            operation,
            "a command panicked inside the gpui engine bridge: its answer is lost and \
             the loop survives"
        );
    }
}

fn execute_inner(
    command: EngineCommand,
    cx: &mut gpui::AsyncApp,
    windows: &mut HashMap<WindowId, EngineWindow>,
) {
    match command {
        EngineCommand::OpenWindow { spec, reply } => {
            let result = crate::ops::open_window(cx, &spec, windows);
            let _ = reply.send(result);
        }
        EngineCommand::CloseWindow { window, reply } => {
            crate::ops::close_window(cx, window, windows);
            let _ = reply.send(());
        }
        EngineCommand::UpdateProps {
            window,
            props,
            reply,
        } => {
            let result = crate::ops::update_props(cx, window, &props, windows);
            let _ = reply.send(result);
        }
        EngineCommand::Invoke {
            window,
            action,
            payload,
            reply,
        } => {
            let result = crate::ops::invoke(cx, window, &action, &payload, windows);
            let _ = reply.send(result);
        }
        EngineCommand::Geometry { window, reply } => {
            let _ = reply.send(crate::ops::read_geometry(cx, window, windows));
        }
        EngineCommand::SetWindowState {
            window,
            change,
            reply,
        } => {
            let _ = reply.send(crate::ops::change_state(cx, window, change, windows));
        }
        EngineCommand::WindowTheme { window, reply } => {
            let _ = reply.send(crate::ops::read_theme(cx, window, windows));
        }
        EngineCommand::SetShape {
            window,
            shape,
            reply,
        } => {
            let _ = reply.send(crate::ops::apply_shape(cx, window, &shape, windows));
        }
        EngineCommand::Monitors { reply } => {
            let _ = reply.send(crate::ops::read_monitors(cx));
        }
        EngineCommand::ReconcileWindows => {
            crate::ops::reconcile_windows(cx, windows);
        }
        EngineCommand::Shutdown => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bare source: the compatibility rules are about identity, not content, so the
    /// test sources carry nothing — what is compared is which allocation they are.
    fn source() -> Arc<dyn gpui::AssetSource> {
        Arc::new(())
    }

    #[test]
    fn a_factory_asking_for_nothing_is_always_compatible() {
        let installed = source();
        assert!(assets_compatible(None, None).is_ok());
        assert!(assets_compatible(Some(&installed), None).is_ok());
    }

    #[test]
    fn the_running_engines_own_source_is_compatible_with_itself() {
        let installed = source();
        assert!(
            assets_compatible(Some(&installed), Some(&installed)).is_ok(),
            "the very allocation the engine carries is the one a factory may ask for"
        );
    }

    #[test]
    fn a_different_source_conflicts_with_the_one_the_engine_carries() {
        let installed = source();
        let requested = source();
        let error = assets_compatible(Some(&installed), Some(&requested))
            .expect_err("two different sources cannot both win a fixed construction");
        assert!(
            matches!(
                error,
                GpuiHostError::AssetSourceConflict {
                    engine_has_assets: true
                }
            ),
            "the conflict reports that the engine does carry a source: {error:?}"
        );
    }

    #[test]
    fn a_source_asked_of_a_bare_engine_conflicts_without_assets_on_the_engine() {
        let requested = source();
        let error = assets_compatible(None, Some(&requested))
            .expect_err("a bare engine cannot take a source after construction");
        assert!(
            matches!(
                error,
                GpuiHostError::AssetSourceConflict {
                    engine_has_assets: false
                }
            ),
            "the conflict reports that the engine carries no source: {error:?}"
        );
    }
}
