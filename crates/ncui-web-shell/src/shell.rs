//! The wasm32 shell implementation. Everything in here is browser platform
//! work: canvas ownership, frame scheduling, device creation on the canvas,
//! and the render loop. Input/clipboard/text-services land in their own
//! modules as the ADR-021 contract items are implemented.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use wasm_bindgen::{JsCast, JsValue};

use ncui_gpu::geometry::{Islands, LogicalPoint, LogicalSize, Rect, Viewport};
use ncui_gpu::input::{IslandHost, Key, MouseButton, PointerEvent, PointerPhase, Row};
use ncui_gpu_renderer::render::wgpu_pass::QuadPipeline;
use ncui_gpu_renderer::{Frame, IslandRect, RenderPlan, SurfaceSize};

/// The shell's input truth: an `IslandHost` over the scene's islands, plus the
/// last routing decision as evidence. Counting events was the first step's
/// scaffolding; the host is what the input contract means — hit-testing,
/// focus, selection — and the gate reads its outcomes, not the counters.
struct InputState {
    host: IslandHost,
    pointer_events: u32,
    key_events: u32,
    downs: u32,
    last_x: f32,
    last_y: f32,
    last_route: String,
    focused: String,
}

/// The two scene islands as the host understands them: same rectangles the
/// renderer draws, because a hit-test that disagreed with the paint would be
/// lying about one of them.
fn build_host() -> IslandHost {
    let mut islands = Islands::new(Viewport::new(LogicalSize::new(320.0, 200.0), 1.0));
    islands
        .add(
            ncui_gpu::geometry::Island::new("red", Rect::new(20.0, 20.0, 100.0, 60.0))
                // Content taller than the viewport, so scrolling has somewhere
                // to go — a scrollable island is what the wheel contract means.
                .with_content(LogicalSize::new(100.0, 200.0)),
        )
        .expect("red island");
    islands
        .add(ncui_gpu::geometry::Island::new(
            "green",
            Rect::new(200.0, 100.0, 80.0, 60.0),
        ))
        .expect("green island");
    // The timeline the frame paints, in the hit-test model too: paint, input
    // routing and accessibility are one truth or they are three lies. Without
    // this island the waveform's pixels are visible but a drag across them
    // routes to the DOM — a picture of a timeline, not a timeline.
    islands
        .add(ncui_gpu::geometry::Island::new(
            "timeline",
            Rect::new(10.0, 165.0, 300.0, 30.0),
        ))
        .expect("timeline island");
    let rows = vec![
        Row::new("r1", "first", 20.0),
        Row::new("r2", "second", 20.0),
        Row::new("r3", "third", 20.0),
    ];
    IslandHost::new(islands, rows).with_display_list("timeline", timeline_list())
}

thread_local! {
    /// Whether the scene needs another frame: input, resize, or startup.
    /// A static scene draws nothing — the same rule the desktop renderer
    /// follows, per ADR-021.
    static DIRTY: std::cell::Cell<bool> = std::cell::Cell::new(true);
    /// How many frames have actually been presented — the idle-stopping
    /// evidence surface.
    static FRAMES: std::cell::Cell<u32> = std::cell::Cell::new(0);
    static INPUT: RefCell<InputState> = RefCell::new(InputState {
        host: build_host(),
        pointer_events: 0,
        key_events: 0,
        downs: 0,
        last_x: 0.0,
        last_y: 0.0,
        last_route: "none".to_owned(),
        focused: "none".to_owned(),
    });
}

/// A snapshot of the shell's input truth, as JSON — the evidence surface the
/// real-browser gate verifies synthesized input against.
#[wasm_bindgen]
pub fn input_state() -> String {
    INPUT.with(|i| {
        let i = i.borrow();
        let selection = i
            .host
            .state(0)
            .and_then(|s| s.selection)
            .map_or("none".to_owned(), |(a, b)| format!("{a}..{b}"));
        format!(
            "{{\"pointer_events\":{},\"key_events\":{},\"downs\":{},\"last_x\":{},\"last_y\":{},\"last_route\":\"{}\",\"focused\":\"{}\",\"red_selection\":\"{}\",\"red_scroll_y\":{},\"clipboard\":\"{}\",\"composition\":\"{}\",\"frames\":{}}}",
            i.pointer_events,
            i.key_events,
            i.downs,
            i.last_x as i32,
            i.last_y as i32,
            i.last_route,
            i.focused,
            selection,
            i.host
                .islands()
                .all()
                .first()
                .map_or(0.0, |isl| isl.scroll.y) as i32,
            i.host
                .state(0)
                .and_then(|st| st.clipboard.clone())
                .unwrap_or_default()
                .replace('\"', "'"),
            i.host
                .state(0)
                .and_then(|st| st.composition.clone())
                .unwrap_or_default(),
            FRAMES.with(|f| f.get()),
        )
    })
}

