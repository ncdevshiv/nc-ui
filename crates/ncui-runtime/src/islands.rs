//! The GPU island surface: a frame a page asks for and composites itself.
//!
//! ADR-002 splits a GPU island in two. The renderer draws a region on the GPU; the page owns the
//! pixels' place in the document. This module is the join between them, and the mechanism is the
//! application's own asset server: the runtime registers a route under `/__ncui/island`, and a page
//! fetches it exactly as it fetches its own files. The page then wraps the bytes in an `ImageData`
//! and draws them into a `canvas`, and from there the browser composites the island with the rest of
//! the document — z-order, clipping, scrolling and opacity become the DOM's business rather than
//! something this runtime has to reimplement.
//!
//! # Why a route on the page's own origin
//!
//! A custom URL scheme would work on Windows and not on Linux, and a second server would be a second
//! thing to trust. The asset server is already the page's origin on every platform, so the page
//! needs no per-platform URL and no new authority: the request it makes is an ordinary same-origin
//! GET.
//!
//! # What a page may ask for
//!
//! One island, drawn as one rectangle or as a list of rows, at a size and in a colour the request
//! names — or, with `a11y=1` and a `label`, the same island's *accessible description*: its label and
//! each visible row's text and place, as JSON. A canvas tells a screen reader nothing, and the page
//! owns the DOM, so the page has to mirror the island's content into it; what the runtime owes the
//! page is the content in a form it can use. Every part of that is bounded — the area by
//! [`ncui_gpu_renderer::island::MAX_ISLAND_PIXELS`], the coordinates by the same reader a window
//! request uses, the rows by count and height, the text by length and size — because the request is
//! a page's, and a page's request is input. Nothing here reaches the filesystem beyond the font the
//! machine already has, spawns anything, or reads the environment: the answer is computed from the
//! numbers, the colours and the strings the request carries.
//!
//! # Where the text is drawn
//!
//! A row may carry text, and the text is drawn: the row's rectangle is drawn by the GPU with the
//! rest of the frame, and the glyphs are shaped with the machine's own font, rasterised into
//! coverage, and composited over those pixels. The rasterisation is on the CPU rather than in a
//! shader because a glyph is a set of contours with holes, and filling one correctly on the GPU
//! needs either a stencil pass (which draws hard edges) or an analytic coverage pass; the pixels
//! cross to the CPU anyway for the readback, so the compositing is where the frame already is.
//! `docs/known-gaps.md` states that boundary rather than leaving it to be discovered.
//!
//! # The cost, stated
//!
//! The pixels cross from the GPU to the CPU and then to the page, so a 640×480 island costs about a
//! megabyte per frame. `wry`'s public API has no shared-texture seam, so a zero-copy path is not
//! expressible through the port this runtime draws through; `docs/known-gaps.md` records that. What
//! this delivers is the part that was missing: the pixels are produced on the GPU and the DOM
//! composites them.

#[cfg(feature = "gpu-islands")]
pub use enabled::{ISLAND_PREFIX, Islands};

#[cfg(feature = "gpu-islands")]
mod enabled {
    use std::future::Future;
    use std::sync::Mutex;
    use std::task::{Context, Poll};

    use ncui_app::{Responder, Route, Served};
    // The surface this module defines is also called `Islands`; the model's is the geometry.
    use ncui_gpu::geometry::{
        Island, Islands as IslandModel, LogicalPoint, LogicalSize, Rect, Viewport,
    };
    use ncui_gpu::input::{IslandHost, PointerEvent, PointerPhase, Routed, Row};
    use ncui_gpu_renderer::atlas::GlyphCache;
    use ncui_gpu_renderer::device::{DeviceFactory, WgpuDevice, WgpuDeviceFactory};
    use ncui_gpu_renderer::frame::{Frame, IslandRect};
    use ncui_gpu_renderer::glyphs::Coverage;
    use ncui_gpu_renderer::island::{IslandPixels, render_to_pixels};
    use ncui_gpu_renderer::surface::SurfaceSize;
    use ncui_gpu_renderer::text::{ShapedLine, default_font};

