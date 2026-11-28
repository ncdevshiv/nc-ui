//! The host objects the registry owns: one per native window, plus the factory that
//! makes them.
//!
//! A host is a thin shell around three things: the command channel into the engine
//! thread, the liveness flag the engine flips when the window goes, and the event sink
//! the view inside emits into. Everything it is asked becomes a round-trip down the
//! channel, with the timeout standing guard: five seconds without an answer is not a slow
//! engine, it is a dead one, and the host says so rather than waiting forever.
//!
//! The web half of the contract is implemented honestly rather than usefully: there is no
//! page, so there are no page messages, no script evaluation, no zoom, no inspector and no
//! child webviews. Those answers are not the host being lazy — they are the truth of what
//! the window is, and the native half is where the real behaviour lives.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ncui_core::WindowId;
use ncui_webview::native::NativeViewBackend;
use ncui_webview::{
    HostFactory, NativeViewCommand, NativeViewError, Theme, WebViewBackend, WindowChange,
    WindowGeometry, WindowState,
};

use crate::GpuiHostError;
use crate::engine::{
    ENGINE_REPLY_TIMEOUT, EngineCommand, EngineSender, OpenWindowOutcome, OpenWindowSpec, engine,
};
use crate::provider::NativeEventSink;

/// Sends one command into the engine and waits for its answer.
///
/// The three ways this fails are the three things a caller can act on: the channel
/// refusing the send means the engine is gone; the timeout means it is gone or stuck,
/// and there is no honest way to tell those apart from outside; and a disconnected reply
/// means the command's answer died with a panic inside the bridge. All three are engine
/// failures rather than window failures, which is why they share the one error.
///
/// Each failure is logged where it is observed, because these are the paths an operator
/// debugs at 3am: a typed error travels back to the caller, but the caller is a pump
/// that cannot say when it happened or what else was failing beside it. The levels are
/// the failure's own meaning. A refused send is a symptom — the engine's end was already
/// logged at its source — so it is a warning. A timeout is a discovery: the engine
/// looked alive and did not answer, which is the partial hang ADR-020 names as the two
/// loops' shared failure mode. A lost answer is a defect — the command panicked inside
/// the bridge — and defects are errors.
fn roundtrip<T>(
    engine: &EngineSender,
    operation: &'static str,
    command: impl FnOnce(flume::Sender<Result<T, GpuiHostError>>) -> EngineCommand,
) -> Result<T, GpuiHostError> {
    let (reply_tx, reply_rx) = flume::bounded(1);
    if engine.send(command(reply_tx)).is_err() {
        tracing::warn!(
            operation,
            "a gpui engine command could not be sent: the engine thread is gone"
        );
        return Err(GpuiHostError::EngineUnavailable {
            reason: "the engine thread is gone: the command channel is closed".to_owned(),
        });
    }
    match reply_rx.recv_timeout(ENGINE_REPLY_TIMEOUT) {
        Ok(result) => result,
        Err(flume::RecvTimeoutError::Timeout) => {
            tracing::error!(
                operation,
                timeout_ms = ENGINE_REPLY_TIMEOUT.as_millis() as u64,
                "the gpui engine did not answer a command in time"
            );
            Err(GpuiHostError::EngineTimeout {
                operation: operation.to_owned(),
            })
        }
        Err(flume::RecvTimeoutError::Disconnected) => {
            tracing::error!(
                operation,
                "the answer to a gpui engine command was lost with the command"
            );
            Err(GpuiHostError::EngineUnavailable {
                reason: format!(
                    "the answer to {operation} was lost: the command panicked inside the engine"
                ),
            })
        }
    }
}

/// The window host handed to the registry: one per native window.
///
/// `Send` but not `Sync`, matching every other host: the registry drives hosts behind
/// its own lock, and the methods take `&self` with interior mutability because the real
/// objects live on the engine thread and these are channel operations against them.
pub struct GpuiWindowHost {
    /// The registry's identity for this window, as engine commands name it.
    window: WindowId,
    /// The command channel into the engine thread.
    engine: EngineSender,
    /// The liveness flag the engine flips when the window goes — the only state a host
    /// can read without a round-trip, and the reason `is_alive` is cheap enough to ask
    /// in a pump.
    alive: Arc<AtomicBool>,
    /// The view's event sink: the host's end of the native event channel.
    events: Arc<NativeEventSink>,
    /// The last failure this host observed, as `last_error` reports it.
    error: Mutex<Option<String>>,
}