fn record_pointer(event: PointerEvent) {
    DIRTY.with(|d| d.set(true));
    INPUT.with(|i| {
        let mut i = i.borrow_mut();
        i.pointer_events += 1;
        i.last_x = event.position.x;
        i.last_y = event.position.y;
        let routed = match event.phase {
            PointerPhase::Down => {
                i.downs += 1;
                i.host.click(&event)
            }
            PointerPhase::Scroll => i.host.scroll(&event),
            // Moves and ups go through the capture path: a drag that leaves the island
            // mid-gesture is the normal shape of editing, not an edge case.
            _ => i.host.route_pointer_captured(&event),
        };
        i.last_route = route_tag(&routed);
        i.focused = i
            .host
            .focused()
            .map_or("none".to_owned(), |idx| idx.to_string());
    });
}

/// A route as evidence-safe text: variant and island id, no debug formatting —
/// the probe is JSON, and raw `Debug` output breaks it.
fn route_tag(routed: &ncui_gpu::input::Routed) -> String {
    use ncui_gpu::input::Routed;
    match routed {
        Routed::Dom => "dom".to_owned(),
        Routed::Host => "host".to_owned(),
        Routed::Ignored => "ignored".to_owned(),
        Routed::Island { id, .. } => format!("island:{id}"),
    }
}

fn record_key(key: Key) {
    DIRTY.with(|d| d.set(true));
    INPUT.with(|i| {
        let mut i = i.borrow_mut();
        i.key_events += 1;
        // Focus entry is the shell's responsibility, the same shape gpui apps
        // follow: the host only arbitrates Tab once an island holds focus, so
        // an unfocused host answering `Dom` means the shell moves focus in.
        // Once focused, the host owns the key — including Tab, which it marks
        // `Host` so an island can never trap the user.
        let routed = match (key, i.host.focused()) {
            (Key::Tab, None) => i.host.focus_next(),
            (Key::ShiftTab, None) => i.host.focus_previous(),
            _ => i.host.route_key(key),
        };
        i.last_route = route_tag(&routed);
        i.focused = i
            .host
            .focused()
            .map_or("none".to_owned(), |idx| idx.to_string());
    });
}

/// Translates a DOM key event into the input model's `Key`. `None` for keys
/// the model does not carry — an honest absence rather than a wrong mapping.
fn key_of(ev: &web_sys::KeyboardEvent) -> Option<Key> {
    let key = ev.key();
    let shift = ev.shift_key();
    Some(match key.as_str() {
        "Tab" if shift => Key::ShiftTab,
        "Tab" => Key::Tab,
        "Enter" => Key::Enter,
        " " => Key::Space,
        "Escape" => Key::Escape,
        "ArrowUp" => Key::ArrowUp,
        "ArrowDown" => Key::ArrowDown,
        "ArrowLeft" => Key::ArrowLeft,
        "ArrowRight" => Key::ArrowRight,
        "Backspace" => Key::Backspace,
        s if s.chars().count() == 1 => Key::Character(s.chars().next()?),
        _ => return None,
    })
}

/// Attach the shell to a canvas element: create the WebGPU device on it and
/// start the frame loop. This is the loader's one entry point —
/// `ncui_web_shell.attach(canvas)` from JavaScript.
///
/// The returned promise resolves once the device exists and the first frame
/// has been presented; it rejects with a readable error when WebGPU is
/// unavailable (old browser, disabled flag), because a shell that cannot
/// draw must say so rather than blank-screen.
#[wasm_bindgen]
pub async fn attach(canvas: web_sys::HtmlCanvasElement) -> Result<(), JsValue> {
    let mut shell = Shell::create(canvas).await?;
    shell.present_first_frame()?;
    shell.wire_input();
    wire_resize_observer(&shell.canvas);
    render_aria_mirror();
    shell.start_loop();
    Ok(())
}

