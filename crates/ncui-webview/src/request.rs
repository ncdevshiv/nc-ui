//! What a window is asked to be: where it goes, what it is called, and how big it is.
//!
//! The engine used to be handed a bare URL and hard-code the rest of the window's shape —
//! every host invented its own title and size, and an application had no way to ask for a
//! window that looked like anything in particular. The request is the answer to that: one
//! value that describes the window, carried from whoever asked for it to the engine that
//! creates it.
//!
//! It travels through the port rather than around it. [`crate::HostFactory::spawn`] takes the
//! request, so a host cannot create a window without being told what the window is, and the
//! registry cannot record a title the engine never used.

use core::fmt;

use serde::{Deserialize, Serialize};

/// The title a window carries when the application does not name one.
pub const DEFAULT_WINDOW_TITLE: &str = "NC-UI";

/// The width a window carries when the application does not name one.
pub const DEFAULT_WINDOW_WIDTH: u32 = 1280;

/// The height a window carries when the application does not name one.
pub const DEFAULT_WINDOW_HEIGHT: u32 = 800;

/// The narrowest window that may be created.
///
/// Below this a window cannot show its own chrome, and a host asked for one would have to
/// either refuse it or create something the user cannot use.
pub const MIN_WINDOW_WIDTH: u32 = 200;

/// The shortest window that may be created.
pub const MIN_WINDOW_HEIGHT: u32 = 200;

/// The widest window that may be created.
pub const MAX_WINDOW_WIDTH: u32 = 16_384;

/// The tallest window that may be created.
pub const MAX_WINDOW_HEIGHT: u32 = 16_384;

/// A window the host is asked to open.
///
/// Serializable, because it is part of the agent-facing surface: an agent that opens a
/// window with a title and a size gets the same window an application would.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowRequest {
    /// Where the window's page starts.
    pub url: String,
    /// The title the window's frame carries.
    #[serde(default = "default_title")]
    pub title: String,
    /// The window's width in logical pixels.
    #[serde(default = "default_width")]
    pub width: u32,
    /// The window's height in logical pixels.
    #[serde(default = "default_height")]
    pub height: u32,
    /// Where the window's left edge sits, when the caller has an opinion.
    ///
    /// Optional because a window placed by the platform is what a first run gets: an
    /// application that has never been run has no idea where the user would like its window, and
    /// inventing a position would put every first window in the same corner of the screen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<i32>,
    /// Where the window's top edge sits, when the caller has an opinion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<i32>,
    /// Whether the window floats above other windows.
    ///
    /// Defaulted to false rather than to "whatever the platform does": a window that floats is a
    /// window the user cannot get out of the way, so it is something a caller asks for.
    #[serde(default)]
    pub always_on_top: bool,
    /// Whether the window carries its own frame.
    #[serde(default = "default_true")]
    pub decorations: bool,
    /// Whether the window's background may be see-through.
    ///
    /// A transparent window is only useful with a page that draws something into it, and on some
    /// platforms it costs a compositing path — so it is opt-in, like floating.
    #[serde(default)]
    pub transparent: bool,
    /// Whether the user may resize the window.
    #[serde(default = "default_true")]
    pub resizable: bool,
    /// The smallest width the user may drag the window to, when a caller has a floor in mind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_width: Option<u32>,
    /// The smallest height the user may drag the window to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_height: Option<u32>,
    /// Whether the window fills the display it is on.
    ///
    /// A window created fullscreen is a window the platform makes fullscreen at creation, rather
    /// than one a page asks to be made fullscreen after the user has seen it in a frame.
    #[serde(default)]
    pub fullscreen: bool,
    /// Whether the window is created maximised.
    #[serde(default)]
    pub maximized: bool,
    /// The image the window carries, when the caller has one.
    ///
    /// Absent rather than defaulted: a window with no icon is a window the platform draws with the
    /// program's own icon, and inventing one here would put a picture on every application that
    /// never chose it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<Icon>,
}

/// The default for a flag that is on unless a caller says otherwise.
const fn default_true() -> bool {
    true
}

/// Where a window is and how big it is, as the platform reports it.
///
/// The unit of a remembered window: what an application writes down when it closes so the next
/// run can put the window back where the user left it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowGeometry {
    /// The left edge.
    pub x: i32,
    /// The top edge.
    pub y: i32,
    /// The width in logical pixels.
    pub width: u32,
    /// The height in logical pixels.
    pub height: u32,
}

/// A display's rectangle, in logical pixels.
///
/// What a host can report about the displays it is on. The clamp below needs nothing else: it
/// asks whether a window would be reachable, not which display is the primary one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorRect {
    /// The left edge.
    pub x: i32,
    /// The top edge.
    pub y: i32,
    /// The width.
    pub width: u32,
    /// The height.
    pub height: u32,
}

impl MonitorRect {
    /// Whether a point is inside this display.
    #[must_use]
    pub const fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x
            && y >= self.y
            && x < self.x.saturating_add(self.width.cast_signed())
            && y < self.y.saturating_add(self.height.cast_signed())
    }
}

