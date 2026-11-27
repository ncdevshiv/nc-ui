//! The window operations the engine performs: one function per command.
//!
//! Each of these runs inside the bridge future on gpui's main thread — the only thread
//! the platform tolerates — and each is deliberately small: take the command apart, find
//! the window, do the one thing, answer. Anything clever they could do would be cleverness
//! spent in the one place every window shares.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use ncui_core::WindowId;
use ncui_webview::{MonitorRect, Theme, WindowGeometry};

use gpui::AppContext as _;

use crate::GpuiHostError;
use crate::engine::{EngineWindow, OpenWindowOutcome, OpenWindowSpec};
use crate::provider::{NativeEventSink, NativeViewContext, NativeViewInstance, providers};

/// The window-root wrapper: the concrete type every native window shares.
///
/// gpui's `open_window` needs a root entity of one concrete type implementing `Render`,
/// and a provider's view is type-erased into `AnyView` — which is an element but not a
/// `Render`. The wrapper is the bridge between those two facts: one trivial entity whose
/// whole render is the view it carries.
pub(crate) struct NativeRoot {
    /// The provider's view, type-erased.
    view: gpui::AnyView,
}

impl gpui::Render for NativeRoot {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        _cx: &mut gpui::Context<'_, Self>,
    ) -> impl gpui::IntoElement {
        self.view.clone()
    }
}

/// An empty view: the stand-in root when a provider's build fails, because the window
/// must still have a root to be closed cleanly and there is nothing else to show.
struct EmptyView;

impl gpui::Render for EmptyView {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        _cx: &mut gpui::Context<'_, Self>,
    ) -> impl gpui::IntoElement {
        gpui::div()
    }
}

/// A root wrapping an empty view, for the failure branches of a window open.
pub(crate) fn empty_root(cx: &mut gpui::App) -> gpui::Entity<NativeRoot> {
    let empty: gpui::AnyView = cx.new(|_| EmptyView).into();
    cx.new(|_| NativeRoot { view: empty })
}

/// The gpui frame a [`ncui_webview::WindowRequest`] describes.
///
/// Position is creation-time only — gpui 0.2.2 cannot move a window afterwards — so the
/// bounds carry the whole of the request's opinion about place: explicit when the caller
/// named both axes, centered on the primary display when it named neither, and centered
/// when it named one, because a window placed by half an opinion is placed by no opinion.
pub(crate) fn window_options(
    request: &ncui_webview::WindowRequest,
    cx: &gpui::App,
) -> gpui::WindowOptions {
    let logical_size = gpui::size(
        gpui::px(request.width as f32),
        gpui::px(request.height as f32),
    );
    let base = match (request.x, request.y) {
        (Some(x), Some(y)) => gpui::WindowBounds::Windowed(gpui::Bounds {
            origin: gpui::point(gpui::px(x as f32), gpui::px(y as f32)),
            size: logical_size,
        }),
        _ => gpui::WindowBounds::centered(logical_size, cx),
    };
    let bounds = if request.fullscreen {
        gpui::WindowBounds::Fullscreen(base.get_bounds())
    } else if request.maximized {
        gpui::WindowBounds::Maximized(base.get_bounds())
    } else {
        base
    };
    gpui::WindowOptions {
        window_bounds: Some(bounds),
        titlebar: Some(gpui::TitlebarOptions {
            title: Some(request.title.clone().into()),
            // On Windows this flag is what strips the non-client area: a window without
            // decorations is one whose frame gpui does not draw, and the view inside is
            // then responsible for its own chrome.
            appears_transparent: !request.decorations,
            ..Default::default()
        }),
        kind: gpui::WindowKind::Normal,
        is_resizable: request.resizable,
        window_min_size: match (request.min_width, request.min_height) {
            (Some(width), Some(height)) => {
                Some(gpui::size(gpui::px(width as f32), gpui::px(height as f32)))
            }
            _ => None,
        },
        window_background: if request.transparent {
            gpui::WindowBackgroundAppearance::Transparent
        } else {
            gpui::WindowBackgroundAppearance::Opaque
        },
        ..Default::default()
    }
}