    /// The path prefix a page fetches an island's frame from.
    pub const ISLAND_PREFIX: &str = "/__ncui/island";

    /// The largest island a page may ask for, along one edge.
    ///
    /// A bound on each edge as well as on the area, because a request for a one-pixel-high island a
    /// million pixels wide is a request for a line, and a renderer asked for it would spend its time
    /// on a shape nobody can see.
    const MAX_EDGE: u32 = 8_192;

    /// The GPU island surface.
    ///
    /// Holds the device rather than creating one per request: bringing up a `wgpu` device is the
    /// expensive part of drawing anything, and an island that made a device per frame would pay the
    /// whole cost of the GPU for every frame.
    #[derive(Debug, Default)]
    pub struct Islands {
        /// The device, created on the first request that needs one.
        ///
        /// A `Mutex` for two reasons: it is what makes a burst of requests draw one at a time rather
        /// than each allocating its own frame, and it is what lets the device be created lazily —
        /// an application that never uses an island never brings up a GPU.
        device: Mutex<Option<WgpuDevice>>,
        /// The font text is drawn with and the glyphs already rasterised from it.
        ///
        /// Held rather than rebuilt per request for two reasons. The font is looked for once: a
        /// machine with no font this runtime can read does not grow one while the application runs,
        /// and asking again on every frame would be a file-system search per request for an answer
        /// that has not changed. And the coverage of a glyph does not depend on the frame, so a page
        /// that redraws the same text — which is what a page does — is not made to rasterise it
        /// again: without the cache every frame pays for every glyph to produce the same pixels.
        glyphs: Mutex<Option<GlyphCache>>,
    }

    impl Islands {
        /// An island surface with no device yet.
        #[must_use]
        pub fn new() -> Self {
            Self::default()
        }

        /// The route a page fetches a frame from.
        ///
        /// The surface is shared with the responder, so the device it creates outlives the request
        /// that created it.
        #[must_use]
        pub fn route(self) -> Route {
            let surface = std::sync::Arc::new(self);
            let responder: Responder =
                std::sync::Arc::new(move |target: &str| surface.answer(target));
            Route::new(ISLAND_PREFIX, responder)
        }

        /// The body for `target`, or `None` when the request is not one this can answer.
        ///
        /// `None` becomes a 404 from the file server, which is the honest answer for a request whose
        /// parameters are not a frame: there is no file there, and there is no island either.
        fn answer(&self, target: &str) -> Option<Served> {
            let request = IslandRequest::parse(target)?;
            if request.a11y {
                return Some(Served {
                    content_type: DESCRIPTION_CONTENT_TYPE,
                    body: request.description()?,
                });
            }
            let pixels = self.render(&request)?;
            Some(Served {
                content_type: IslandPixels::CONTENT_TYPE,
                body: pixels.rgba,
            })
        }

        /// Draws one frame for `request`, or `None` when this machine cannot.
        ///
        /// A machine with no graphics adapter is not an error the page can do anything about, so it
        /// is reported as "no such route" rather than as a failure — the same choice the GPU tests
        /// make when they skip.
        fn render(&self, request: &IslandRequest) -> Option<IslandPixels> {
            let mut guard = self
                .device
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if guard.is_none() {
                let created = block_on(WgpuDeviceFactory.create()).ok()?;
                *guard = Some(created);
            }
            let device = guard.as_ref()?;
            let size = SurfaceSize::new(request.width, request.height);
            let frame = Frame {
                // Transparent, so the parts of the frame the island does not cover show the page
                // through them. A black clear would paste a rectangle over the document.
                clear_colour: [0, 0, 0, 0],
                islands: request.rectangles(),
            };
            let pixels = render_to_pixels(device, size, &frame).ok()?;
            self.draw_text(pixels, request)
        }