impl WindowGeometry {
    /// This geometry moved so that its title bar is on a display that exists.
    ///
    /// A window is *reachable* when the point a user would drag it by is inside a display. The
    /// remembered size is kept, and the position is moved to the nearest point that is reachable:
    /// a window the user can grab and move is a window they can fix, while one that opens
    /// off-screen is a window they cannot find at all.
    ///
    /// With no displays to compare against — a host that cannot report them, or a headless run —
    /// the geometry is returned unchanged, because moving it would be guessing.
    #[must_use]
    pub fn clamped_to(&self, monitors: &[MonitorRect]) -> Self {
        if monitors.is_empty()
            || monitors
                .iter()
                .any(|monitor| monitor.contains(self.x, self.y))
        {
            return *self;
        }
        // The nearest display to the remembered place, by the distance from it to the display's
        // own rectangle. A display the window is already inside would have won above.
        let Some(nearest) = monitors
            .iter()
            .min_by_key(|monitor| {
                let right = monitor.x.saturating_add(monitor.width.cast_signed());
                let bottom = monitor.y.saturating_add(monitor.height.cast_signed());
                let dx = if self.x < monitor.x {
                    monitor.x - self.x
                } else if self.x >= right {
                    self.x - right + 1
                } else {
                    0
                };
                let dy = if self.y < monitor.y {
                    monitor.y - self.y
                } else if self.y >= bottom {
                    self.y - bottom + 1
                } else {
                    0
                };
                i64::from(dx).pow(2) + i64::from(dy).pow(2)
            })
            .copied()
        else {
            // Unreachable: the list is not empty above. Written as a refusal rather than a
            // panic because a geometry is not worth stopping a process over.
            return *self;
        };
        let right = nearest.x.saturating_add(nearest.width.cast_signed());
        let bottom = nearest.y.saturating_add(nearest.height.cast_signed());
        Self {
            x: self.x.clamp(nearest.x, right.saturating_sub(1)),
            y: self.y.clamp(nearest.y, bottom.saturating_sub(1)),
            width: self.width,
            height: self.height,
        }
    }
}

impl WindowGeometry {
    /// Whether this is a geometry a window can be created with.
    ///
    /// A remembered geometry is a *file* an application wrote, so it is input: a stored size
    /// below the minimum, or one past the maximum, is refused rather than handed to the platform
    /// — a window asked for a size the platform will not accept is a window that does not open.
    #[must_use]
    pub const fn is_usable(&self) -> bool {
        self.width >= MIN_WINDOW_WIDTH
            && self.width <= MAX_WINDOW_WIDTH
            && self.height >= MIN_WINDOW_HEIGHT
            && self.height <= MAX_WINDOW_HEIGHT
    }
}

/// How a window is being presented right now, as its host reports it.
///
/// Reported rather than remembered: a window's presentation is the platform's answer, and the
/// user can change it at any moment by dragging, maximising or focusing. A runtime that kept its
/// own copy would report what it asked for rather than what the user has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WindowState {
    /// Whether the window fills the display it is on.
    pub fullscreen: bool,
    /// Whether the window is maximised.
    pub maximized: bool,
    /// Whether the window is minimised.
    pub minimized: bool,
    /// Whether the window is on screen at all.
    ///
    /// Distinct from the three above: a window can be un-minimised, un-maximised and unfocused and
    /// still be hidden, which is how an application shows a splash window, hides it, and shows the
    /// real one — or keeps a background window alive without putting it in the user's way.
    pub visible: bool,
    /// Whether the platform is drawing the window's frame.
    ///
    /// The read side of the `decorations` a shape sets, and the fact a page needs to decide whether to
    /// draw its own title bar: a page that assumed decorations without asking would draw a second one
    /// over the platform's.
    pub decorated: bool,
    /// Whether the user may drag the window's edges to resize it.
    pub resizable: bool,
    /// Whether the window floats above the others.
    pub always_on_top: bool,
    /// Whether the window is the one the platform is giving input to.
    pub focused: bool,
}

/// Whether the platform is drawing its own interface light or dark.
///
/// Reported rather than chosen: the theme is the user's setting for the whole machine, and a window
/// that decided for itself would be a window that disagrees with every other one on the screen. A
/// page asks so it can follow, which is the only thing it can do about a choice that is not its.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    /// The platform draws its interface light.
    Light,
    /// The platform draws its interface dark.
    Dark,
}

impl Theme {
    /// The theme's name, as a page or an agent reads it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }
}

/// One change a caller may ask of a window's presentation.
///
/// A closed set rather than a bag of booleans: "make this fullscreen" and "make this maximised"
/// are different requests, and a caller that sent both would be asking for whichever the platform
/// happened to apply last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "change", content = "value")]
pub enum WindowChange {
    /// Fill the display, or return to the windowed size.
    Fullscreen(bool),
    /// Maximise, or restore.
    Maximized(bool),
    /// Minimise, or restore.
    Minimized(bool),
    /// Show the window, or hide it.
    ///
    /// Hiding is not closing: the window and its page stay alive, and showing it again is what makes
    /// a splash-then-main or a keep-it-warm window possible.
    Visible(bool),
    /// Bring the window to the front and give it input focus.
    Focus,
}