/// Canvas backing-store sizing is the shell's platform work (ADR-021): CSS
/// pixels times devicePixelRatio, driven by ResizeObserver rather than
/// polling, so high-DPI is correct and a resize costs exactly one
/// reconfiguration. The render loop's reconfigure check picks the new backing
/// size up on the next frame.
fn wire_resize_observer(canvas: &web_sys::HtmlCanvasElement) {
    fn apply(canvas: &web_sys::HtmlCanvasElement) {
        DIRTY.with(|d| d.set(true));
        let dpr = web_sys::window().map_or(1.0, |w| w.device_pixel_ratio());
        let width = (f64::from(canvas.client_width()) * dpr).round().max(1.0) as u32;
        let height = (f64::from(canvas.client_height()) * dpr).round().max(1.0) as u32;
        if canvas.width() != width {
            canvas.set_width(width);
        }
        if canvas.height() != height {
            canvas.set_height(height);
        }
    }
    apply(canvas);
    let observed = canvas.clone();
    let callback: Closure<dyn FnMut(js_sys::Array, web_sys::ResizeObserver)> =
        Closure::new(move |_, _| apply(&observed));
    let observer =
        web_sys::ResizeObserver::new(callback.as_ref().unchecked_ref()).expect("ResizeObserver");
    observer.observe(canvas);
    callback.forget();
    // The observer must live for the page's lifetime, the way the shell does.
    std::mem::forget(observer);
}

/// The scene's semantic tree. The islands are regions with labels; the rows
/// are the focusable items inside them. The mirror is honest because it is
/// built from the same rows the host hit-tests.
/// The shell's custom island content — one function, because the renderer's scene and
/// the accessibility tree must describe the same timeline, and two constructions of the
/// same fixture are exactly the drift the display-list design exists to prevent.
fn timeline_list() -> ncui_gpu::display::DisplayList {
    ncui_gpu::display::DisplayList::builder()
            .rect(Rect::new(0.0, 0.0, 300.0, 30.0), [24, 24, 28, 255])
            .rect(Rect::new(20.0, 4.0, 120.0, 22.0), [50, 80, 180, 255])
            .clip_push(Rect::new(20.0, 4.0, 120.0, 22.0))
            .rect(Rect::new(24.0, 8.0, 3.0, 14.0), [140, 180, 255, 255])
            .rect(Rect::new(30.0, 8.0, 3.0, 14.0), [140, 180, 255, 255])
            .rect(Rect::new(60.0, 8.0, 3.0, 14.0), [140, 180, 255, 255])
            .rect(Rect::new(136.0, 8.0, 8.0, 14.0), [140, 180, 255, 255])
            .hit_region(
                "waveform",
                Rect::new(20.0, 4.0, 120.0, 22.0),
                "waveform intro.mp4",
                ncui_gpu::display::RegionRole::Surface,
            )
            // The clip's thumbnail well: the fixture gradient scaled into the leading
            // edge of the clip, so the gate can prove blits end-to-end in compositor
            // pixels.
            .blit(Rect::new(90.0, 6.0, 24.0, 13.5), "thumb:intro")
            .clip_pop()
            // The clip's label, drawn over the waveform: the pixel gate reads these
            // pixels back, and a text path that drew nothing — or a rectangle where the
            // letters should be — fails there rather than here.
            .text(
                LogicalPoint { x: 26.0, y: 19.0 },
                "intro.mp4",
                11.0,
                [235, 240, 255, 255],
            )
            .rect(Rect::new(80.0, 0.0, 2.0, 30.0), [255, 70, 70, 255])
            .hit_region(
                "playhead",
                Rect::new(76.0, 0.0, 10.0, 30.0),
                "playhead",
                ncui_gpu::display::RegionRole::Handle,
            )
            .finish()
            .expect("the timeline fixture is balanced")
}