        /// Draws the text each visible row carries into the frame the GPU produced.
        ///
        /// `None` when a row asked for text and it cannot be drawn: a frame with the row's rectangle
        /// but not its text is a picture of something the page did not ask for, and the route's
        /// answer to a request it cannot honour is that there is nothing there.
        fn draw_text(
            &self,
            mut pixels: IslandPixels,
            request: &IslandRequest,
        ) -> Option<IslandPixels> {
            if !request.rows.iter().any(|row| row.text.is_some()) {
                return Some(pixels);
            }
            // The drawing happens with the cache held, because the cache owns the font: the two are
            // one thing — a font and the glyphs already rasterised from it — and handing out the font
            // separately would let a caller rasterise around the cache without noticing.
            let drawn = self.with_glyphs(|cache| draw_rows(cache, &mut pixels, request));
            match drawn {
                Some(Ok(())) => Some(pixels),
                Some(Err(reason)) => {
                    tracing::warn!(%reason, "an island's text could not be drawn");
                    None
                }
                None => {
                    tracing::warn!(
                        "an island row asked for text, and this machine has no font this runtime can read"
                    );
                    None
                }
            }
        }

        /// Runs `draw` with the glyph cache, building it on the first row that needs text.
        ///
        /// `None` when this machine has no font this runtime can read.
        fn with_glyphs<T>(&self, draw: impl FnOnce(&GlyphCache) -> T) -> Option<T> {
            let mut guard = self
                .glyphs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if guard.is_none() {
                *guard = default_font().ok().map(GlyphCache::new);
            }
            guard.as_ref().map(draw)
        }
    }

    /// Draws every visible row's text over the frame, through the glyph cache.
    ///
    /// # Errors
    ///
    /// Returns the reason a line could not be shaped, which is a font this runtime cannot read
    /// rather than a mistake in the request.
    fn draw_rows(
        cache: &GlyphCache,
        pixels: &mut IslandPixels,
        request: &IslandRequest,
    ) -> Result<(), String> {
        let font = cache.font();
        for (index, bounds) in request.visible_places(request.island_bounds()) {
            let Some(row) = request.rows.get(index) else {
                continue;
            };
            let Some(text) = row.text.as_ref() else {
                continue;
            };
            let lines = font
                .wrap(&text.text, text.size, bounds.width)
                .map_err(|error| format!("row {index}: {error}"))?;
            // The block of lines is centred in the row when it fits and top-aligned when it
            // does not, so a block taller than its row loses its last lines rather than its
            // first. A single line lands where it always did.
            let step = lines.first().map_or(0.0, ShapedLine::height);
            let block = lines.len() as f32 * step;
            let top = bounds.y + (row.height - block).max(0.0) / 2.0;
            let mut baseline = top + lines.first().map_or(0.0, |line| line.ascent);
            for line in &lines {
                let mut pen = bounds.x;
                for glyph in &line.glyphs {
                    if let Some(coverage) = cache.coverage(glyph.glyph_id, text.size) {
                        let left = pen + glyph.x_offset + coverage.origin_x;
                        // The outline's y axis points up and the frame's points down, so a glyph
                        // raised above the baseline is drawn at a smaller y.
                        let top = baseline - coverage.origin_y - glyph.y_offset;
                        blend(pixels, &coverage, left, top, text.colour, bounds);
                    }
                    pen += glyph.x_advance;
                }
                baseline += step;
            }
        }
        Ok(())
    }