impl WindowChange {
    /// The name of the field this change sets, for an error message.
    #[must_use]
    pub const fn field(self) -> &'static str {
        match self {
            Self::Fullscreen(_) => "fullscreen",
            Self::Maximized(_) => "maximized",
            Self::Minimized(_) => "minimized",
            Self::Visible(_) => "visible",
            Self::Focus => "focus",
        }
    }

    /// The change a payload asks for, or `None` when it asks for none.
    ///
    /// The closed set and its rule live with the type rather than in whichever layer happens to
    /// read a payload: "at most one change, and each named exactly" is a property of what a change
    /// *is*, and a second reader of the same payload would otherwise have to know it too.
    ///
    /// # Errors
    ///
    /// Returns [`WindowChangeError`] when a field is not a boolean, when `focus` is asked to be
    /// false, or when more than one change is named.
    pub fn from_payload(payload: &serde_json::Value) -> Result<Option<Self>, WindowChangeError> {
        let mut named: Vec<Self> = Vec::new();
        for field in ["fullscreen", "maximized", "minimized", "visible"] {
            if let Some(value) = payload.get(field) {
                let value = value
                    .as_bool()
                    .ok_or(WindowChangeError::NotABoolean { field })?;
                named.push(match field {
                    "fullscreen" => Self::Fullscreen(value),
                    "maximized" => Self::Maximized(value),
                    "minimized" => Self::Minimized(value),
                    _ => Self::Visible(value),
                });
            }
        }
        if let Some(value) = payload.get("focus") {
            let value = value
                .as_bool()
                .ok_or(WindowChangeError::NotABoolean { field: "focus" })?;
            // There is no way to take focus *away* from a window: the platform decides who has it,
            // and a caller asking for `focus: false` is asking for something nobody can promise.
            if !value {
                return Err(WindowChangeError::CannotUnfocus);
            }
            named.push(Self::Focus);
        }
        match named.len() {
            0 => Ok(None),
            1 => Ok(named.pop()),
            _ => Err(WindowChangeError::MoreThanOne),
        }
    }
}

/// Why a payload does not describe a change a window can be asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowChangeError {
    /// A field that names a change carries something other than a boolean.
    NotABoolean {
        /// The field.
        field: &'static str,
    },
    /// `focus: false`, which is not a request this runtime can honour.
    CannotUnfocus,
    /// More than one change in one request, which would be asking for whichever the platform
    /// happened to apply last.
    MoreThanOne,
}

impl WindowChangeError {
    /// The field the caller should look at.
    #[must_use]
    pub const fn field(self) -> &'static str {
        match self {
            Self::NotABoolean { field } => field,
            Self::CannotUnfocus => "focus",
            Self::MoreThanOne => "window",
        }
    }
}

impl fmt::Display for WindowChangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotABoolean { field } => write!(f, "{field} must be true or false"),
            Self::CannotUnfocus => f.write_str(
                "focus cannot be false: a window is focused by bringing it forward, and nothing \
                 here can take the platform's focus away",
            ),
            Self::MoreThanOne => f.write_str(
                "at most one of `fullscreen`, `maximized`, `minimized`, `visible` and `focus` may \
                 be given: each names a different request",
            ),
        }
    }
}

/// The largest edge an icon may have.
///
/// The limit is what a platform actually accepts: the largest icon in a Windows icon resource is
/// 256, and a caller that asked for more would be handing the platform a bitmap it will scale down
/// or refuse. Beyond this the decoder refuses rather than allocating a buffer the size of a screen
/// from a header a file happened to contain.
pub const MAX_ICON_EDGE: u32 = 1_024;

/// The image a window carries in its frame, its task bar entry and its alt-tab tile.
///
/// Straight (non-premultiplied) RGBA, row-major from the top-left, because that is what every
/// platform's window-creation call takes. The decoding — a file on disk into these pixels — is
/// deliberately not here: this crate is the port, and what a file format means is a decision the
/// runtime makes, so the port carries the decoded result rather than the bytes it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Icon {
    /// The icon's width in pixels.
    pub width: u32,
    /// The icon's height in pixels.
    pub height: u32,
    /// The pixels, four bytes per pixel in red, green, blue, alpha order.
    pub rgba: Vec<u8>,
}

impl Icon {
    /// Whether this is an icon a window can be given.
    ///
    /// The buffer must hold exactly the pixels its dimensions name. A short buffer is a buffer the
    /// platform would read past the end of, so it is refused here rather than at the boundary where
    /// the answer is a crash.
    #[must_use]
    pub fn is_whole(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.width <= MAX_ICON_EDGE
            && self.height <= MAX_ICON_EDGE
            && self
                .width
                .checked_mul(self.height)
                .and_then(|pixels| pixels.checked_mul(4))
                .is_some_and(|expected| {
                    usize::try_from(expected).is_ok_and(|expected| expected == self.rgba.len())
                })
    }
}