fn scene_tree() -> ncui_a11y::SemanticTree {
    use ncui_a11y::{Node, Role, SemanticTree};
    let rows = || -> Vec<Node> {
        ["first", "second", "third"]
            .iter()
            .enumerate()
            .map(|(i, text)| Node::leaf(Role::Button, text, (i + 1) as u32, true))
            .collect()
    };
    let red = Node {
        role: Role::Region,
        name: "red island".to_owned(),
        order: 10,
        focusable: false,
        children: rows(),
    };
    let green = Node::leaf(Role::Region, "green island", 20, false);
    // The timeline island's controls come from the display list itself: the same
    // effective regions hit-testing resolves, because accessibility descriptions
    // derive from the one stream — a region clipped out of the paint is not
    // announced either. Focusable handles are what a keyboard can drag; surfaces
    // are announced, not stopped at.
    let timeline_children: Vec<Node> = timeline_list()
        .accessibility_descriptions()
        .iter()
        .enumerate()
        .map(|(index, region)| {
            use ncui_gpu::display::RegionRole;
            let (role, focusable) = match region.role {
                RegionRole::Button => (Role::Button, true),
                RegionRole::Handle => (Role::Slider, true),
                RegionRole::Track | RegionRole::Curve => (Role::Group, false),
                RegionRole::Ruler => (Role::Region, false),
                RegionRole::Surface => (Role::StaticText, false),
            };
            Node::leaf(role, &region.label, (30 + index) as u32, focusable)
        })
        .collect();
    let timeline = Node {
        role: Role::Region,
        name: "timeline".to_owned(),
        order: 25,
        focusable: false,
        children: timeline_children,
    };
    SemanticTree::new().root(red).root(green).root(timeline)
}

/// Mirrors the semantic tree into visually hidden ARIA DOM — the web's
/// accessibility primitive, carrying the same information the AccessKit
/// bridge publishes on Windows (ADR-021). The nodes are real DOM, so browser
/// assistive technology sees them in the accessibility tree; they are hidden
/// visually because the canvas is the visual truth.
fn render_aria_mirror() {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Some(document) = window.document() else {
        return;
    };
    let Some(body) = document.body() else { return };
    let Ok(container) = document.create_element("div") else {
        return;
    };
    container.set_id("ncui-aria");
    // Visually hidden but present to AT: clipped to zero rather than
    // display:none, which browsers drop from the accessibility tree.
    let _ = container.set_attribute(
        "style",
        "position:absolute;width:1px;height:1px;overflow:hidden;clip:rect(0 0 0 0);white-space:nowrap",
    );
    let _ = container.set_attribute("role", "application");
    let _ = container.set_attribute("aria-label", "ncui shell");

    fn emit(document: &web_sys::Document, parent: &web_sys::Element, node: &ncui_a11y::Node) {
        use ncui_a11y::Role;
        let role = match node.role {
            Role::Application => "application",
            Role::Window => "window",
            Role::Group => "group",
            Role::Region => "region",
            Role::StaticText => "text",
            Role::Button => "button",
            Role::Checkbox => "checkbox",
            Role::Radio => "radio",
            Role::List => "list",
            Role::ListItem => "listitem",
            Role::Slider => "slider",
            Role::TextInput => "textbox",
            Role::Link => "link",
            Role::ComboBox => "combobox",
            Role::SearchBox => "searchbox",
            Role::ProgressBar => "progressbar",
            Role::Alert => "alert",
            Role::Menu => "menu",
            Role::MenuItem => "menuitem",
            Role::Tab => "tab",
            Role::TabList => "tablist",
        };
        let Ok(el) = document.create_element("div") else {
            return;
        };
        let _ = el.set_attribute("role", role);
        if !node.name.is_empty() {
            let _ = el.set_attribute("aria-label", &node.name);
        }
        if node.focusable {
            let _ = el.set_attribute("tabindex", "-1");
        }
        for child in &node.children {
            emit(document, &el, child);
        }
        let _ = parent.append_child(&el);
    }

    let tree = scene_tree();
    for root in tree.roots() {
        emit(&document, &container, root);
    }
    let _ = body.append_child(&container);
}

fn logical(ev: &web_sys::MouseEvent, canvas: &web_sys::HtmlCanvasElement) -> LogicalPoint {
    // offsetX/Y are CSS pixels relative to the target — the input model's
    // window coordinates. devicePixelRatio scaling to physical happens where
    // physical matters (rendering), not in input.
    let _ = canvas;
    LogicalPoint::new(ev.offset_x() as f32, ev.offset_y() as f32)
}

fn button_of(ev: &web_sys::MouseEvent) -> MouseButton {
    match ev.button() {
        1 => MouseButton::Middle,
        2 => MouseButton::Secondary,
        _ => MouseButton::Primary,
    }
}