/// Opens one native window: build the view inside gpui's closure, keep the instance.
///
/// The provider's build can only happen inside `open_window`'s closure — that is the
/// first moment a `&mut Window` exists — but the closure's return type has no room for
/// failure. The outcome therefore travels through a slot the closure writes and the
/// bridge reads afterwards: a failed build closes the window it just made and reports
/// the provider's refusal, rather than leaving an empty window on the screen as a
/// monument to a view that never built.
pub(crate) fn open_window(
    cx: &mut gpui::AsyncApp,
    spec: OpenWindowSpec,
    windows: &mut HashMap<WindowId, EngineWindow>,
) -> Result<OpenWindowOutcome, GpuiHostError> {
    let provider = providers()
        .get(&spec.provider)
        .ok_or_else(|| GpuiHostError::UnknownProvider {
            provider: spec.provider.clone(),
        })?;

    let sink = Arc::new(NativeEventSink::new());
    let alive = Arc::new(AtomicBool::new(true));
    let context = NativeViewContext {
        window: spec.window,
        props: spec.props.clone(),
        events: Arc::clone(&sink),
    };

    let options = cx.update(|cx| window_options(&spec.request, cx)).map_err(|error| {
        GpuiHostError::EngineUnavailable {
            reason: format!("the application could not prepare the window's frame: {error}"),
        }
    })?;

    // The slot the closure writes. `Rc` would do on one thread, but the closure is
    // `Send`-bound through gpui's types in some configurations and the mutex costs
    // nothing here — it is locked twice in the lifetime of the window.
    let build_outcome: Arc<Mutex<Option<Result<Box<dyn NativeViewInstance>, String>>>> =
        Arc::new(Mutex::new(None));
    let closure_outcome = Arc::clone(&build_outcome);

    let build = move |window: &mut gpui::Window, cx: &mut gpui::App| {
        let built = catch_unwind(AssertUnwindSafe(|| provider.build(context, window, cx)));
        match built {
            Ok(Ok((view, instance))) => {
                if let Ok(mut slot) = closure_outcome.lock() {
                    *slot = Some(Ok(instance));
                }
                cx.new(|_| NativeRoot { view })
            }
            Ok(Err(reason)) => {
                if let Ok(mut slot) = closure_outcome.lock() {
                    *slot = Some(Err(reason));
                }
                empty_root(cx)
            }
            Err(_) => {
                if let Ok(mut slot) = closure_outcome.lock() {
                    *slot = Some(Err(
                        "the provider panicked while the view was being built".to_owned()
                    ));
                }
                empty_root(cx)
            }
        }
    };

    let handle = cx
        .open_window(options, build)
        .map_err(|error| GpuiHostError::EngineUnavailable {
            reason: format!("gpui could not create the window: {error}"),
        })?;

    let outcome = build_outcome
        .lock()
        .ok()
        .and_then(|mut slot| slot.take());
    match outcome {
        Some(Ok(instance)) => {
            windows.insert(
                spec.window,
                EngineWindow {
                    handle: handle.into(),
                    instance,
                    alive: Arc::clone(&alive),
                    provider: spec.provider.clone(),
                },
            );
            Ok(OpenWindowOutcome {
                events: sink,
                alive,
            })
        }
        Some(Err(reason)) => {
            let _ = handle.update(cx, |_, window, _| {
                window.remove_window();
            });
            Err(GpuiHostError::ProviderRefused {
                provider: spec.provider.clone(),
                reason,
            })
        }
        None => {
            let _ = handle.update(cx, |_, window, _| {
                window.remove_window();
            });
            Err(GpuiHostError::EngineUnavailable {
                reason: "the view's build neither succeeded nor reported its failure".to_owned(),
            })
        }
    }
}

/// Closes one window, treating an absent window as already closed.
pub(crate) fn close_window(
    cx: &mut gpui::AsyncApp,
    window: WindowId,
    windows: &mut HashMap<WindowId, EngineWindow>,
) {
    if let Some(engine_window) = windows.remove(&window) {
        engine_window.alive.store(false, Ordering::SeqCst);
        let _ = engine_window.handle.update(cx, |_, window, _| {
            window.remove_window();
        });
    }
}