/// What a caller asks a window to become, after it exists.
///
/// Everything a window's request can name that a platform can change afterwards. A field that is
/// absent is left alone rather than reset: the user may have moved the window, and a page that
/// asked about the title must not drag it back to where it was created.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WindowShape {
    /// The title the window's frame carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Where the window's left edge goes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<i32>,
    /// Where the window's top edge goes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<i32>,
    /// The window's width in logical pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// The window's height in logical pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Whether the window floats above other windows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub always_on_top: Option<bool>,
    /// Whether the user may resize the window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resizable: Option<bool>,
}

impl WindowShape {
    /// Whether this shape asks for nothing at all.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.x.is_none()
            && self.y.is_none()
            && self.width.is_none()
            && self.height.is_none()
            && self.always_on_top.is_none()
            && self.resizable.is_none()
    }

    /// Whether this is a shape a window can be asked to take.
    ///
    /// # Errors
    ///
    /// Returns [`WindowRequestError`] when the shape names nothing, when a title is empty, when a
    /// position or size names one half of itself, or when a dimension is outside the range a window
    /// may have. The rules are the request's own, because a window that may not be *created* at a
    /// size may not be resized to it either.
    pub fn validate(&self) -> Result<(), WindowRequestError> {
        if self.is_empty() {
            return Err(WindowRequestError::EmptyShape);
        }
        if self
            .title
            .as_ref()
            .is_some_and(|title| title.trim().is_empty())
        {
            return Err(WindowRequestError::EmptyTitle);
        }
        if let Some(width) = self.width
            && !(MIN_WINDOW_WIDTH..=MAX_WINDOW_WIDTH).contains(&width)
        {
            return Err(WindowRequestError::WidthOutOfRange { width });
        }
        if let Some(height) = self.height
            && !(MIN_WINDOW_HEIGHT..=MAX_WINDOW_HEIGHT).contains(&height)
        {
            return Err(WindowRequestError::HeightOutOfRange { height });
        }
        if self.x.is_some() != self.y.is_some() {
            return Err(WindowRequestError::HalfPosition);
        }
        if self.width.is_some() != self.height.is_some() {
            return Err(WindowRequestError::HalfSize);
        }
        Ok(())
    }
}

/// The title used when a request does not name one.
fn default_title() -> String {
    DEFAULT_WINDOW_TITLE.to_owned()
}
/// The width used when a request does not name one.
fn default_width() -> u32 {
    DEFAULT_WINDOW_WIDTH
}

/// The height used when a request does not name one.
fn default_height() -> u32 {
    DEFAULT_WINDOW_HEIGHT
}