    /// Draws one glyph's coverage over the frame's pixels, inside `clip`.
    ///
    /// Source-over compositing: the text's colour weighted by the coverage, over what the GPU drew.
    /// The clip is the row the text belongs to, so a glyph whose descent reaches past its row is
    /// cut off at the row's edge rather than drawn over the next one.
    fn blend(
        pixels: &mut IslandPixels,
        coverage: &Coverage,
        left: f32,
        top: f32,
        colour: [u8; 4],
        clip: Rect,
    ) {
        if !(left.is_finite() && top.is_finite()) {
            return;
        }
        // Whole pixels: the coverage was rasterised on the pixel grid, so placing it on a
        // fractional position would mean resampling it into a blur.
        let left = left.round();
        let top = top.round();
        let width = pixels.width as f32;
        let height = pixels.height as f32;
        for row in 0..coverage.height {
            let y = top + row as f32;
            if y < clip.y || y >= clip.y + clip.height || y < 0.0 || y >= height {
                continue;
            }
            for column in 0..coverage.width {
                let x = left + column as f32;
                if x < clip.x || x >= clip.x + clip.width || x < 0.0 || x >= width {
                    continue;
                }
                let alpha = coverage.at(column, row).unwrap_or(0);
                if alpha == 0 {
                    continue;
                }
                let source = (u32::from(alpha) * u32::from(colour[3]) + 127) / 255;
                if source == 0 {
                    continue;
                }
                let index = ((y as usize) * (width as usize) + (x as usize)) * 4;
                let Some(pixel) = pixels.rgba.get_mut(index..index + 4) else {
                    continue;
                };
                for channel in 0..3 {
                    let under = u32::from(pixel[channel]);
                    let over = u32::from(colour[channel]);
                    pixel[channel] =
                        u8::try_from((over * source + under * (255 - source) + 127) / 255)
                            .unwrap_or(255);
                }
                let under = u32::from(pixel[3]);
                pixel[3] = u8::try_from(source + under * (255 - source) / 255).unwrap_or(255);
            }
        }
    }

    /// One island a page asked for.
    #[derive(Debug, Clone, PartialEq)]
    struct IslandRequest {
        /// The frame's width in physical pixels.
        width: u32,
        /// The frame's height in physical pixels.
        height: u32,
        /// Where the island's left edge sits in the frame.
        x: u32,
        /// Where the island's top edge sits in the frame.
        y: u32,
        /// The island's width.
        island_width: u32,
        /// The island's height.
        island_height: u32,
        /// The island's colour, when it is one flat rectangle.
        colour: Option<[u8; 4]>,
        /// The island's rows, when its content is a list.
        rows: Vec<RowSpec>,
        /// How far the rows are scrolled, in pixels.
        scroll: u32,
        /// Whether the page asked for the island's accessible description instead of its pixels.
        a11y: bool,
        /// The label a screen reader announces for the island as a whole.
        label: Option<String>,
    }