/// Reconciles the tracked windows against gpui's live ones after a close notification.
///
/// gpui reports that *a* window closed without saying which, so the truth is read from
/// the application itself: the live handles it still lists. Anything the bridge tracks
/// that the application no longer lists is a window that went — its liveness is flipped
/// and its instance dropped, which is what lets the host answer `is_alive` truthfully
/// without ever round-tripping.
pub(crate) fn reconcile_windows(cx: &mut gpui::AsyncApp, windows: &mut HashMap<WindowId, EngineWindow>) {
    let live: Vec<gpui::AnyWindowHandle> = cx
        .update(|cx| cx.windows())
        .unwrap_or_default();
    let live_ids: Vec<u64> = live.iter().map(|handle| handle.window_id().as_u64()).collect();
    let gone: Vec<WindowId> = windows
        .iter()
        .filter(|(_, engine_window)| !live_ids.contains(&engine_window.handle.window_id().as_u64()))
        .map(|(id, _)| *id)
        .collect();
    for id in gone {
        if let Some(engine_window) = windows.remove(&id) {
            engine_window.alive.store(false, Ordering::SeqCst);
        }
    }
}

/// The window lookup every operation shares: present-and-tracked, or the typed refusal.
pub(crate) fn tracked<'a>(
    window: WindowId,
    windows: &'a mut HashMap<WindowId, EngineWindow>,
) -> Result<&'a mut EngineWindow, GpuiHostError> {
    windows.get_mut(&window).ok_or_else(|| GpuiHostError::WindowGone {
        window: window.to_string(),
    })
}

/// Replaces one window's props through the instance that knows what they mean.
pub(crate) fn update_props(
    cx: &mut gpui::AsyncApp,
    window: WindowId,
    props: &serde_json::Value,
    windows: &mut HashMap<WindowId, EngineWindow>,
) -> Result<(), GpuiHostError> {
    let engine_window = tracked(window, windows)?;
    let handle = engine_window.handle;
    let provider = engine_window.provider.clone();
    let instance = &mut engine_window.instance;
    handle
        .update(cx, |_, window, cx| {
            instance.update_props(props, window, cx)
        })
        .map_err(|_| GpuiHostError::WindowGone {
            window: window.to_string(),
        })?
        .map_err(|reason| GpuiHostError::ProviderRefused { provider, reason })
}

/// Invokes one provider verb through the instance.
pub(crate) fn invoke(
    cx: &mut gpui::AsyncApp,
    window: WindowId,
    action: &str,
    payload: &serde_json::Value,
    windows: &mut HashMap<WindowId, EngineWindow>,
) -> Result<(), GpuiHostError> {
    let engine_window = tracked(window, windows)?;
    let handle = engine_window.handle;
    let provider = engine_window.provider.clone();
    let instance = &mut engine_window.instance;
    handle
        .update(cx, |_, window, cx| {
            instance.invoke(action, payload, window, cx)
        })
        .map_err(|_| GpuiHostError::WindowGone {
            window: window.to_string(),
        })?
        .map_err(|reason| GpuiHostError::ProviderRefused { provider, reason })
}

/// Reads one window's geometry back from the platform.
pub(crate) fn read_geometry(
    cx: &mut gpui::AsyncApp,
    window: WindowId,
    windows: &mut HashMap<WindowId, EngineWindow>,
) -> Result<WindowGeometry, GpuiHostError> {
    let engine_window = tracked(window, windows)?;
    let handle = engine_window.handle;
    handle
        .update(cx, |_, window, _| {
            let bounds = window.bounds();
            WindowGeometry {
                x: f32::from(bounds.origin.x) as i32,
                y: f32::from(bounds.origin.y) as i32,
                width: f32::from(bounds.size.width) as u32,
                height: f32::from(bounds.size.height) as u32,
            }
        })
        .map_err(|_| GpuiHostError::WindowGone {
            window: window.to_string(),
        })
}