/// Fetches the font the shell renders text with.
///
/// The asset is `ncui-font.ttf` beside the page: the gate's build copies it there, and
/// the packaging step does the same for a release. What font that is matters — shaping
/// is only as honest as the bytes — so the asset name is fixed rather than negotiated.
async fn fetch_font_bytes() -> Result<Vec<u8>, JsValue> {
    let window = web_sys::window().ok_or_else(|| JsValue::from_str("no window"))?;
    let response = wasm_bindgen_futures::JsFuture::from(window.fetch_with_str("ncui-font.ttf"))
        .await
        .map_err(|e| JsValue::from_str(&format!("font fetch: {e:?}")))?;
    let response: web_sys::Response = response
        .dyn_into()
        .map_err(|_| JsValue::from_str("font fetch: not a response"))?;
    if !response.ok() {
        return Err(JsValue::from_str(&format!(
            "font fetch: status {}",
            response.status()
        )));
    }
    let buffer = wasm_bindgen_futures::JsFuture::from(
        response
            .array_buffer()
            .map_err(|e| JsValue::from_str(&format!("font body: {e:?}")))?,
    )
    .await
    .map_err(|e| JsValue::from_str(&format!("font body: {e:?}")))?;
    let bytes = js_sys::Uint8Array::new(&buffer);
    let mut out = vec![0u8; bytes.length() as usize];
    bytes.copy_to(&mut out[..]);
    Ok(out)
}

/// The live shell: device, surface and the frame loop's shared state.
struct Shell {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    canvas: web_sys::HtmlCanvasElement,
    pipeline: QuadPipeline,
    /// The font the scene's text is shaped and rasterised with, behind its cache.
    glyph_cache: ncui_gpu_renderer::atlas::GlyphCache,
    /// The CPU mirror of the glyph texture; the pipeline uploads its dirty regions.
    glyph_atlas: ncui_gpu_renderer::text_layout::ContentAtlas,
    /// The bitmaps the scene's blits resolve against.
    bitmaps: ncui_gpu_renderer::bitmap::BitmapStore,
}

impl Shell {
    async fn create(canvas: web_sys::HtmlCanvasElement) -> Result<Self, JsValue> {
        let mut instance_desc = wgpu::InstanceDescriptor::new_without_display_handle();
        instance_desc.backends = wgpu::Backends::BROWSER_WEBGPU;
        let instance = wgpu::Instance::new(instance_desc);
        let surface = instance
            .create_surface(wgpu::SurfaceTarget::Canvas(canvas.clone()))
            .map_err(|e| JsValue::from_str(&format!("surface creation failed: {e}")))?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .map_err(|e| JsValue::from_str(&format!("no WebGPU adapter: {e}")))?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default())
            .await
            .map_err(|e| JsValue::from_str(&format!("device request failed: {e}")))?;