    impl IslandRequest {
        /// Reads a request off a target, or `None` when it is not one.
        ///
        /// Every field is required and every field is bounded: a request that leaves one out is a
        /// request for a frame nobody described, and a request past a bound is refused rather than
        /// clamped, because a clamped island is an island drawn somewhere the page did not ask for.
        fn parse(target: &str) -> Option<Self> {
            let query = target.split_once('?')?.1;
            let mut request = Self {
                width: 0,
                height: 0,
                x: 0,
                y: 0,
                island_width: 0,
                island_height: 0,
                colour: None,
                rows: Vec::new(),
                scroll: 0,
                a11y: false,
                label: None,
            };
            for pair in query.split('&') {
                let (key, value) = pair.split_once('=')?;
                match key {
                    "w" => request.width = bounded(value)?,
                    "h" => request.height = bounded(value)?,
                    "x" => request.x = bounded(value)?,
                    "y" => request.y = bounded(value)?,
                    "iw" => request.island_width = bounded(value)?,
                    "ih" => request.island_height = bounded(value)?,
                    "colour" => request.colour = Some(colour(value)?),
                    "rows" => request.rows = rows(value)?,
                    "scroll" => request.scroll = bounded(value)?,
                    // A flag with no value, or `a11y=1`: both mean "the description, please".
                    "a11y" => {
                        request.a11y = value == "1" || value.is_empty();
                        if !request.a11y {
                            return None;
                        }
                    }
                    "label" => {
                        let decoded = percent_decode(value)?;
                        if decoded.len() > MAX_LABEL_BYTES {
                            return None;
                        }
                        request.label = Some(decoded);
                    }
                    _ => return None,
                }
            }
            // An island's content is one thing or the other. A request that named both would be a
            // request for whichever the renderer happened to draw last, and one that named neither
            // describes no content at all.
            let has_colour = request.colour.is_some();
            let has_rows = !request.rows.is_empty();
            if has_colour == has_rows {
                return None;
            }
            if request.scroll != 0 && request.rows.is_empty() {
                return None;
            }
            // A description with no label announces nothing, so it is not a description: the request
            // is refused rather than answered with something a reader cannot use. The label is the
            // one thing the page cannot infer — the runtime knows the rows, not what they are for.
            if request.a11y {
                if request
                    .label
                    .as_deref()
                    .is_none_or(|label| label.trim().is_empty())
                {
                    return None;
                }
            } else if request.label.is_some() {
                return None;
            }
            if request.width == 0
                || request.height == 0
                || request.width > MAX_EDGE
                || request.height > MAX_EDGE
                || u64::from(request.width) * u64::from(request.height)
                    > u64::from(ncui_gpu_renderer::island::MAX_ISLAND_PIXELS)
            {
                return None;
            }
            if request.island_width == 0 || request.island_height == 0 {
                return None;
            }
            if request.x >= request.width || request.y >= request.height {
                return None;
            }
            Some(request)
        }

        /// The rectangles the GPU draws for this island.
        ///
        /// The rows are laid out by the island model itself — the same code that answers which row a
        /// pointer is in — so what a renderer draws and what the host hit-tests cannot disagree. A
        /// second implementation of the layout here would be a second answer to that question, and
        /// the two would drift the first time either changed.
        /// The island's accessible description, as JSON.
        ///
        /// The same rows the renderer draws, from the same model, so what a screen reader is told
        /// cannot drift from what the page shows. `None` when the island cannot produce a
        /// description this runtime would call reachable — which is the same refusal the accessibility
        /// crate makes, applied here so a page cannot mirror something nobody can use.
        fn description(&self) -> Option<Vec<u8>> {
            let label = self.label.clone()?;
            let island_bounds = self.island_bounds();
            let places = self.visible_places(island_bounds);
            let mut rows = Vec::with_capacity(places.len());
            let mut nodes = Vec::with_capacity(places.len() + 1);
            let mut list = ncui_a11y::Node::with_children(ncui_a11y::Role::List, &label, 1, false);
            for (order, (index, bounds)) in places.iter().enumerate() {
                let text = self
                    .rows
                    .get(*index)
                    .and_then(|row| row.text.as_ref())
                    .map_or("", |text| text.text.as_str());
                let order = u32::try_from(order).unwrap_or(u32::MAX);
                list = list.child(ncui_a11y::Node::leaf(
                    ncui_a11y::Role::ListItem,
                    text,
                    order + 2,
                    false,
                ));
                rows.push(serde_json::json!({
                    "index": index,
                    "text": text,
                    "x": bounds.x,
                    "y": bounds.y,
                    "width": bounds.width,
                    "height": bounds.height,
                }));
            }
            nodes.push(list);
            let descriptor = ncui_a11y::IslandDescriptor {
                order: 0,
                label: label.clone(),
                nodes,
            };
            if !descriptor.is_reachable() {
                tracing::warn!(
                    label = %label,
                    "an island asked for its accessible description and cannot produce a reachable one"
                );
                return None;
            }
            let description = serde_json::json!({
                "label": label,
                "order": descriptor.order,
                "rows": rows,
            });
            serde_json::to_vec(&description).ok()
        }