impl WindowRequest {
    /// A request for a window at `url`, with the default title and size.
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            title: default_title(),
            width: default_width(),
            height: default_height(),
            x: None,
            y: None,
            always_on_top: false,
            decorations: true,
            transparent: false,
            resizable: true,
            min_width: None,
            min_height: None,
            fullscreen: false,
            maximized: false,
            icon: None,
        }
    }

    /// A request with a title.
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// A request with a place to sit, in logical pixels.
    ///
    /// Both coordinates or neither: a window given one axis would be placed by a rule no caller
    /// asked for, and the platform's own rule is a better one than a guess.
    #[must_use]
    pub const fn with_position(mut self, x: i32, y: i32) -> Self {
        self.x = Some(x);
        self.y = Some(y);
        self
    }

    /// Sets whether the window floats above other windows.
    #[must_use]
    pub const fn with_always_on_top(mut self, always_on_top: bool) -> Self {
        self.always_on_top = always_on_top;
        self
    }

    /// Sets whether the window carries its own frame.
    #[must_use]
    pub const fn with_decorations(mut self, decorations: bool) -> Self {
        self.decorations = decorations;
        self
    }

    /// Sets whether the window's background may be see-through.
    #[must_use]
    pub const fn with_transparent(mut self, transparent: bool) -> Self {
        self.transparent = transparent;
        self
    }

    /// Sets whether the user may resize the window.
    #[must_use]
    pub const fn with_resizable(mut self, resizable: bool) -> Self {
        self.resizable = resizable;
        self
    }

    /// Sets the smallest size the user may drag the window to.
    #[must_use]
    pub const fn with_min_size(mut self, width: u32, height: u32) -> Self {
        self.min_width = Some(width);
        self.min_height = Some(height);
        self
    }

    /// A request with a size, in logical pixels.
    #[must_use]
    pub const fn with_size(mut self, width: u32, height: u32) -> Self {
        self.width = width;
        self.height = height;
        self
    }

    /// Sets whether the window is created filling its display.
    #[must_use]
    pub const fn with_fullscreen(mut self, fullscreen: bool) -> Self {
        self.fullscreen = fullscreen;
        self
    }

    /// Sets whether the window is created maximised.
    #[must_use]
    pub const fn with_maximized(mut self, maximized: bool) -> Self {
        self.maximized = maximized;
        self
    }

    /// Gives the window the image it carries.
    #[must_use]
    pub fn with_icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }

    /// Whether the request describes a window the host may create.
    ///
    /// # Errors
    ///
    /// Returns [`WindowRequestError`] when the URL is empty, the title is empty, or a
    /// dimension is outside the range a window may be created at. A request that names an
    /// impossible window is refused at the boundary, where the reason can be reported,
    /// rather than at the engine, where it would be a crash.
    pub fn validate(&self) -> Result<(), WindowRequestError> {
        if self.url.trim().is_empty() {
            return Err(WindowRequestError::EmptyUrl);
        }
        if let Some(min_width) = self.min_width
            && !(MIN_WINDOW_WIDTH..=self.width).contains(&min_width)
        {
            return Err(WindowRequestError::MinimumSizeOutOfRange {
                field: "min_width",
                value: min_width,
                size: self.width,
            });
        }
        if let Some(min_height) = self.min_height
            && !(MIN_WINDOW_HEIGHT..=self.height).contains(&min_height)
        {
            return Err(WindowRequestError::MinimumSizeOutOfRange {
                field: "min_height",
                value: min_height,
                size: self.height,
            });
        }
        // A floor with no wall is not a floor: a caller that names one dimension and not the other
        // gets a window the user can squash flat in the dimension nobody constrained.
        if self.min_width.is_some() != self.min_height.is_some() {
            return Err(WindowRequestError::HalfMinimumSize);
        }
        if self.title.trim().is_empty() {
            return Err(WindowRequestError::EmptyTitle);
        }
        if !(MIN_WINDOW_WIDTH..=MAX_WINDOW_WIDTH).contains(&self.width) {
            return Err(WindowRequestError::WidthOutOfRange { width: self.width });
        }
        if !(MIN_WINDOW_HEIGHT..=MAX_WINDOW_HEIGHT).contains(&self.height) {
            return Err(WindowRequestError::HeightOutOfRange {
                height: self.height,
            });
        }
        if let Some(icon) = &self.icon
            && !icon.is_whole()
        {
            return Err(WindowRequestError::BadIcon {
                width: icon.width,
                height: icon.height,
                bytes: icon.rgba.len(),
            });
        }
        Ok(())
    }
}

/// A request that describes a window which cannot be created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowRequestError {
    /// The request names no URL.
    EmptyUrl,
    /// The request names no title.
    EmptyTitle,
    /// The width is outside the range a window may be created at.
    WidthOutOfRange {
        /// The width that was asked for.
        width: u32,
    },
    /// The height is outside the range a window may be created at.
    HeightOutOfRange {
        /// The height that was asked for.
        height: u32,
    },
    /// A floor the user may drag the window to is not one the window could have.
    MinimumSizeOutOfRange {
        /// Which floor.
        field: &'static str,
        /// What was asked for.
        value: u32,
        /// The window's own size in that dimension.
        size: u32,
    },
    /// One dimension of the floor was given and the other was not.
    HalfMinimumSize,
    /// A shape that names nothing to change.
    EmptyShape,
    /// One axis of a position was given and the other was not.
    HalfPosition,
    /// One dimension of a size was given and the other was not.
    HalfSize,
    /// The icon is not one a window can be given.
    BadIcon {
        /// The width the icon claims.
        width: u32,
        /// The height the icon claims.
        height: u32,
        /// How many bytes of pixels came with it.
        bytes: usize,
    },
}

impl fmt::Display for WindowRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyUrl => write!(f, "a window request must name a URL"),
            Self::EmptyTitle => write!(f, "a window request must name a title"),
            Self::WidthOutOfRange { width } => write!(
                f,
                "a window width of {width} is outside {MIN_WINDOW_WIDTH}..={MAX_WINDOW_WIDTH}"
            ),
            Self::HeightOutOfRange { height } => write!(
                f,
                "a window height of {height} is outside {MIN_WINDOW_HEIGHT}..={MAX_WINDOW_HEIGHT}"
            ),
            Self::MinimumSizeOutOfRange { field, value, size } => write!(
                f,
                "a window `{field}` of {value} is not a floor the window could have: it must be                  at least the minimum and at most the window's own {size}"
            ),
            Self::HalfMinimumSize => write!(
                f,
                "a window's minimum size must name both dimensions: one without the other leaves                  the user able to squash the window flat in the dimension nobody constrained"
            ),
            Self::EmptyShape => write!(
                f,
                "a window shape must name something to change: a shape with no field set asks for                  nothing"
            ),
            Self::HalfPosition => write!(
                f,
                "a window's position must name both axes: one without the other places the window                  by a rule nobody asked for"
            ),
            Self::HalfSize => write!(
                f,
                "a window's size must name both dimensions: one without the other leaves the window                  at whatever size the other axis happened to have"
            ),
            Self::BadIcon {
                width,
                height,
                bytes,
            } => write!(
                f,
                "an icon of {width}x{height} carrying {bytes} bytes is not a whole image: it must be \
                 between 1 and {MAX_ICON_EDGE} pixels on each edge and carry four bytes per pixel"
            ),
        }
    }
}