        // Size the canvas honestly: CSS pixels times devicePixelRatio, so
        // high-DPI is the shell's problem and never the scene's (ADR-021).
        let width = canvas.width().max(1);
        let height = canvas.height().max(1);
        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .first()
            .copied()
            .unwrap_or(wgpu::TextureFormat::Bgra8UnormSrgb);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width,
            height,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps
                .alpha_modes
                .first()
                .copied()
                .unwrap_or(wgpu::CompositeAlphaMode::Auto),
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);
        let pipeline = QuadPipeline::new(&device, &queue, format);
        // The font is content the browser cannot supply: web fonts are licensed to the
        // page's own text, not readable as bytes. The shell fetches the font asset it
        // is packaged with, and a shell without it refuses to start — falling back to
        // no text would put a scene on the screen that is not the scene the producer
        // built, which is the same lie the renderer refuses in pixels.
        let font_bytes = fetch_font_bytes().await?;
        let font = ncui_gpu_renderer::text::Font::from_bytes(font_bytes, 0)
            .map_err(|e| JsValue::from_str(&format!("font parse: {e}")))?;
        let glyph_cache = ncui_gpu_renderer::atlas::GlyphCache::new(font);
        let glyph_atlas = ncui_gpu_renderer::text_layout::ContentAtlas::new();
        let mut bitmaps = ncui_gpu_renderer::bitmap::BitmapStore::new();
        // The scene's fixture thumbnail: a 16x9 frame-gradient, procedurally generated
        // because the scene is a fixture and its pixels must be deterministic — the
        // gate asserts against exactly this gradient.
        let mut thumb = Vec::with_capacity(16 * 9 * 4);
        for y in 0..9u8 {
            for x in 0..16u8 {
                thumb.extend_from_slice(&[x * 16, y * 28, 200, 255]);
            }
        }
        bitmaps.insert("thumb:intro", 16, 9, thumb);
        Ok(Self {
            device,
            queue,
            surface,
            config,
            canvas,
            pipeline,
            glyph_cache,
            glyph_atlas,
            bitmaps,
        })
    }

    /// The scene the shell draws. Deterministic on purpose: the gate verifies
    /// pixels, and a scene whose truth is known is the only kind a pixel gate
    /// can verify. Real scene production — islands owned by content — lands
    /// with the input contract, on this same path.
    fn scene(&self) -> Frame {
        // The custom island: a miniature timeline, in the shape the display-list and
        // render-plan tests already pin — a track, a clip, waveform bars clipped to
        // the clip's edge, a playhead. The gate reads its pixels, so the bar that
        // extends past the clip is the clipping proof: paint past the edge would be a
        // lie the pixel gate catches.
        let timeline = timeline_list();
        Frame {
            custom_islands: vec![ncui_gpu_renderer::frame::CustomIsland {
                x: 10.0,
                y: 165.0,
                width: 300.0,
                height: 30.0,
                scroll_x: 0.0,
                scroll_y: 0.0,
                list: timeline,
            }],
            clear_colour: [28, 41, 82, 255],
            islands: vec![
                IslandRect {
                    x: 20.0,
                    y: 20.0,
                    width: 100.0,
                    height: 60.0,
                    colour: [220, 40, 40, 255],
                },
                IslandRect {
                    x: 200.0,
                    y: 100.0,
                    width: 80.0,
                    height: 60.0,
                    colour: [40, 200, 40, 255],
                },
            ],
        }
    }

    /// Render and present one frame — currently the pipeline proof: a solid
    /// clear. Scene rendering lands on the same path; a clear frame already
    /// exercises instance, adapter, device, surface configuration, render
    /// pass, submission and presentation — every stage that can fail in a
    /// browser, which is why it is the honest first milestone.
    fn present_first_frame(&mut self) -> Result<(), JsValue> {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            other => return Err(JsValue::from_str(&format!("acquire frame: {other:?}"))),
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let frame_scene = self.scene();
        let size = SurfaceSize {
            width: self.config.width,
            height: self.config.height,
        };
        let plan = RenderPlan::from_frame_content(
            &frame_scene,
            size,
            &self.glyph_cache,
            &mut self.glyph_atlas,
            Some(&self.bitmaps),
        )
        .map_err(|e| JsValue::from_str(&format!("render plan: {e}")))?;
        self.pipeline.sync_atlas(&self.queue, &mut self.glyph_atlas);
        self.pipeline.draw(&self.device, &self.queue, &view, &plan);
        frame.present();
        Ok(())
    }

    /// DOM events enter the input model here — the same model the native host
    /// feeds, per ADR-021. Listeners live for the page's lifetime, the way the
    /// shell itself does.
    fn wire_input(&self) {
        let canvas = self.canvas.clone();
        let on = |target: &web_sys::HtmlCanvasElement,
                  kind: &str,
                  f: Closure<dyn FnMut(web_sys::MouseEvent)>| {
            target
                .add_event_listener_with_callback(kind, f.as_ref().unchecked_ref())
                .expect("add event listener");
            f.forget();
        };
        let c = canvas.clone();
        on(
            &canvas,
            "pointermove",
            Closure::new(move |ev: web_sys::MouseEvent| {
                record_pointer(
                    PointerEvent::new(logical(&ev, &c), PointerPhase::Move).with_modifiers(
                        ev.shift_key(),
                        ev.ctrl_key() || ev.meta_key(),
                        ev.alt_key(),
                    ),
                );
            }),
        );
        let c = canvas.clone();
        on(
            &canvas,
            "pointerdown",
            Closure::new(move |ev: web_sys::MouseEvent| {
                record_pointer(
                    PointerEvent::new(logical(&ev, &c), PointerPhase::Down)
                        .with_button(button_of(&ev))
                        .with_modifiers(
                            ev.shift_key(),
                            ev.ctrl_key() || ev.meta_key(),
                            ev.alt_key(),
                        ),
                );
            }),
        );
        let c = canvas.clone();
        on(
            &canvas,
            "pointerup",
            Closure::new(move |ev: web_sys::MouseEvent| {
                record_pointer(
                    PointerEvent::new(logical(&ev, &c), PointerPhase::Up)
                        .with_button(button_of(&ev))
                        .with_modifiers(
                            ev.shift_key(),
                            ev.ctrl_key() || ev.meta_key(),
                            ev.alt_key(),
                        ),
                );
            }),
        );
        // Wheel events are scroll-phase pointer events in the input model;
        // deltaY is the user's intent in CSS pixels, negated because content
        // follows the finger on the web and the model scrolls content-space.
        let wheel: Closure<dyn FnMut(web_sys::WheelEvent)> =
            Closure::new(move |ev: web_sys::WheelEvent| {
                ev.prevent_default();
                let point = LogicalPoint::new(ev.offset_x() as f32, ev.offset_y() as f32);
                record_pointer(
                    PointerEvent::new(point, PointerPhase::Scroll)
                        .scrolling(LogicalPoint::new(ev.delta_x() as f32, ev.delta_y() as f32))
                        .with_modifiers(
                            ev.shift_key(),
                            ev.ctrl_key() || ev.meta_key(),
                            ev.alt_key(),
                        ),
                );
            });
        canvas
            .add_event_listener_with_callback("wheel", wheel.as_ref().unchecked_ref())
            .expect("add wheel listener");
        wheel.forget();
        // The IME shim: browser composition events only fire on editable
        // elements, so a visually hidden textarea is the composition surface —
        // the web's platform reality the way WM_IME is Windows'. Composition
        // text lands in the same host state machine the native fields use.
        if let Some(document) = web_sys::window().and_then(|w| w.document()) {
            if let Ok(shim) = document.create_element("textarea") {
                let _ = shim.set_attribute("id", "ncui-ime-shim");
                let _ = shim.set_attribute("aria-hidden", "true");
                let _ = shim.set_attribute(
                    "style",
                    "position:absolute;top:0;left:0;width:1px;height:1px;opacity:0;border:none;padding:0",
                );
                if let Some(body) = document.body() {
                    let _ = body.append_child(&shim);
                }
                let update: Closure<dyn FnMut(web_sys::CompositionEvent)> =
                    Closure::new(move |ev: web_sys::CompositionEvent| {
                        let text = ev.data().unwrap_or_default();
                        INPUT.with(|i| {
                            // Composition replace semantics: the browser's
                            // update carries the whole in-progress string, so
                            // the host's state is set, not appended.
                            let _ = i.borrow_mut().host.set_composition(Some(text.clone()));
                        });
                    });
                let _ = shim.add_event_listener_with_callback(
                    "compositionupdate",
                    update.as_ref().unchecked_ref(),
                );
                update.forget();
                let end: Closure<dyn FnMut(web_sys::CompositionEvent)> =
                    Closure::new(move |_ev: web_sys::CompositionEvent| {
                        record_key(Key::CompositionCommit);
                    });
                let _ = shim.add_event_listener_with_callback(
                    "compositionend",
                    end.as_ref().unchecked_ref(),
                );
                end.forget();
            }
        }
        // Drag and drop: browser drag events carry a DataTransfer; text drops
        // land in the host's drop path, which is also where the island's
        // clipboard receives them — one semantic, two platforms.
        let dragover: Closure<dyn FnMut(web_sys::DragEvent)> =
            Closure::new(move |ev: web_sys::DragEvent| {
                ev.prevent_default();
            });
        canvas
            .add_event_listener_with_callback("dragover", dragover.as_ref().unchecked_ref())
            .expect("add dragover listener");
        dragover.forget();
        let drop: Closure<dyn FnMut(web_sys::DragEvent)> =
            Closure::new(move |ev: web_sys::DragEvent| {
                ev.prevent_default();
                let Some(text) = ev
                    .data_transfer()
                    .and_then(|dt| dt.get_data("text/plain").ok())
                else {
                    return;
                };
                if text.is_empty() {
                    return;
                }
                DIRTY.with(|d| d.set(true));
                let event = ncui_gpu::input::DragEvent {
                    payload: ncui_gpu::input::DragPayload::Text(text),
                    position: LogicalPoint::new(ev.offset_x() as f32, ev.offset_y() as f32),
                    dropping: true,
                };
                INPUT.with(|i| {
                    let _ = i.borrow_mut().host.drop_text(&event);
                });
            });
        canvas
            .add_event_listener_with_callback("drop", drop.as_ref().unchecked_ref())
            .expect("add drop listener");
        drop.forget();
        let window = web_sys::window().expect("a window");
        let keys: Closure<dyn FnMut(web_sys::KeyboardEvent)> =
            Closure::new(move |ev: web_sys::KeyboardEvent| {
                // Clipboard is the shell's platform work: the host owns the
                // selection model, the browser owns the clipboard, and the two
                // meet here — capability-gated by the browser's own permission
                // model, which the ADR accepts as the web's platform reality.
                if ev.ctrl_key() || ev.meta_key() {
                    match ev.key().as_str() {
                        "c" => {
                            ev.prevent_default();
                            let copied = INPUT.with(|i| i.borrow_mut().host.copy());
                            if let Some(text) = copied {
                                if let Some(window) = web_sys::window() {
                                    let clipboard = window.navigator().clipboard();
                                    let _ = clipboard.write_text(&text);
                                }
                            }
                            return;
                        }
                        "v" => {
                            ev.prevent_default();
                            if let Some(window) = web_sys::window() {
                                let clipboard = window.navigator().clipboard();
                                wasm_bindgen_futures::spawn_local(async move {
                                    let promise = clipboard.read_text();
                                    if let Ok(value) =
                                        wasm_bindgen_futures::JsFuture::from(promise).await
                                    {
                                        if let Some(text) = value.as_string() {
                                            INPUT.with(|i| {
                                                let _ = i.borrow_mut().host.paste(&text);
                                            });
                                        }
                                    }
                                });
                            }
                            return;
                        }
                        _ => {}
                    }
                }
                if let Some(key) = key_of(&ev) {
                    record_key(key);
                }
            });
        window
            .add_event_listener_with_callback("keydown", keys.as_ref().unchecked_ref())
            .expect("add key listener");
        keys.forget();
    }

    /// The frame loop: requestAnimationFrame-driven, repainting each frame
    /// for now. Idle-stopping arrives with scene dirty-tracking — a static
    /// scene must not redraw forever, and that rule lands with the scene.
    fn start_loop(self) {
        let shell = Rc::new(RefCell::new(self));
        fn request_frame(f: &Closure<dyn FnMut()>) {
            let window = web_sys::window().expect("a window");
            window
                .request_animation_frame(f.as_ref().unchecked_ref())
                .expect("requestAnimationFrame");
        }
        let step: Rc<RefCell<Option<Closure<dyn FnMut()>>>> = Rc::new(RefCell::new(None));
        let step2 = step.clone();
        let shell2 = shell.clone();
        *step2.borrow_mut() = Some(Closure::new(move || {
            // Reconfigure if the canvas backing size changed — ResizeObserver
            // granularity is a refinement; the correctness rule is that the
            // surface always matches the canvas.
            {
                // Read-then-write in two borrows: an immutable borrow still live
                // across `borrow_mut` is a panic in wasm — which the gate's
                // resize probe correctly turned into an unreachable trap.
                let (w, h, needs) = {
                    let sh = shell2.borrow();
                    (
                        sh.canvas.width().max(1),
                        sh.canvas.height().max(1),
                        sh.canvas.width().max(1) != sh.config.width
                            || sh.canvas.height().max(1) != sh.config.height,
                    )
                };
                if needs {
                    let mut shmut = shell2.borrow_mut();
                    shmut.config.width = w;
                    shmut.config.height = h;
                    shmut.surface.configure(&shmut.device, &shmut.config);
                }
            }
            let dirty = DIRTY.with(|d| {
                let was = d.get();
                d.set(false);
                was
            });
            if dirty {
                if shell2.borrow_mut().present_first_frame().is_ok() {
                    FRAMES.with(|f| f.set(f.get() + 1));
                }
            }
            if let Some(f) = step.borrow().as_ref() {
                request_frame(f);
            }
        }));
        if let Some(f) = step2.borrow().as_ref() {
            request_frame(f);
        }
        // The closure must live for the page's lifetime: the shell is the
        // application, and the application does not exit.
        std::mem::forget(step2);
    }
}