        /// Where the island sits in the frame.
        fn island_bounds(&self) -> Rect {
            Rect::new(
                self.x as f32,
                self.y as f32,
                self.island_width as f32,
                self.island_height as f32,
            )
        }

        fn rectangles(&self) -> Vec<IslandRect> {
            let island_bounds = self.island_bounds();
            let Some(colour) = self.colour else {
                return self.row_rectangles(island_bounds);
            };
            vec![IslandRect {
                x: island_bounds.x,
                y: island_bounds.y,
                width: island_bounds.width,
                height: island_bounds.height,
                colour,
            }]
        }

        /// The rectangles for an island whose content is rows.
        fn row_rectangles(&self, island_bounds: Rect) -> Vec<IslandRect> {
            self.visible_places(island_bounds)
                .into_iter()
                .filter_map(|(index, bounds)| {
                    // A place names the row it is, so its colour is that row's colour.
                    let colour = self.rows.get(index)?.colour;
                    Some(IslandRect {
                        x: bounds.x,
                        y: bounds.y,
                        width: bounds.width,
                        height: bounds.height,
                        colour,
                    })
                })
                .collect()
        }

        /// The rows the viewport reaches, each with the index of the row it is.
        ///
        /// The geometry comes from the island model — the same code that answers which row a pointer
        /// is in — so what a renderer draws and what the host hit-tests cannot disagree.
        fn visible_places(&self, island_bounds: Rect) -> Vec<(usize, Rect)> {
            let viewport =
                Viewport::new(LogicalSize::new(self.width as f32, self.height as f32), 1.0);
            let mut islands = IslandModel::new(viewport);
            let content_height: f32 = self.rows.iter().map(|row| row.height).sum();
            if islands
                .add(
                    Island::new("island", island_bounds)
                        .with_content(LogicalSize::new(island_bounds.width, content_height)),
                )
                .is_err()
            {
                return Vec::new();
            }
            let rows: Vec<Row> = self
                .rows
                .iter()
                .enumerate()
                .map(|(index, row)| {
                    let text = row.text.as_ref().map_or("", |text| text.text.as_str());
                    Row::new(&format!("row-{index}"), text, row.height)
                })
                .collect();
            let mut host = IslandHost::new(islands, rows);
            let scrolled = host.scroll(
                &PointerEvent::new(island_bounds.center(), PointerPhase::Scroll)
                    .scrolling(LogicalPoint::new(0.0, self.scroll as f32)),
            );
            if matches!(scrolled, Routed::Dom) {
                return Vec::new();
            }
            host.visible_rows(0)
                .into_iter()
                .map(|place| (place.index, place.bounds))
                .collect()
        }
    }

    /// One row a page described: how tall it is, what colour it is drawn in, and the text on it.
    #[derive(Debug, Clone, PartialEq)]
    struct RowSpec {
        /// The row's height in pixels.
        height: f32,
        /// The row's colour.
        colour: [u8; 4],
        /// The text drawn on the row, when it carries any.
        text: Option<RowText>,
    }

    /// The text of one row: what it says, and how it is drawn.
    #[derive(Debug, Clone, PartialEq)]
    struct RowText {
        /// The text itself, already decoded.
        text: String,
        /// The text's colour.
        colour: [u8; 4],
        /// The text's size, in pixels.
        size: f32,
    }

    /// The most rows one request may describe.
    ///
    /// A bound because every row becomes geometry: a request for a million rows is a request for a
    /// million rectangles, which is a frame nobody asked for and a page cannot use.
    const MAX_ROWS: usize = 512;

    /// The tallest one row may be, in pixels.
    const MAX_ROW_HEIGHT: u32 = 4_096;

    /// The largest text one row may carry, in bytes.
    ///
    /// A bound on the string as well as on the rows: text is shaped and rasterised per glyph, and a
    /// row that carried a megabyte of it would spend the frame's whole budget on one line.
    const MAX_ROW_TEXT_BYTES: usize = 1_024;