impl std::error::Error for WindowRequestError {}

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_geometry_is_usable_only_when_a_window_could_have_it() {
        // A remembered geometry is a file an application wrote, so it is input: a size below the
        // minimum or past the maximum is refused rather than handed to the platform, because a
        // window asked for a size it cannot have is a window that does not open.
        let usable = WindowGeometry {
            x: 10,
            y: 20,
            width: MIN_WINDOW_WIDTH,
            height: MIN_WINDOW_HEIGHT,
        };
        assert!(usable.is_usable());
        for geometry in [
            WindowGeometry {
                width: MIN_WINDOW_WIDTH - 1,
                ..usable
            },
            WindowGeometry {
                height: MIN_WINDOW_HEIGHT - 1,
                ..usable
            },
            WindowGeometry {
                width: MAX_WINDOW_WIDTH + 1,
                ..usable
            },
            WindowGeometry {
                height: MAX_WINDOW_HEIGHT + 1,
                ..usable
            },
        ] {
            assert!(!geometry.is_usable(), "{geometry:?}");
        }
        // A negative position is a legitimate geometry: a second monitor is often to the left of
        // the first, and refusing negative coordinates would refuse half of them.
        assert!(
            WindowGeometry {
                x: -1920,
                y: -200,
                ..usable
            }
            .is_usable()
        );
    }

    #[test]
    fn a_remembered_place_on_a_display_that_is_gone_is_moved_onto_one_that_exists() {
        // The defect this closes: a geometry stored while a second monitor was attached puts the
        // window where that monitor used to be. The user cannot see it and cannot move it.
        let primary = MonitorRect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let remembered = WindowGeometry {
            x: 2400,
            y: 300,
            width: 800,
            height: 600,
        };
        let clamped = remembered.clamped_to(&[primary]);
        assert!(
            primary.contains(clamped.x, clamped.y),
            "the window must land on a display that exists: {clamped:?}"
        );
        assert_eq!(clamped.width, 800, "the remembered size is kept");
        assert_eq!(clamped.height, 600);

        // A place that is already reachable is left exactly where it was: a window the user
        // moved is a window the user chose.
        let reachable = WindowGeometry {
            x: 100,
            y: 100,
            ..remembered
        };
        assert_eq!(reachable.clamped_to(&[primary]), reachable);

        // Two displays: the nearest one wins, so a window from the left screen lands on the left
        // screen rather than being dragged across the desk.
        let left = MonitorRect {
            x: -1920,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let from_the_left = WindowGeometry {
            x: -1800,
            y: 200,
            ..remembered
        };
        assert_eq!(from_the_left.clamped_to(&[primary, left]), from_the_left);
        let gone_left = WindowGeometry {
            x: -3000,
            y: 200,
            ..remembered
        };
        let moved = gone_left.clamped_to(&[primary, left]);
        assert!(left.contains(moved.x, moved.y), "{moved:?}");

        // A host that cannot report displays moves nothing: guessing would be worse than leaving
        // the geometry as the user left it.
        assert_eq!(remembered.clamped_to(&[]), remembered);
    }

    #[test]
    fn the_window_options_default_to_a_window_a_user_can_move() {
        // The defaults are the ones that need no explanation: a window a user can move, resize
        // and close, which is what every window was before anyone asked for anything else.
        let plain = WindowRequest::new("ncui://app");
        assert!(!plain.always_on_top, "a window does not float uninvited");
        assert!(plain.decorations, "and carries its frame");
        assert!(!plain.transparent, "and is opaque");
        assert!(plain.resizable, "and can be resized");
        assert_eq!(plain.min_width, None, "and has no floor nobody named");

        // A floor is checked against the window it belongs to, in both dimensions, and half a
        // floor is refused by the request itself rather than only by whoever reads a payload.
        let sized = WindowRequest::new("ncui://app").with_size(800, 600);
        assert!(sized.clone().with_min_size(400, 300).validate().is_ok());
        assert!(matches!(
            sized.clone().with_min_size(100, 300).validate(),
            Err(WindowRequestError::MinimumSizeOutOfRange {
                field: "min_width",
                ..
            })
        ));
        assert!(matches!(
            sized.clone().with_min_size(400, 1_000).validate(),
            Err(WindowRequestError::MinimumSizeOutOfRange {
                field: "min_height",
                ..
            })
        ));
        let mut half = sized.clone();
        half.min_width = Some(400);
        assert!(matches!(
            half.validate(),
            Err(WindowRequestError::HalfMinimumSize)
        ));

        // The options survive the wire, which is how an agent asks for the same window a page can.
        let options = sized
            .with_always_on_top(true)
            .with_decorations(false)
            .with_resizable(false);
        let encoded = serde_json::to_string(&options).expect("a request encodes");
        let decoded: WindowRequest = serde_json::from_str(&encoded).expect("and reads back");
        assert_eq!(decoded, options);
        assert!(encoded.contains("always_on_top"), "{encoded}");
    }

    #[test]
    fn a_request_carries_a_place_and_survives_the_wire() {
        let request = WindowRequest::new("ncui://app")
            .with_size(640, 480)
            .with_position(120, 90);
        assert_eq!(request.x, Some(120));
        assert_eq!(request.y, Some(90));
        let encoded = serde_json::to_string(&request).expect("a request encodes");
        let decoded: WindowRequest = serde_json::from_str(&encoded).expect("and reads back");
        assert_eq!(decoded, request);
        // A request with no place says nothing about one, rather than saying "zero".
        let plain = WindowRequest::new("ncui://app");
        assert_eq!(plain.x, None);
        assert!(
            !serde_json::to_string(&plain)
                .expect("encodes")
                .contains("\"x\"")
        );
    }

    #[test]
    fn a_request_carries_the_defaults_it_did_not_overrule() {
        let request = WindowRequest::new("https://app.example.com");
        assert_eq!(request.url, "https://app.example.com");
        assert_eq!(request.title, DEFAULT_WINDOW_TITLE);
        assert_eq!(request.width, DEFAULT_WINDOW_WIDTH);
        assert_eq!(request.height, DEFAULT_WINDOW_HEIGHT);
        assert!(request.validate().is_ok());
    }

    #[test]
    fn a_request_may_name_its_title_and_size() {
        let request = WindowRequest::new("https://app.example.com")
            .with_title("Counter")
            .with_size(640, 480);
        assert_eq!(request.title, "Counter");
        assert_eq!(request.width, 640);
        assert_eq!(request.height, 480);
        assert!(request.validate().is_ok());
    }

    #[test]
    fn a_request_that_names_no_url_is_refused() {
        let request = WindowRequest::new("   ");
        assert_eq!(request.validate(), Err(WindowRequestError::EmptyUrl));
    }

    #[test]
    fn a_request_that_names_no_title_is_refused() {
        let request = WindowRequest::new("https://app.example.com").with_title(" ");
        assert_eq!(request.validate(), Err(WindowRequestError::EmptyTitle));
    }

    #[test]
    fn a_window_smaller_or_larger_than_the_limits_is_refused() {
        let too_small = WindowRequest::new("https://app.example.com").with_size(1, 400);
        assert_eq!(
            too_small.validate(),
            Err(WindowRequestError::WidthOutOfRange { width: 1 })
        );
        let too_tall = WindowRequest::new("https://app.example.com").with_size(400, 1);
        assert_eq!(
            too_tall.validate(),
            Err(WindowRequestError::HeightOutOfRange { height: 1 })
        );
        let too_wide = WindowRequest::new("https://app.example.com").with_size(100_000, 400);
        assert!(too_wide.validate().is_err());
    }

    #[test]
    fn a_request_round_trips_through_json_with_its_defaults() {
        // The request is part of the agent-facing surface, so it has to survive the trip
        // through the wire format the agent uses.
        let request = WindowRequest::new("https://app.example.com").with_title("Counter");
        let encoded = serde_json::to_string(&request).expect("encode");
        let decoded: WindowRequest = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(decoded, request);

        // A message that names only a URL takes the defaults rather than failing.
        let partial: WindowRequest =
            serde_json::from_str(r#"{"url":"https://app.example.com"}"#).expect("decode");
        assert_eq!(partial.title, DEFAULT_WINDOW_TITLE);
        assert_eq!(partial.width, DEFAULT_WINDOW_WIDTH);
    }

    #[test]
    fn a_presentation_payload_names_at_most_one_change() {
        let read = |json: &str| {
            WindowChange::from_payload(&serde_json::from_str(json).expect("a payload"))
        };
        assert_eq!(read("{}").expect("no change"), None, "no change is a read");
        assert_eq!(
            read(r#"{"fullscreen":true}"#).expect("one change"),
            Some(WindowChange::Fullscreen(true))
        );
        assert_eq!(
            read(r#"{"minimized":false}"#).expect("one change"),
            Some(WindowChange::Minimized(false))
        );
        assert_eq!(
            read(r#"{"focus":true}"#).expect("one change"),
            Some(WindowChange::Focus)
        );
        assert_eq!(
            read(r#"{"visible":false}"#).expect("one change"),
            Some(WindowChange::Visible(false)),
            "hiding a window is a change like any other"
        );
        assert_eq!(
            read(r#"{"minimized":true,"visible":true}"#),
            Err(WindowChangeError::MoreThanOne),
            "minimising and hiding are different requests, not two spellings of one"
        );
        assert_eq!(
            read(r#"{"fullscreen":true,"maximized":true}"#),
            Err(WindowChangeError::MoreThanOne),
            "two changes in one request would be asking for whichever the platform applied last"
        );
        assert_eq!(
            read(r#"{"focus":false}"#),
            Err(WindowChangeError::CannotUnfocus),
            "nothing here can take the platform's focus away"
        );
        assert_eq!(
            read(r#"{"maximized":"yes"}"#),
            Err(WindowChangeError::NotABoolean { field: "maximized" })
        );
        assert_eq!(WindowChangeError::MoreThanOne.field(), "window");
        assert_eq!(WindowChangeError::CannotUnfocus.field(), "focus");
    }

    #[test]
    fn a_shape_names_something_and_holds_together() {
        let read = |json: &str| -> WindowShape { serde_json::from_str(json).expect("a shape") };
        assert!(WindowShape::default().is_empty());
        assert_eq!(
            WindowShape::default().validate(),
            Err(WindowRequestError::EmptyShape),
            "a shape that asks for nothing is refused rather than answered with a no-op"
        );
        assert_eq!(
            read(r#"{"title":"notes"}"#).validate(),
            Ok(()),
            "a title alone is a shape"
        );
        assert_eq!(
            read(r#"{"title":"  "}"#).validate(),
            Err(WindowRequestError::EmptyTitle),
            "a window with a blank title has no title to show"
        );
        assert_eq!(
            read(r#"{"x":10}"#).validate(),
            Err(WindowRequestError::HalfPosition)
        );
        assert_eq!(
            read(r#"{"width":640}"#).validate(),
            Err(WindowRequestError::HalfSize)
        );
        assert_eq!(
            read(&format!(
                r#"{{"width":{},"height":480}}"#,
                MIN_WINDOW_WIDTH - 1
            ))
            .validate(),
            Err(WindowRequestError::WidthOutOfRange {
                width: MIN_WINDOW_WIDTH - 1
            }),
            "a size a window may not be created at is a size it may not be resized to"
        );
        assert_eq!(
            read(r#"{"x":10,"y":20,"width":640,"height":480,"always_on_top":true}"#).validate(),
            Ok(())
        );
    }

    #[test]
    fn a_shape_carries_only_what_it_names() {
        let shape: WindowShape = serde_json::from_str(r#"{"resizable":false}"#).expect("a shape");
        assert_eq!(shape.resizable, Some(false));
        assert_eq!(shape.title, None);
        assert_eq!(
            (shape.x, shape.y, shape.width, shape.height),
            (None, None, None, None),
            "a field a caller did not name is absent, so the window keeps what the user left it as"
        );
    }

    #[test]
    fn an_icon_is_carried_only_when_it_is_whole() {
        let icon = Icon {
            width: 2,
            height: 1,
            rgba: vec![255, 0, 0, 255, 0, 255, 0, 255],
        };
        assert!(icon.is_whole());
        let request = WindowRequest::new("ncui://app").with_icon(icon.clone());
        assert!(request.validate().is_ok());
        assert_eq!(request.icon.as_ref(), Some(&icon));

        // A buffer that does not hold the pixels its dimensions name is refused: the platform
        // would read past the end of it.
        let short = Icon {
            rgba: vec![0; 7],
            ..icon.clone()
        };
        assert!(!short.is_whole());
        assert!(matches!(
            WindowRequest::new("ncui://app").with_icon(short).validate(),
            Err(WindowRequestError::BadIcon { .. })
        ));
        // An icon with no pixels is not an icon.
        assert!(
            !Icon {
                width: 0,
                height: 0,
                rgba: Vec::new()
            }
            .is_whole()
        );
        // And one larger than a platform accepts is refused rather than handed over.
        assert!(
            !Icon {
                width: MAX_ICON_EDGE + 1,
                height: 1,
                rgba: vec![0; (MAX_ICON_EDGE as usize + 1) * 4]
            }
            .is_whole()
        );

        // A request with no icon says nothing about one, so a platform's own icon is left alone.
        let plain = WindowRequest::new("ncui://app");
        assert_eq!(plain.icon, None);
        assert!(
            !serde_json::to_string(&plain)
                .expect("encodes")
                .contains("icon")
        );
        // And an icon survives the wire, which is how an agent opens the same window a page does.
        let encoded = serde_json::to_string(&request).expect("encode");
        let decoded: WindowRequest = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(decoded, request);
    }

    #[test]
    fn a_request_carries_a_presentation_and_survives_the_wire() {
        let request = WindowRequest::new("https://app.example.com")
            .with_fullscreen(true)
            .with_maximized(true);
        let encoded = serde_json::to_string(&request).expect("encode");
        let decoded: WindowRequest = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(decoded, request);
        assert!(decoded.fullscreen && decoded.maximized);
        // A request that names neither is windowed, which is what every window was before this
        // existed: the field is additive rather than a behaviour change.
        let plain: WindowRequest =
            serde_json::from_str(r#"{"url":"https://app.example.com"}"#).expect("decode");
        assert!(!plain.fullscreen && !plain.maximized);
    }
}