/// Performs one presentation change.
///
/// The changes gpui can make are real — minimise, maximise, fullscreen and focus all
/// exist — but two restores and a hide do not: there is no unmaximise, no unminimise and
/// no hide in gpui 0.2.2's platform contract. Those are answered as unsupported rather
/// than performed as nothing, because a caller told "done" for a thing that was not done
/// would act on a window that never changed.
pub(crate) fn change_state(
    cx: &mut gpui::AsyncApp,
    window: WindowId,
    change: ncui_webview::WindowChange,
    windows: &mut HashMap<WindowId, EngineWindow>,
) -> Result<(), GpuiHostError> {
    use ncui_webview::WindowChange;
    // An unsupported change is reported with the operation and the missing capability,
    // carried out of the closure because the closure's answer must be one type.
    let unsupported: Result<(), (&'static str, &'static str)> = match change {
        WindowChange::Maximized(false) => Err((
            "restore a maximised window",
            "gpui 0.2.2 can maximise but cannot restore: there is no unmaximise call",
        )),
        WindowChange::Minimized(false) => Err((
            "restore a minimised window",
            "gpui 0.2.2 can minimise but cannot restore: there is no unminimise call",
        )),
        WindowChange::Visible(false) => Err((
            "hide a window",
            "gpui 0.2.2 has no hide: a window it owns is on screen while it is open",
        )),
        _ => Ok(()),
    };
    if let Err((operation, reason)) = unsupported {
        return Err(GpuiHostError::UnsupportedOperation {
            operation: operation.to_owned(),
            reason: reason.to_owned(),
        });
    }
    let engine_window = tracked(window, windows)?;
    let handle = engine_window.handle;
    handle
        .update(cx, move |_, window, _| match change {
            WindowChange::Fullscreen(true) => {
                if !window.is_fullscreen() {
                    window.toggle_fullscreen();
                }
            }
            WindowChange::Fullscreen(false) => {
                if window.is_fullscreen() {
                    window.toggle_fullscreen();
                }
            }
            WindowChange::Maximized(true) => {
                window.zoom_window();
            }
            WindowChange::Minimized(true) => {
                window.minimize_window();
            }
            WindowChange::Visible(true) | WindowChange::Focus => {
                window.activate_window();
            }
            // The unsupported arms returned before the window was ever looked up.
            WindowChange::Maximized(false)
            | WindowChange::Minimized(false)
            | WindowChange::Visible(false) => {}
        })
        .map_err(|_| GpuiHostError::WindowGone {
            window: window.to_string(),
        })
}

/// Reads the platform's appearance for one window.
///
/// gpui reads the real setting — Windows' `UISettings` foreground colour — so this is
/// the user's actual light or dark choice, not a guess. `Vibrant` variants are the same
/// answer with more saturation, and they map onto their plain counterpart because the
/// runtime's `Theme` is a binary fact.
pub(crate) fn read_theme(
    cx: &mut gpui::AsyncApp,
    window: WindowId,
    windows: &mut HashMap<WindowId, EngineWindow>,
) -> Result<Theme, GpuiHostError> {
    let engine_window = tracked(window, windows)?;
    let handle = engine_window.handle;
    handle
        .update(cx, |_, window, _| match window.appearance() {
            gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark => Theme::Dark,
            gpui::WindowAppearance::Light | gpui::WindowAppearance::VibrantLight => Theme::Light,
        })
        .map_err(|_| GpuiHostError::WindowGone {
            window: window.to_string(),
        })
}

/// Applies one window shape: title, size, or both.
///
/// The host vets the shape before the command is sent, so by the time the shape arrives
/// every field it carries is one gpui can perform — a title, a width or a height. Size is
/// a resize against the window's current bounds, because gpui's resize takes both edges
/// and a shape may name only one: the edge nobody asked to change is the edge the window
/// already has, which is the only honest reading of "leave it alone".
pub(crate) fn apply_shape(
    cx: &mut gpui::AsyncApp,
    window: WindowId,
    shape: &ncui_webview::WindowShape,
    windows: &mut HashMap<WindowId, EngineWindow>,
) -> Result<(), GpuiHostError> {
    let engine_window = tracked(window, windows)?;
    let handle = engine_window.handle;
    let title = shape.title.clone();
    let width = shape.width;
    let height = shape.height;
    handle
        .update(cx, move |_, window, _| {
            if let Some(title) = title {
                window.set_window_title(&title);
            }
            if width.is_some() || height.is_some() {
                let current = window.bounds();
                window.resize(gpui::size(
                    gpui::px(width.map_or_else(|| f32::from(current.size.width), |w| w as f32)),
                    gpui::px(height.map_or_else(|| f32::from(current.size.height), |h| h as f32)),
                ));
            }
        })
        .map_err(|_| GpuiHostError::WindowGone {
            window: window.to_string(),
        })
}

/// The machine's displays as the runtime reports them.
pub(crate) fn read_monitors(cx: &mut gpui::AsyncApp) -> Vec<MonitorRect> {
    cx.update(|cx| {
        cx.displays()
            .iter()
            .map(|display| {
                let bounds = display.bounds();
                MonitorRect {
                    x: f32::from(bounds.origin.x) as i32,
                    y: f32::from(bounds.origin.y) as i32,
                    width: f32::from(bounds.size.width) as u32,
                    height: f32::from(bounds.size.height) as u32,
                }
            })
            .collect()
    })
    .unwrap_or_default()
}