    /// The largest text size, in pixels.
    const MAX_TEXT_SIZE: u32 = 256;

    /// The longest label an island's description may carry, in bytes.
    ///
    /// A label is announced, not read: one longer than a sentence is a description that should have
    /// been the rows.
    const MAX_LABEL_BYTES: usize = 256;

    /// What an island's accessible description is served as.
    const DESCRIPTION_CONTENT_TYPE: &str = "application/json";

    /// Reads a row list, separated by commas.
    ///
    /// Two shapes: `height:colour` is a plain row, and `height:colour:textcolour:size:text` is a row
    /// with text on it. The text is percent-encoded, because a comma separates rows and a colon
    /// separates fields, so neither can appear in it raw.
    fn rows(value: &str) -> Option<Vec<RowSpec>> {
        let mut rows = Vec::new();
        for entry in value.split(',') {
            if rows.len() >= MAX_ROWS {
                return None;
            }
            let fields: Vec<&str> = entry.splitn(5, ':').collect();
            let height: u32 = fields.first()?.parse().ok()?;
            if height == 0 || height > MAX_ROW_HEIGHT {
                return None;
            }
            let background = colour(fields.get(1)?)?;
            let text = match fields.len() {
                2 => None,
                5 => {
                    let size: u32 = fields.get(3)?.parse().ok()?;
                    if size == 0 || size > MAX_TEXT_SIZE {
                        return None;
                    }
                    let decoded = percent_decode(fields.get(4)?)?;
                    if decoded.len() > MAX_ROW_TEXT_BYTES {
                        return None;
                    }
                    (!decoded.is_empty()).then(|| RowText {
                        text: decoded,
                        colour: colour(fields.get(2).copied().unwrap_or("ffffffff"))
                            .unwrap_or([255, 255, 255, 255]),
                        size: size as f32,
                    })
                }
                _ => return None,
            };
            rows.push(RowSpec {
                height: height as f32,
                colour: background,
                text,
            });
        }
        (!rows.is_empty()).then_some(rows)
    }

    /// Decodes the percent escapes a row's text arrives with.
    ///
    /// Only `%XX` is decoded: a `+` is a plus in a URL's query, not a space, and decoding it as one
    /// would change text the page sent. A malformed escape is refused rather than guessed at, for
    /// the same reason every other field of a request is.
    fn percent_decode(value: &str) -> Option<String> {
        let bytes = value.as_bytes();
        let mut decoded = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'%' {
                let pair = bytes.get(index + 1..index + 3)?;
                let hex = core::str::from_utf8(pair).ok()?;
                decoded.push(u8::from_str_radix(hex, 16).ok()?);
                index += 3;
            } else {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
        String::from_utf8(decoded).ok()
    }

    /// A whole number, bounded by what a frame may be.
    fn bounded(value: &str) -> Option<u32> {
        let number: u32 = value.parse().ok()?;
        (number <= MAX_EDGE).then_some(number)
    }

    /// Eight hexadecimal digits, as red, green, blue and alpha.
    fn colour(value: &str) -> Option<[u8; 4]> {
        if value.len() != 8 {
            return None;
        }
        let mut channels = [0u8; 4];
        for (index, channel) in channels.iter_mut().enumerate() {
            *channel = u8::from_str_radix(value.get(index * 2..index * 2 + 2)?, 16).ok()?;
        }
        Some(channels)
    }

    /// Drives a future to completion on the current thread.
    ///
    /// The responder runs on a connection thread, which is not the thread `wgpu`'s futures expect to
    /// be polled on, so the future is polled here rather than handed to an executor this runtime does
    /// not have.
    fn block_on<F: Future>(future: F) -> F::Output {
        let waker = std::task::Waker::noop();
        let mut context = Context::from_waker(waker);
        let mut future = Box::pin(future);
        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(value) => return value,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }
}