impl core::fmt::Debug for GpuiWindowHost {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GpuiWindowHost")
            .field("window", &self.window)
            .field("alive", &self.alive.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl GpuiWindowHost {
    /// Records a failure so `last_error` can answer for it, returning the failure.
    ///
    /// The error is borrowed because recording is all that happens to it: the string is
    /// the host's copy, and nothing here consumes the typed value.
    fn note<T>(&self, error: &GpuiHostError) -> Option<T> {
        if let Ok(mut slot) = self.error.lock() {
            *slot = Some(error.to_string());
        }
        None
    }
}

impl Drop for GpuiWindowHost {
    fn drop(&mut self) {
        // Closing is fire-and-forget: the send is non-blocking because the channel is
        // unbounded, and a reply would tell us nothing we act on. When the engine is
        // already gone the send simply fails, which is the same outcome the close would
        // have had — the window is gone either way.
        let (reply_tx, _reply_rx) = flume::bounded(1);
        let _ = self.engine.send(EngineCommand::CloseWindow {
            window: self.window,
            reply: reply_tx,
        });
    }
}

impl WebViewBackend for GpuiWindowHost {
    fn last_error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|slot| slot.clone())
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    fn poll_page_message(&self) -> Option<ncui_webview::PageMessage> {
        // There is no page. `None` is not a missing implementation — it is the complete
        // and correct answer, forever, for a window whose content is Rust.
        None
    }

    fn evaluate_script(&self, _script: &str) -> bool {
        false
    }

    fn geometry(&self) -> Option<WindowGeometry> {
        let window = self.window;
        match roundtrip(&self.engine, "geometry", |reply| EngineCommand::Geometry {
            window,
            reply,
        }) {
            Ok(geometry) => Some(geometry),
            Err(error) => self.note(&error),
        }
    }

    fn window_state(&self) -> Option<WindowState> {
        // gpui 0.2.2 cannot read minimized or visibility back — `IsIconic` is used inside
        // the platform layer but never exposed — so there is no honest state to report:
        // one correct-except-minimized is a lie about one field. The contract's `None` is
        // exactly for this: the backend cannot say.
        None
    }

    fn set_window_state(&self, change: WindowChange) -> Option<WindowState> {
        // The change itself is performed — minimise, maximise, fullscreen and focus are
        // real operations — but the state cannot be read back honestly (see
        // `window_state`), so there is no state to answer with. The refusal is recorded
        // so the caller can see the change was dispatched even though nothing came back.
        let window = self.window;
        let result: Result<(), GpuiHostError> =
            roundtrip(&self.engine, "set_window_state", |reply| {
                EngineCommand::SetWindowState {
                    window,
                    change,
                    reply,
                }
            });
        match result {
            Ok(()) => None,
            Err(error) => self.note(&error),
        }
    }

    fn set_window_shape(&self, shape: &ncui_webview::WindowShape) -> bool {
        // The capability matrix is gpui 0.2.2's own: a title can be set and a size can be
        // resized to, but a window cannot be moved after creation, cannot float above the
        // others, and cannot change whether the user may resize it — those are fixed when
        // the window is made. A shape asking for anything outside the matrix is refused
        // whole rather than half-applied, because `true` means the window took the shape
        // it was asked for — not the parts of it that happened to be possible.
        let unsupported = shape.x.is_some()
            || shape.y.is_some()
            || shape.always_on_top.is_some()
            || shape.resizable.is_some();
        if unsupported {
            if let Ok(mut slot) = self.error.lock() {
                *slot = Some(
                    "gpui 0.2.2 cannot move a window, float it, or change its resizing after                      creation: only the title and size of a shape can be applied"
                        .to_owned(),
                );
            }
            return false;
        }
        if shape.is_empty() {
            return true;
        }
        let window = self.window;
        let shape = shape.clone();
        let result: Result<(), GpuiHostError> =
            roundtrip(&self.engine, "set_window_shape", |reply| {
                EngineCommand::SetShape {
                    window,
                    shape,
                    reply,
                }
            });
        match result {
            Ok(()) => true,
            Err(error) => {
                if let Ok(mut slot) = self.error.lock() {
                    *slot = Some(error.to_string());
                }
                false
            }
        }
    }

    fn window_theme(&self) -> Option<Theme> {
        let window = self.window;
        match roundtrip(&self.engine, "window_theme", |reply| {
            EngineCommand::WindowTheme { window, reply }
        }) {
            Ok(theme) => Some(theme),
            Err(error) => self.note(&error),
        }
    }

    fn native_view<'a>(&'a self) -> Option<&'a (dyn NativeViewBackend + 'a)> {
        Some(self)
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

impl NativeViewBackend for GpuiWindowHost {
    fn poll_native_event(&self) -> Option<ncui_webview::NativeViewEvent> {
        self.events.poll()
    }

    fn send_view_command(&self, command: NativeViewCommand) -> Result<(), NativeViewError> {
        if !command.is_within_bounds() {
            return Err(NativeViewError::OutOfBounds {
                reason: format!(
                    "the command exceeds the native boundary: payloads are bounded at {} bytes \
                     and names at {}",
                    ncui_webview::MAX_NATIVE_PAYLOAD_BYTES,
                    ncui_webview::native::MAX_NATIVE_NAME_BYTES,
                ),
            });
        }
        if !self.is_alive() {
            return Err(NativeViewError::WindowGone);
        }
        let window = self.window;
        let result = match command {
            NativeViewCommand::UpdateProps { props } => {
                roundtrip(&self.engine, "update_props", |reply| {
                    EngineCommand::UpdateProps {
                        window,
                        props,
                        reply,
                    }
                })
            }
            NativeViewCommand::Invoke { action, payload } => {
                roundtrip(&self.engine, "invoke", |reply| EngineCommand::Invoke {
                    window,
                    action,
                    payload,
                    reply,
                })
            }
        };
        result.map_err(|error| {
            let mapped = match error {
                GpuiHostError::WindowGone { .. } => NativeViewError::WindowGone,
                GpuiHostError::EngineUnavailable { reason } => {
                    NativeViewError::EngineUnavailable { reason }
                }
                GpuiHostError::EngineTimeout { operation } => NativeViewError::EngineUnavailable {
                    reason: format!("the engine did not answer {operation} in time"),
                },
                other => NativeViewError::Refused {
                    reason: other.to_string(),
                },
            };
            if let Ok(mut slot) = self.error.lock() {
                *slot = Some(mapped.to_string());
            }
            mapped
        })
    }

    fn dropped_native_events(&self) -> u64 {
        self.events.dropped()
    }
}

/// The factory the registry installs to host native windows.
///
/// Construction starts the engine thread — a factory that exists but cannot answer is a
/// worse failure than a factory that never existed — so construction is fallible and the
/// error is the engine's own account of why it is not running.
#[derive(Debug)]
pub struct GpuiHostFactory {
    /// The command channel into the engine thread.
    engine: EngineSender,
    /// The last refusal the factory produced, as `last_error` reports it.
    error: Mutex<Option<String>>,
}

impl GpuiHostFactory {
    /// Builds the factory, starting the engine thread if this process has not already.
    ///
    /// The engine's application is constructed with gpui's default asset source — the
    /// `()` that answers every load with absence. A factory whose windows need real
    /// assets is built with [`GpuiHostFactory::with_asset_source`] instead.
    ///
    /// # Errors
    ///
    /// Returns [`GpuiHostError::EngineUnavailable`] when the engine thread could not
    /// start or has already died — the reason inside is the platform's own account:
    /// a failed device, a missing display, or a loop that ended.
    pub fn new() -> Result<Self, GpuiHostError> {
        Self::start(None)
    }

    /// Builds the factory with the asset source the engine's application is constructed
    /// with.
    ///
    /// gpui fixes the asset source at application construction — `with_assets` exists on
    /// the application builder and nowhere else — so the source is a birthright, not a
    /// setting: the factory whose construction starts the engine is the factory whose
    /// source the application carries. That is why this is a constructor rather than a
    /// builder method on an existing factory, and why the factory does not keep a copy
    /// once construction has answered: after the engine is built the source lives inside
    /// the application, and a retained copy would pretend at a per-factory asset
    /// pipeline that one process cannot have.
    ///
    /// The source is type-erased because the composition root cannot name the concrete
    /// type — this crate is layer 3 and the sources live in layer 4 — and `Arc` because
    /// the engine is process-wide: every later factory built with the *same* source is
    /// accepted, and identity is the only comparison two type-erased trait objects can
    /// honestly support.
    ///
    /// # Errors
    ///
    /// Returns [`GpuiHostError::EngineUnavailable`] when the engine thread could not
    /// start or has already died. Returns [`GpuiHostError::AssetSourceConflict`] when
    /// the engine is already running with a different source — or with no source at all
    /// — because the first construction wins. A caller that can accept the running
    /// engine's configuration falls back to [`GpuiHostFactory::new`]; a caller that
    /// cannot must construct this factory before anything else starts the engine.
    pub fn with_asset_source(source: Arc<dyn gpui::AssetSource>) -> Result<Self, GpuiHostError> {
        Self::start(Some(source))
    }

    /// The one construction path both public constructors share: whatever the engine is
    /// asked for, the asking is the same.
    fn start(assets: Option<Arc<dyn gpui::AssetSource>>) -> Result<Self, GpuiHostError> {
        Ok(Self {
            engine: engine(assets)?,
            error: Mutex::new(None),
        })
    }
}

impl HostFactory for GpuiHostFactory {
    fn spawn(
        &self,
        window: WindowId,
        request: &ncui_webview::WindowRequest,
        seam: &Arc<ncui_webview::HostSeam>,
    ) -> Option<Box<dyn WebViewBackend>> {
        let ncui_webview::WindowContent::Native { provider, props } = &request.content else {
            let error = GpuiHostError::WrongContentKind {
                kind: request.content.kind_name().to_owned(),
            };
            // This factory is the backstop for a routing decision the registry already
            // made upstream — the registry routes native content here and everything
            // else away. The backstop firing therefore means the routing itself is
            // broken, which is a defect rather than a refusal, and a defect is logged.
            tracing::warn!(
                window = %window,
                kind = request.content.kind_name(),
                error_code = error.code(),
                "the gpui host was asked for content that is not native: the routing upstream is broken"
            );
            if let Ok(mut slot) = self.error.lock() {
                *slot = Some(error.to_string());
            }
            return None;
        };

        let spec = OpenWindowSpec {
            window,
            provider: provider.clone(),
            props: props.clone(),
            request: request.clone(),
            // The seam the registry built for this window carries the runtime's service
            // channel when it offers one — with this window's narrowed grant already
            // closed over, so what reaches the provider can never be wider than what
            // the window was actually given.
            services: seam.services().cloned(),
        };
        let result: Result<OpenWindowOutcome, GpuiHostError> =
            roundtrip(&self.engine, "open_window", |reply| {
                EngineCommand::OpenWindow {
                    spec: Box::new(spec),
                    reply,
                }
            });
        match result {
            Ok(outcome) => Some(Box::new(GpuiWindowHost {
                window,
                engine: self.engine.clone(),
                alive: outcome.alive,
                events: outcome.events,
                error: Mutex::new(None),
            })),
            Err(error) => {
                if let Ok(mut slot) = self.error.lock() {
                    *slot = Some(error.to_string());
                }
                None
            }
        }
    }

    fn monitors(&self) -> Vec<ncui_webview::MonitorRect> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        if self
            .engine
            .send(EngineCommand::Monitors { reply: reply_tx })
            .is_err()
        {
            return Vec::new();
        }
        reply_rx
            .recv_timeout(ENGINE_REPLY_TIMEOUT)
            .unwrap_or_default()
    }

    fn last_error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|slot| slot.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The smallest asset source that is not gpui's default: it proves the plumbing is
    /// about the configured object rather than about the `()` the engine carries when
    /// nobody configures one.
    #[derive(Debug)]
    struct TestAssets;

    impl gpui::AssetSource for TestAssets {
        fn load(&self, _path: &str) -> gpui::Result<Option<std::borrow::Cow<'static, [u8]>>> {
            Ok(None)
        }

        fn list(&self, _path: &str) -> gpui::Result<Vec<gpui::SharedString>> {
            Ok(Vec::new())
        }
    }

    /// The ordering invariant this test relies on: it is the *only* lib test that starts
    /// the engine, because the engine is process-wide and the first construction decides
    /// what the application carries. Any future lib test that constructs a factory must
    /// go through this one or accept whatever ordering the parallel runner chooses —
    /// which is why none may. The display-gated integration suite proves the source
    /// actually reaches the running application; what this test proves is the factory
    /// half: construction succeeds, identity is honoured, and a mismatch is the typed
    /// conflict rather than a silent drop.
    #[test]
    fn a_factory_configured_with_an_asset_source_carries_it_into_the_engine() {
        let source: Arc<dyn gpui::AssetSource> = Arc::new(TestAssets);
        let factory = match GpuiHostFactory::with_asset_source(Arc::clone(&source)) {
            Ok(factory) => factory,
            Err(GpuiHostError::EngineUnavailable { .. }) => {
                // No display on this machine: the engine cannot start, so the test skips
                // rather than fails — the same contract the integration suite keeps.
                return;
            }
            Err(error) => {
                panic!("the factory failed in a way the test does not understand: {error}")
            }
        };
        drop(factory);

        // The running engine carries this source, so asking for the very same
        // allocation again is accepted: identity is the honest comparison.
        GpuiHostFactory::with_asset_source(Arc::clone(&source))
            .expect("the running engine's own source is accepted");
        // A factory asking for nothing accepts whatever the engine carries.
        GpuiHostFactory::new().expect("a factory without a source accepts the running engine");
        // A different source cannot be installed — gpui fixes the source at
        // construction — and the refusal is the typed conflict, with the field that
        // says the engine does carry one.
        let conflict = GpuiHostFactory::with_asset_source(Arc::new(TestAssets));
        assert!(
            matches!(
                conflict,
                Err(GpuiHostError::AssetSourceConflict {
                    engine_has_assets: true
                })
            ),
            "a different asset source on a running engine is a typed conflict: {conflict:?}"
        );
    }
}
