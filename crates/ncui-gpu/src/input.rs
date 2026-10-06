//! The input contract ADR-002 makes mandatory for GPU islands.
//!
//! ADR-002: "GPU Islands must support the full input contract: mouse, keyboard, focus,
//! scrolling, clipboard, selection, resize, high-DPI, drag and drop, IME, and accessibility.
//! An island that is a bare canvas and drops the rest is not acceptable."
//!
//! [`InputContract`] enumerates those eleven, and [`IslandHost::satisfied_contracts`] reports
//! which of them a given host can actually demonstrate, by *driving* the host and observing an
//! effect that item alone could produce. An implementation that drops IME is reported as missing
//! IME rather than assumed complete, and an item that cannot yet be demonstrated — accessibility,
//! because no semantic tree is built — is reported unsatisfied rather than claimed.
//!
//! It is queried by this crate's tests; **no registry, CLI command or policy tool reads it
//! yet**, so treat it as an in-crate capability report rather than a release gate. Wiring it to
//! the feature registry as ADR-002 evidence for `gpu.islands` is what would make it one.
//!
//! # Scope: this is the input half, not a renderer
//!
//! Nothing here draws. There is no GPU device, no render pass and no pixel output; the crate
//! is the compositing boundary's decision logic, which is what can be tested without hardware.
//! `gpu.islands` is therefore still recorded `not_implemented` in the feature registry, and
//! this module is the part of it that exists so far.
//!
//! # Why routing lives here and not in the renderer
//!
//! A pointer event arrives with window coordinates and no idea what it should mean. Deciding
//! "this belongs to island 3, at content offset (40, 12), over row 2" is logic, it is the same
//! logic on every frame, and it is exactly the DOM/GPU boundary ADR-002 calls highest-risk.
//! Keeping it out of the drawing code is what lets it be tested without a device.

use crate::geometry::{Island, Islands, LogicalPoint, LogicalSize, Rect, RenderMode, Viewport};

/// A key press, named rather than key-coded so the contract is legible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    /// Move focus forward.
    Tab,
    /// Move focus backward.
    ShiftTab,
    /// Activate.
    Enter,
    /// Select.
    Space,
    /// Leave the current composite.
    Escape,
    /// Move up.
    ArrowUp,
    /// Move down.
    ArrowDown,
    /// Move left.
    ArrowLeft,
    /// Move right.
    ArrowRight,
    /// Editable text: A.
    Character(char),
    /// Editable text: Backspace.
    Backspace,
    /// Editable text: Enter commits a composition.
    CompositionCommit,
    /// Editable text: Escape abandons a composition.
    CompositionCancel,
}

/// A mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseButton {
    /// The primary button.
    Primary,
    /// The secondary button.
    Secondary,
    /// The middle button.
    Middle,
}

/// What kind of pointer event this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerPhase {
    /// The pointer arrived.
    Enter,
    /// The pointer moved.
    Move,
    /// A button went down.
    Down,
    /// A button came up.
    Up,
    /// The pointer left.
    Leave,
    /// The wheel turned.
    Scroll,
}

/// A pointer event in window coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointerEvent {
    /// Where, in window coordinates.
    pub position: LogicalPoint,
    /// Which button, for the button phases.
    pub button: MouseButton,
    /// What happened.
    pub phase: PointerPhase,
    /// Scroll amount, for [`PointerPhase::Scroll`].
    pub scroll: LogicalPoint,
    /// Whether a modifier was held; island-local modifier handling, not a global flag.
    pub shift: bool,
    /// Whether the platform's second modifier was held.
    pub control: bool,
    /// Whether the platform's command/alt modifier was held.
    pub alt: bool,
}

impl PointerEvent {
    /// A pointer phase with no scroll and no modifiers.
    #[must_use]
    pub const fn new(position: LogicalPoint, phase: PointerPhase) -> Self {
        Self {
            position,
            button: MouseButton::Primary,
            phase,
            scroll: LogicalPoint::new(0.0, 0.0),
            shift: false,
            control: false,
            alt: false,
        }
    }

    /// With a scroll delta.
    #[must_use]
    pub const fn scrolling(mut self, delta: LogicalPoint) -> Self {
        self.scroll = delta;
        self
    }

    /// With a button.
    #[must_use]
    pub const fn with_button(mut self, button: MouseButton) -> Self {
        self.button = button;
        self
    }

    /// With modifiers.
    #[must_use]
    pub const fn with_modifiers(mut self, shift: bool, control: bool, alt: bool) -> Self {
        self.shift = shift;
        self.control = control;
        self.alt = alt;
        self
    }
}

/// What a drag carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DragPayload {
    /// Plain text.
    Text(String),
    /// A file path.
    File(String),
    /// Arbitrary bytes the host names.
    Bytes(Vec<u8>),
}

/// A drag-and-drop event.
#[derive(Debug, Clone, PartialEq)]
pub struct DragEvent {
    /// What is being dragged.
    pub payload: DragPayload,
    /// Where the pointer is, in window coordinates.
    pub position: LogicalPoint,
    /// Whether the drop is the end of a drag rather than a hover.
    pub dropping: bool,
}

/// The eleven behaviours ADR-002 requires of a GPU island.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum InputContract {
    /// Pointer events, routed to the right island and in content coordinates.
    Mouse,
    /// Key events, including the ones the host must not swallow.
    Keyboard,
    /// Focus entering and leaving an island.
    Focus,
    /// Scrolling, clamped to content.
    Scrolling,
    /// Copy and cut, and paste from the host.
    Clipboard,
    /// Range selection, including extending it.
    Selection,
    /// Resize, keeping content coordinates correct.
    Resize,
    /// High-DPI, meaning a scale factor applied once per window.
    HighDpi,
    /// Drag and drop, in and out.
    DragAndDrop,
    /// Text input including IME composition.
    Ime,
    /// The semantic tree, focus order and keyboard routing.
    Accessibility,
}

/// Every contract item, so "is it complete?" is checkable.
pub const INPUT_CONTRACT: &[InputContract] = &[
    InputContract::Mouse,
    InputContract::Keyboard,
    InputContract::Focus,
    InputContract::Scrolling,
    InputContract::Clipboard,
    InputContract::Selection,
    InputContract::Resize,
    InputContract::HighDpi,
    InputContract::DragAndDrop,
    InputContract::Ime,
    InputContract::Accessibility,
];

impl InputContract {
    /// The stable wire name, which is what `render_mode` observability reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mouse => "mouse",
            Self::Keyboard => "keyboard",
            Self::Focus => "focus",
            Self::Scrolling => "scrolling",
            Self::Clipboard => "clipboard",
            Self::Selection => "selection",
            Self::Resize => "resize",
            Self::HighDpi => "high_dpi",
            Self::DragAndDrop => "drag_and_drop",
            Self::Ime => "ime",
            Self::Accessibility => "accessibility",
        }
    }
}

/// Contract items with no implementation to probe yet.
///
/// Each is reported unsatisfied by [`IslandHost::probes`] rather than being probed, because a
/// probe that hardcodes its own answer is the defect this module exists to avoid.
///
/// Removing an entry does NOT fail a test: the constant and its guard are behaviourally inert
/// while the `Accessibility` arm in `probes` independently answers `false`. The assertion below
/// pins only that this item stays listed — it does not force the dispatch arm to be revisited,
/// so emptying the list and deleting the assertion together would still compile.
///
/// * [`InputContract::Accessibility`] — **deferred, not blocked.** `ncui-a11y` already owns the
///   semantic tree, focus order, keyboard routing and `IslandDescriptor::is_reachable`, and
///   ADR-011 permits the dependency (layer 2 → layer 1). What is missing is the wiring: nothing
///   in this crate builds a tree from its islands yet. Doing that is the work that removes this
///   entry, and it belongs with the renderer rather than with a contract probe.
const UNIMPLEMENTED: &[InputContract] = &[InputContract::Accessibility];

/// Keeps [`InputContract::Accessibility`] listed until someone deliberately removes it, so a
/// deferral cannot be dropped by an edit that touches nothing else. It pins the first element
/// only — a later entry would compile silently — which is deliberate: the list has one item and
/// this is a tripwire for the day it does not.
///
/// Written as a pattern over the literal rather than `UNIMPLEMENTED.contains(..)`, because slice
/// `contains` is not a `const fn`; this has to be evaluated at compile time to be worth anything.
const _: () = assert!(matches!(UNIMPLEMENTED, [InputContract::Accessibility, ..]));

impl core::fmt::Display for InputContract {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A row, so selection and hit-testing have something with a height.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// The row's stable id.
    pub id: String,
    /// The text the row displays.
    pub text: String,
    /// The row's height in content pixels.
    pub height: f32,
}

impl Row {
    /// A row.
    #[must_use]
    pub fn new(id: &str, text: &str, height: f32) -> Self {
        Self {
            id: id.to_owned(),
            text: text.to_owned(),
            height: height.max(0.0),
        }
    }
}

/// What the host did with an event.
#[derive(Debug, Clone, PartialEq)]
pub enum Routed {
    /// The event belonged to no island and went to the DOM.
    Dom,
    /// The island handled it, with the content-space point it was routed to.
    Island {
        /// Which island.
        id: String,
        /// The point in that island's content coordinates.
        content: LogicalPoint,
    },
    /// The host handled it, and the island should not see it.
    Host,
    /// Nothing should happen, and saying so is the point.
    Ignored,
}

/// The state one island keeps: selection, composition, focus.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IslandState {
    /// The selected row range, half-open.
    pub selection: Option<(usize, usize)>,
    /// An in-progress IME composition.
    pub composition: Option<String>,
    /// Whether the island holds focus.
    pub focused: bool,
    /// The clipboard contents the host holds for this island.
    pub clipboard: Option<String>,
}

impl IslandState {
    /// The rows a copy or cut would yield.
    #[must_use]
    pub fn selected_text(&self, rows: &[Row]) -> Option<String> {
        let (start, end) = self.selection?;
        let end = end.min(rows.len());
        if start >= end {
            return None;
        }
        Some(
            rows[start..end]
                .iter()
                .map(|row| row.text.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
}

/// Everything the host needs to answer: the islands, their rows, their state.
#[derive(Debug, Clone)]
pub struct IslandHost {
    islands: Islands,
    rows: Vec<Row>,
    state: Vec<IslandState>,
    /// Which island holds focus, by index.
    focused: Option<usize>,
}

impl IslandHost {
    /// A host over `islands`, each with the same rows.
    #[must_use]
    pub fn new(islands: Islands, rows: Vec<Row>) -> Self {
        let count = islands.len();
        Self {
            islands,
            rows,
            state: vec![IslandState::default(); count],
            focused: None,
        }
    }

    /// The islands.
    #[must_use]
    pub const fn islands(&self) -> &Islands {
        &self.islands
    }

    /// The rows every island renders.
    #[must_use]
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// One island's state.
    #[must_use]
    pub fn state(&self, index: usize) -> Option<&IslandState> {
        self.state.get(index)
    }

    /// Which island holds focus.
    #[must_use]
    pub const fn focused(&self) -> Option<usize> {
        self.focused
    }

    /// The row at a content-space point, if any.
    ///
    /// Rows are stacked from the content origin, so a hit test walks until it passes the
    /// point. A row of zero height is skipped rather than dividing by it.
    #[must_use]
    pub fn row_at(&self, content: LogicalPoint) -> Option<usize> {
        let mut y = 0.0;
        for (index, row) in self.rows.iter().enumerate() {
            if row.height > 0.0 && content.y >= y && content.y < y + row.height {
                return Some(index);
            }
            y += row.height;
        }
        None
    }

    /// Routes a pointer event.
    ///
    /// The returned [`Routed::Island`] carries content coordinates, so the renderer never
    /// has to know where the island sits on the page.
    #[must_use]
    pub fn route_pointer(&self, event: &PointerEvent) -> Routed {
        match self.islands.hit_test(event.position) {
            None => Routed::Dom,
            Some(island) => {
                let content = island.content_point(event.position).unwrap_or_default();
                Routed::Island {
                    id: island.id.clone(),
                    content,
                }
            }
        }
    }

    /// Handles a click, updating selection.
    #[must_use]
    pub fn click(&mut self, event: &PointerEvent) -> Routed {
        let routed = self.route_pointer(event);
        let Routed::Island { content, .. } = &routed else {
            // A click on the DOM clears an island's selection: the user acted somewhere
            // else, so a stale highlight is wrong rather than merely untidy.
            for state in &mut self.state {
                state.selection = None;
            }
            return routed;
        };

        let Some(index) = self
            .islands
            .all()
            .iter()
            .position(|i| i.id == routed_id(&routed))
        else {
            return routed;
        };
        if let Some(row) = self.row_at(*content) {
            // Shift extends from wherever the selection started, so a drag-select does not
            // need the anchor tracked separately. `min`/`max` keep the range ordered whichever
            // way the user dragged, because a reversed range is not a selection.
            let anchor = self.state[index].selection.map_or(row, |(start, _)| start);
            self.state[index].selection = if event.shift {
                Some((anchor.min(row), anchor.max(row) + 1))
            } else {
                Some((row, row + 1))
            };
        }
        routed
    }

    /// Applies a scroll to whichever island the point is in.
    #[must_use]
    pub fn scroll(&mut self, event: &PointerEvent) -> Routed {
        let Some(island) = self.islands.hit_test(event.position) else {
            return Routed::Dom;
        };
        let Some(index) = self.islands.all().iter().position(|i| i.id == island.id) else {
            return Routed::Dom;
        };
        let id = island.id.clone();
        let scrolled = island.clone().scrolled_by(event.scroll);
        // `replace` looks the island up by id again; if that lookup and the one above disagree,
        // the scroll would be silently dropped while the caller was told it landed. Refuse
        // instead — a swallowed scroll reads as a dropped frame.
        if self.islands.replace(scrolled).is_err() {
            return Routed::Dom;
        }
        // Focus is single-sourced in `self.focused`; clearing only the per-island flag would
        // leave `focused()` reporting an island the user can no longer actually type into.
        self.state[index].focused = false;
        if self.focused == Some(index) {
            self.focused = None;
        }
        Routed::Island {
            id,
            content: LogicalPoint::default(),
        }
    }

    /// Moves focus to the next island, wrapping.
    #[must_use]
    pub fn focus_next(&mut self) -> Routed {
        self.focus_relative(1)
    }

    /// Moves focus to the previous island, wrapping.
    #[must_use]
    pub fn focus_previous(&mut self) -> Routed {
        self.focus_relative(-1)
    }

    /// Shared focus step, so the two directions cannot disagree.
    fn focus_relative(&mut self, step: i32) -> Routed {
        if self.islands.is_empty() {
            return Routed::Ignored;
        }
        let count = self.islands.len();
        let count_isize = isize::try_from(count).unwrap_or(isize::MAX);
        let step = isize::try_from(step).unwrap_or(0);
        let next = match self.focused {
            None => {
                if step > 0 {
                    0
                } else {
                    count - 1
                }
            }
            Some(current) => {
                let current = isize::try_from(current).unwrap_or(0);
                usize::try_from(current.wrapping_add(step).rem_euclid(count_isize)).unwrap_or(0)
            }
        };
        for (index, state) in self.state.iter_mut().enumerate() {
            state.focused = index == next;
        }
        self.focused = Some(next);
        Routed::Island {
            id: self.islands.all()[next].id.clone(),
            content: LogicalPoint::default(),
        }
    }

    /// Copies the focused island's selection into its clipboard.
    #[must_use]
    pub fn copy(&mut self) -> Option<String> {
        let index = self.focused?;
        let text = self.state[index].selected_text(&self.rows)?;
        self.state[index].clipboard = Some(text.clone());
        Some(text)
    }

    /// Cuts the focused island's selection: copy, then clear it.
    #[must_use]
    pub fn cut(&mut self) -> Option<String> {
        let text = self.copy()?;
        self.state[self.focused?].selection = None;
        Some(text)
    }

    /// Pastes the host's clipboard into the focused island.
    #[must_use]
    pub fn paste(&mut self, text: &str) -> Routed {
        let Some(index) = self.focused else {
            return Routed::Ignored;
        };
        self.state[index].clipboard = Some(text.to_owned());
        Routed::Island {
            id: self.islands.all()[index].id.clone(),
            content: LogicalPoint::default(),
        }
    }

    /// Routes a key press to the focused island.
    ///
    /// A key with no focused island is [`Routed::Dom`], not ignored: the document owns the
    /// keyboard when no island has focus, and swallowing the key would make the page
    /// unresponsive.
    #[must_use]
    pub fn route_key(&mut self, key: Key) -> Routed {
        let Some(index) = self.focused else {
            return Routed::Dom;
        };
        let id = self.islands.all()[index].id.clone();
        match key {
            // The host owns these three even inside an island, so an island cannot trap
            // the user. The island is told, but the host decides.
            Key::Tab | Key::ShiftTab | Key::Escape => Routed::Host,
            // IME: composition accumulates and commits, and a cancel restores. The first
            // keystroke starts the composition — an `if let` on an already-present value made
            // the whole accumulate path unreachable, because nothing else ever created one.
            Key::Character(c) => {
                self.state[index]
                    .composition
                    .get_or_insert_with(String::new)
                    .push(c);
                Routed::Island {
                    id,
                    content: LogicalPoint::default(),
                }
            }
            Key::CompositionCommit => {
                // Only overwrite the clipboard when there is actually a composition to commit.
                // Assigning the `Option` unconditionally made a stray Enter wipe text the user
                // had copied earlier.
                if let Some(committed) = self.state[index].composition.take() {
                    self.state[index].clipboard = Some(committed);
                }
                Routed::Island {
                    id,
                    content: LogicalPoint::default(),
                }
            }
            Key::CompositionCancel => {
                self.state[index].composition = None;
                Routed::Island {
                    id,
                    content: LogicalPoint::default(),
                }
            }
            _ => Routed::Island {
                id,
                content: LogicalPoint::default(),
            },
        }
    }

    /// Routes a drag event.
    #[must_use]
    pub fn route_drag(&self, event: &DragEvent) -> Routed {
        match self.islands.hit_test(event.position) {
            None => Routed::Dom,
            Some(island) => {
                let content = island.content_point(event.position).unwrap_or_default();
                Routed::Island {
                    id: island.id.clone(),
                    content,
                }
            }
        }
    }

    /// The clipboard text a dropped payload carries, if it is text.
    #[must_use]
    pub fn drop_text(&mut self, event: &DragEvent) -> Option<String> {
        let Routed::Island { id, .. } = self.route_drag(event) else {
            return None;
        };
        let index = self.islands.all().iter().position(|i| i.id == id)?;
        let text = match &event.payload {
            DragPayload::Text(text) => text.clone(),
            _ => return None,
        };
        self.state[index].clipboard = Some(text.clone());
        Some(text)
    }

    /// Resizes the window, keeping every island inside the new bounds.
    ///
    /// # Errors
    ///
    /// Returns a reason when an island would fall outside the new window. Clamping it would
    /// be a lie about layout: the DOM would have re-laid out, and the island would be drawn
    /// where the DOM no longer has anything.
    pub fn resize(&mut self, size: LogicalSize) -> Result<(), String> {
        let viewport = Viewport::new(size, self.islands.viewport().scale_factor);
        let mut rebuilt = Islands::new(viewport);
        for island in self.islands.all() {
            rebuilt
                .add(island.clone())
                .map_err(|e| format!("after resize: {e}"))?;
        }
        self.islands = rebuilt;
        Ok(())
    }

    /// Which contract items this host actually satisfies, by *probe* rather than by claim.
    ///
    /// Each item is decided by driving this host and observing an effect that item alone
    /// could produce. An item this build cannot demonstrate is reported absent rather than
    /// assumed present — that is the whole difference between this and returning
    /// `INPUT_CONTRACT` unconditionally, which would pass on a host whose routing is a no-op.
    ///
    /// The probe is destructive (it clicks, scrolls, focuses and resizes a copy), so it runs
    /// against a clone and leaves `self` untouched.
    ///
    /// [`InputContract::HighDpi`] is the one item not driven from this host: it is a property
    /// of the viewport arithmetic, so its probe builds a viewport at a known non-unity scale
    /// rather than reading `self`. Every other item is exercised against the host being asked
    /// about, including Resize, which drives a growth and a refusal.
    #[must_use]
    pub fn satisfied_contracts(&self) -> Vec<InputContract> {
        // Derived from `INPUT_CONTRACT` rather than hand-listed. A second list of eleven had
        // to be kept in agreement with this one, and a dropped item in it would have shown up
        // only as a count that still matched.
        INPUT_CONTRACT
            .iter()
            .filter(|item| self.probes(**item))
            .copied()
            .collect()
    }

    /// Whether every contract item is satisfied.
    ///
    /// **Currently always `false`**, and not because a host is broken: at least one item is
    /// listed as deferred, so the full set cannot be reached by any host until the semantic
    /// tree lands (see the `UNIMPLEMENTED` constant above).
    /// Kept because it is the question a caller will want to ask then, and its answer is the
    /// one [`Self::satisfied_contracts`] reports — but read the list, not this boolean, to know
    /// *what* is missing.
    #[must_use]
    pub fn satisfies_contract(&self) -> bool {
        // Reads the one list rather than re-probing: asking twice could give two different
        // answers for a host whose probes are not pure, and the count is the whole question.
        self.satisfied_contracts().len() == INPUT_CONTRACT.len()
    }

    /// Drives this host for one contract item and reports whether the expected effect landed.
    ///
    /// Each probe asserts an effect that its own item alone could produce, so an
    /// implementation that drops the behaviour fails here rather than passing on a claim.
    ///
    /// `point` lands on a real row (see [`Self::first_row_point`]) so no verdict depends on the
    /// island's *position*. Size is a separate matter: Selection depends on the island being
    /// tall enough to render its second row, which `probe_selection` declares and explains —
    /// a user cannot shift-click into a row that is not on screen. No other item varies with
    /// the island's height.
    fn probes(&self, item: InputContract) -> bool {
        // The single source of truth for what cannot yet be demonstrated. An item listed here is
        // reported unsatisfied without a probe, rather than being "probed" by a hardcoded `false`
        // that no host could ever influence.
        if UNIMPLEMENTED.contains(&item) {
            return false;
        }
        let mut host = self.clone();
        // Probe from a known page state. `content_point` adds the scroll offset, so on a
        // scrolled page the row a probe aims at is off-screen and the point leaves the island
        // entirely — the verdict would then depend on how far the user had scrolled rather than
        // on what the host can do. Scrolling is Scrolling's own probe to exercise.
        host.reset_scroll();
        match item {
            InputContract::Mouse => self
                .first_row_point()
                .is_some_and(|p| probe_mouse(&host, p)),
            InputContract::Keyboard => probe_keyboard(&mut host),
            InputContract::Focus => probe_focus(&mut host),
            InputContract::Scrolling => self
                .first_row_point()
                .is_some_and(|p| probe_scrolling(&mut host, p, 0)),
            InputContract::Clipboard => self
                .first_row_point()
                .is_some_and(|p| probe_clipboard(&mut host, p, 0)),
            InputContract::Selection => self
                .first_row_point()
                .is_some_and(|p| probe_selection(&mut host, p, 0)),
            InputContract::Resize => probe_resize(&mut host),
            // High-DPI is a property of the viewport arithmetic, not of this host's
            // particular window, so its probe builds its own rather than reading `self`.
            InputContract::HighDpi => probe_high_dpi(),
            InputContract::DragAndDrop => self
                .first_row_point()
                .is_some_and(|p| probe_drag_and_drop(&mut host, p)),
            InputContract::Ime => probe_ime(&mut host, 0),
            // Unreachable: every `UNIMPLEMENTED` item returned above. Present only so the
            // match is exhaustive — a new contract item will not compile without a decision.
            InputContract::Accessibility => false,
        }
    }

    /// A window-space point that lands on the **first row** of the first island.
    ///
    /// Derived from the rows, not the island's centre: an island taller than its row stack has
    /// a centre that misses every row, and clicking there would report Selection and Clipboard
    /// unsatisfied on a build where both work — the contract must depend on the implementation,
    /// not on where the island sits on the page.
    ///
    /// Assumes a scroll reset (see [`Self::probes`]), so no scroll offset is applied here.
    fn first_row_point(&self) -> Option<LogicalPoint> {
        let island = self.islands.all().first()?;
        let row = self.rows.first()?;
        // Half a row down, so the point is inside the row rather than on its boundary, and
        // clamped into the island for a row taller than the island.
        let y = island.bounds.y + (row.height / 2.0).min(island.bounds.height / 2.0);
        Some(LogicalPoint::new(
            island.bounds.x + island.bounds.width / 2.0,
            y,
        ))
    }

    /// Returns every island to the top of its content, so a probe's coordinates do not depend
    /// on how far the page has been scrolled.
    fn reset_scroll(&mut self) {
        // A narrow in-place edit, not a mutable accessor: this cannot change an island's
        // identity, bounds or order, so there is nothing to look up and nothing to fail.
        self.islands.scroll_all_to_origin();
    }

    /// The render mode of every island, for the observability ADR-002 requires.
    #[must_use]
    pub fn render_modes(&self) -> Vec<(&str, RenderMode)> {
        self.islands
            .all()
            .iter()
            .map(|island| (island.id.as_str(), island.mode))
            .collect()
    }
}

/// The island id a [`Routed::Island`] carries.
fn routed_id(routed: &Routed) -> &str {
    match routed {
        Routed::Island { id, .. } => id.as_str(),
        _ => "",
    }
}

/// A press at a window-space point, which is what a click and a selection both start from.
fn press(point: LogicalPoint) -> PointerEvent {
    PointerEvent::new(point, PointerPhase::Down)
}

/// [`InputContract::Mouse`]: a point inside an island routes there, and one outside does not.
fn probe_mouse(host: &IslandHost, point: LogicalPoint) -> bool {
    matches!(host.route_pointer(&press(point)), Routed::Island { .. })
        && host.route_pointer(&press(LogicalPoint::new(f32::MAX, f32::MAX))) == Routed::Dom
}

/// A window-space point on the **second** row, for a probe that needs a row different from the
/// first to extend a selection into.
///
/// The second row, not the last: a probe needs *a* second row, and the furthest row is only
/// visible when the whole stack fits inside the island — which for any realistically long list
/// it does not. Anchoring on the last row reported Selection unsatisfied on the crate's own
/// 50-row fixture while a real shift-click demonstrably worked. `None` when there is only one
/// row, because then no range can be widened at all.
fn second_row_point(host: &IslandHost, island: &Island) -> Option<f32> {
    let first = host.rows.first()?;
    let second = host.rows.get(1)?;
    // Rows stack downward from the content origin, so the second row starts one row down.
    // Assumes a scroll reset (see [`IslandHost::probes`]).
    let content_y = first.height + second.height / 2.0;
    Some(island.bounds.y + content_y)
}

/// [`InputContract::Keyboard`]: the three keys an island must not trap stay the host's, and a
/// plain character is handed to the island rather than dropped.
fn probe_keyboard(host: &mut IslandHost) -> bool {
    if !matches!(host.focus_next(), Routed::Island { .. }) {
        return false;
    }
    let untrapped = [Key::Tab, Key::ShiftTab, Key::Escape]
        .iter()
        .all(|key| host.route_key(*key) == Routed::Host);
    untrapped && matches!(host.route_key(Key::Character('x')), Routed::Island { .. })
}

/// [`InputContract::Focus`]: focus moves in both directions and only ever marks one island.
fn probe_focus(host: &mut IslandHost) -> bool {
    let first = host.focus_next();
    let moved = host.focused().is_some();
    let back = host.focus_previous();
    let exclusive = host.state.iter().filter(|s| s.focused).count() <= 1;
    exclusive
        && moved
        && matches!(first, Routed::Island { .. })
        && matches!(back, Routed::Island { .. })
}

/// [`InputContract::Scrolling`]: a scroll is clamped to the island's scrollable range.
///
/// Deliberately *not* "the island moved": an island whose content fits cannot scroll, and
/// reporting that as a missing capability would make the verdict depend on the page's content
/// extent rather than on the implementation. Both ends of the range are checked, so a host that
/// clamps at neither boundary fails.
///
/// A row dependence does remain — the dispatch gates this probe on `first_row_point()`, so a
/// host with no rows reports Scrolling unsatisfied. That is shared with the other row-driven
/// items and is a property of what the host has to offer, not a claim about the implementation.
#[allow(
    clippy::float_cmp,
    reason = "a clamp boundary or DPI conversion off by one ulp is an off-by-one, not float \n              noise; an epsilon here would hide exactly the defect this probe exists to catch"
)]
fn probe_scrolling(host: &mut IslandHost, point: LogicalPoint, island: usize) -> bool {
    let Some(island_ref) = host.islands.all().get(island) else {
        return false;
    };
    let max = island_ref.max_scroll();

    let down = PointerEvent::new(point, PointerPhase::Scroll)
        .scrolling(LogicalPoint::new(0.0, max.y + 10_000.0));
    if !matches!(host.scroll(&down), Routed::Island { .. }) {
        return false;
    }
    let at_bottom = host.islands.all()[island].scroll.y;
    if at_bottom != max.y {
        return false;
    }

    let up =
        PointerEvent::new(point, PointerPhase::Scroll).scrolling(LogicalPoint::new(0.0, -10_000.0));
    if !matches!(host.scroll(&up), Routed::Island { .. }) {
        return false;
    }
    // Clamped at the top edge too, and never negative.
    host.islands.all()[island].scroll.y == 0.0
}

/// [`InputContract::Clipboard`]: copy, cut and paste all round-trip through the island.
fn probe_clipboard(host: &mut IslandHost, point: LogicalPoint, island: usize) -> bool {
    if !matches!(host.focus_next(), Routed::Island { .. }) {
        return false;
    }
    if !matches!(host.click(&press(point)), Routed::Island { .. }) {
        return false;
    }
    if host.state(island).is_none_or(|s| s.selection.is_none()) {
        return false;
    }
    // Copy first, because `cut` clears the selection it would otherwise need.
    let copied = host.copy();
    let cut = host.cut();
    // `paste` reports where the text landed; only an island can accept it.
    copied.is_some() && cut.is_some() && matches!(host.paste("probe"), Routed::Island { .. })
}

/// [`InputContract::Selection`]: a shift-click *widens* the range and keeps it ordered.
///
/// Reports unsatisfied in two measured cases: a host with **no second row** (there is nothing
/// to widen into), and a host **shorter than `rows[0].height + rows[1].height / 2`** (the second
/// row is off-screen, so the user cannot shift-click into it). Both are deliberate: claiming
/// Selection on a host where the gesture is impossible would be the worse lie.
///
/// The extension point has to be a different row from the anchor — shift-clicking the same
/// point yields the same one-row range, which a superset test cannot distinguish from a real
/// extension. It is derived from `rows` rather than from the island's height, because an island
/// taller than its rows has no point 75% down that lands on a row.
fn probe_selection(host: &mut IslandHost, point: LogicalPoint, island: usize) -> bool {
    if !matches!(host.click(&press(point)), Routed::Island { .. }) {
        return false;
    }
    let Some(anchor) = host.state(island).and_then(|s| s.selection) else {
        return false;
    };
    if anchor.1 - anchor.0 != 1 {
        return false;
    }

    // A point on the *second* row, which is the nearest row that differs from the anchor.
    let Some(second) = second_row_point(host, &host.islands.all()[island]) else {
        return false;
    };
    if second <= point.y {
        return false;
    }
    let extended = press(LogicalPoint::new(point.x, second)).with_modifiers(true, false, false);
    if !matches!(host.click(&extended), Routed::Island { .. }) {
        return false;
    }
    let Some(range) = host.state(island).and_then(|s| s.selection) else {
        return false;
    };
    // Strictly wider than the anchor, anchored at the same row, and ordered.
    range.0 <= anchor.0 && range.1 > anchor.1 && range.0 < range.1
}

/// [`InputContract::Resize`]: growing the window keeps every island, and shrinking past one
/// refuses rather than silently dropping it.
///
/// Checking only that growth returns `Ok` cannot fail for the reason the contract cares about:
/// a resize that rebuilt an *empty* page would still pass. So this asserts the islands survive
/// with their identities intact, and drives the refusal branch too.
fn probe_resize(host: &mut IslandHost) -> bool {
    let before: Vec<String> = host.islands.all().iter().map(|i| i.id.clone()).collect();
    let viewport = host.islands.viewport();
    let grown = viewport.size.width * 2.0;
    let taller = viewport.size.height * 2.0;

    if host.resize(LogicalSize::new(grown, taller)).is_err() {
        return false;
    }
    let after: Vec<String> = host.islands.all().iter().map(|i| i.id.clone()).collect();
    if before != after {
        return false;
    }
    // The islands must still be inside the new, larger window.
    let bounds = host.islands.viewport().bounds();
    if !host
        .islands
        .all()
        .iter()
        .all(|i| bounds.contains_rect(&i.bounds))
    {
        return false;
    }

    // The refusal branch: shrinking to nothing cannot hold the islands, and the contract says
    // refuse rather than clamp — a clamped island would be drawn where the DOM no longer is.
    let refused = host.resize(LogicalSize::new(0.0, 0.0)).is_err();
    refused && host.islands.len() == before.len()
}

/// [`InputContract::HighDpi`]: a non-unity scale really scales coordinates.
///
/// Reading the host's stored `scale_factor` cannot fail — `Viewport::new` sanitises it, so it
/// is always finite and positive by construction — and testing a host whose scale happens to
/// be 1.0 cannot fail either, because at 1.0 a conversion that ignores the scale is the
/// identity and still round-trips. So this builds its own viewport at a known non-unity scale
/// and requires the conversion to move a point by that factor and back again.
#[allow(
    clippy::float_cmp,
    reason = "a DPI conversion off by one ulp is an off-by-one, not float noise; an epsilon \
              here would hide exactly the defect this probe exists to catch"
)]
fn probe_high_dpi() -> bool {
    const SCALE: f32 = 2.0;
    let size = LogicalSize::new(800.0, 600.0);
    let viewport = Viewport::new(size, SCALE);

    // The window's physical size is the logical size times the scale.
    let physical = viewport.physical_size();
    if physical.width != size.width * SCALE || physical.height != size.height * SCALE {
        return false;
    }

    // A point must actually move under `to_physical`, and come back under `to_logical`.
    // Checking both directions is what makes this falsifiable at scale 1.0's degenerate twin.
    let origin = LogicalPoint::new(0.0, 0.0);
    let corner = LogicalPoint::new(size.width, size.height);
    for point in [
        origin,
        corner,
        LogicalPoint::new(size.width / 2.0, size.height / 3.0),
    ] {
        let device = viewport.to_physical(point);
        if device.x != point.x * SCALE || device.y != point.y * SCALE {
            return false;
        }
        let back = viewport.to_logical(device);
        if back.x != point.x || back.y != point.y {
            return false;
        }
    }
    true
}

/// [`InputContract::DragAndDrop`]: a text drop inside an island transfers its payload.
fn probe_drag_and_drop(host: &mut IslandHost, point: LogicalPoint) -> bool {
    let event = DragEvent {
        payload: DragPayload::Text("probe".to_owned()),
        position: point,
        dropping: true,
    };
    host.drop_text(&event).is_some()
}

/// [`InputContract::Ime`]: composition accumulates on its own and committing ends it.
fn probe_ime(host: &mut IslandHost, island: usize) -> bool {
    if !matches!(host.focus_next(), Routed::Island { .. }) {
        return false;
    }
    let typed = [Key::Character('a'), Key::Character('b')]
        .iter()
        .all(|key| matches!(host.route_key(*key), Routed::Island { .. }));
    if !typed {
        return false;
    }
    if host.state(island).and_then(|s| s.composition.as_deref()) != Some("ab") {
        return false;
    }
    if !matches!(
        host.route_key(Key::CompositionCommit),
        Routed::Island { .. }
    ) {
        return false;
    }
    let state = host.state(island);
    state.is_some_and(|s| s.composition.is_none())
        && state.and_then(|s| s.clipboard.as_deref()) == Some("ab")
}

/// The rectangles a render pass would draw, in the order it would draw them.
#[must_use]
pub fn draw_order(islands: &Islands) -> Vec<Rect> {
    islands.all().iter().map(|island| island.bounds).collect()
}

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    // Geometry is asserted on exact values here on purpose: a clamp boundary that is off by
    // one ulp is a real off-by-one, not float noise, so an epsilon would hide the defect.
    clippy::float_cmp,
    reason = "test assertions must abort loudly on failure, and geometry is exact"
)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{Island, LogicalPoint, LogicalSize, Rect, Viewport};

    fn rows(n: usize) -> Vec<Row> {
        (0..n)
            .map(|i| Row::new(&format!("row-{i}"), &format!("line {i}"), 20.0))
            .collect()
    }

    fn host() -> IslandHost {
        let viewport = Viewport::new(LogicalSize::new(1000.0, 800.0), 1.0);
        let mut islands = Islands::new(viewport);
        islands
            .add(
                Island::new("grid", Rect::new(0.0, 0.0, 1000.0, 600.0))
                    .with_content(LogicalSize::new(1000.0, 2000.0)),
            )
            .expect("added");
        IslandHost::new(islands, rows(50))
    }

    fn click_at(x: f32, y: f32) -> PointerEvent {
        PointerEvent::new(LogicalPoint::new(x, y), PointerPhase::Down)
    }

    #[test]
    fn the_contract_enumerates_all_eleven_items_adr_002_names() {
        // A count rather than a spot check: an item dropped from the enum would otherwise
        // still report "complete".
        assert_eq!(INPUT_CONTRACT.len(), 11);
        let names: Vec<&str> = INPUT_CONTRACT.iter().map(|c| c.as_str()).collect();
        for required in [
            "mouse",
            "keyboard",
            "focus",
            "scrolling",
            "clipboard",
            "selection",
            "resize",
            "high_dpi",
            "drag_and_drop",
            "ime",
            "accessibility",
        ] {
            assert!(
                names.contains(&required),
                "`{required}` is missing from the contract"
            );
        }
    }

    #[test]
    fn every_deferral_names_a_real_contract_item_exactly_once() {
        // `UNIMPLEMENTED` is the single place a contract item is recorded as not-yet-probed. An
        // entry naming something outside `INPUT_CONTRACT` is inert — it can never defer
        // anything — and an inert entry reads as a deferral that records a decision nobody made.
        // A duplicate is the same defect wearing a different hat.
        //
        // This deliberately does *not* try to check that every other item has a probe. The
        // compiler already does that, unconditionally and without a false-positive mode:
        // `probes`' `match` is exhaustive (removing an arm is E0004), and every `probe_*` is
        // private and called from exactly one arm, so an arm that stopped calling its probe is
        // `dead_code`. An earlier version of this test re-derived both by parsing this file
        // with `include_str!`, and was wrong in both directions: it could pass while an arm was
        // cross-wired to another probe, and it failed on a pure rename of `probes`' parameter.
        // The behaviour a mutation actually changes is already covered by
        // `the_contract_probe_is_driven_by_the_host_and_not_by_a_fixed_list`.
        for item in UNIMPLEMENTED {
            assert!(
                INPUT_CONTRACT.contains(item),
                "UNIMPLEMENTED names {item:?}, which is not in INPUT_CONTRACT: that entry can \
                 never defer anything"
            );
        }
        for item in INPUT_CONTRACT {
            let times = UNIMPLEMENTED.iter().filter(|u| *u == item).count();
            assert!(
                times <= 1,
                "{item:?} appears {times} times in UNIMPLEMENTED"
            );
        }
    }

    #[test]
    fn the_contract_probe_reports_exactly_the_items_the_canonical_host_demonstrates() {
        // A golden vector, and the reason the probes are observed at all: asserting the *exact*
        // set means a probe forced to `return false` drops out of this list and fails. Under
        // negative-only assertions all ten live probes are unobserved, because a probe that
        // answers `true` passes any test that only checks what is missing.
        //
        // Measured coverage, stated precisely because the loose version of it is false. Re-derive
        // with `python tools/probe_coverage.py`, which runs this battery on a scratch copy and
        // names the killing test for each — a figure in a comment that nothing re-measures is
        // just a claim:
        //
        // * Forcing a probe's body to `false` kills this test for all ten.
        // * Forcing a probe's body to `true` kills some test for five — Keyboard, Focus,
        //   Selection, Resize, Ime. It is `the_contract_probe_is_driven_by_...` doing the
        //   killing, not this one.
        //
        //   The other five survive because no fixture in this battery is one where they would
        //   answer `false`. They are reached — the golden vector below calls every one of them,
        //   and the one-row host reaches all five while the row-less host reaches only HighDpi —
        //   and each already answers `true` there. Forcing `true` is a no-op on every host the
        //   suite constructs, so no test can kill it. HighDpi is ungated; the other four are
        //   reached through `first_row_point()` and simply never need to.
        // * Forcing the verdict at the dispatch site instead kills **nine of the ten — all but
        //   HighDpi**, whose arm is ungated and already `true`. (This figure was 10/10 for as
        //   long as a source-parsing guard test also happened to kill that mutation; removing
        //   that test — the compiler already enforces what it checked — put HighDpi back. The
        //   count is the one to re-derive after any change here, not the killer's name: one
        //   mutation commonly fails several tests and the runner reports whichever came first.
        //
        // So: one direction is fully covered, the other is not, and the gap is a property of
        // the harness rather than an oversight. A weaker claim than "every probe is fully
        // mutation-covered", and recorded here rather than left to be assumed.
        assert_eq!(
            host().satisfied_contracts(),
            vec![
                InputContract::Mouse,
                InputContract::Keyboard,
                InputContract::Focus,
                InputContract::Scrolling,
                InputContract::Clipboard,
                InputContract::Selection,
                InputContract::Resize,
                InputContract::HighDpi,
                InputContract::DragAndDrop,
                InputContract::Ime,
            ],
            "the canonical fixture demonstrates ten of eleven; Accessibility is the exception"
        );
    }

    #[test]
    fn a_scrolled_island_reports_the_same_contract_as_an_unscrolled_one() {
        // Scrolled to the very bottom, not by an arbitrary amount. A small scroll merely
        // renumbers the rows the probe aims at, so the verdict would be unchanged whether or
        // not `reset_scroll` ran — this test would pass with the reset deleted. At
        // `max_scroll` the target rows are off the end of the stack entirely, which is what
        // makes the reset load-bearing. Derived from the fixture so it cannot drift.
        let mut scrolled = host();
        let bottom = scrolled.islands().all()[0].max_scroll().y;
        let event = PointerEvent::new(LogicalPoint::new(10.0, 10.0), PointerPhase::Scroll)
            .scrolling(LogicalPoint::new(0.0, bottom));
        // The routing target is not what this test asserts; the state effect below is.
        let _ = scrolled.scroll(&event);
        assert_eq!(
            scrolled.islands().all()[0].scroll.y,
            bottom,
            "the page really is scrolled to the bottom"
        );
        assert_eq!(
            scrolled.satisfied_contracts(),
            host().satisfied_contracts(),
            "scrolling the page must not change what the host can demonstrate"
        );
    }

    #[test]
    fn the_contract_probe_reports_accessibility_unsatisfied_until_the_tree_is_wired() {
        // The honest answer today, and the one that used to be overclaimed: there is no
        // semantic tree in this crate, so Accessibility must not read as satisfied. If this
        // starts failing because someone wired `ncui-a11y` in, that is an improvement — the
        // assertion below should then be the thing that is updated, not this one.
        let satisfied = host().satisfied_contracts();
        assert!(
            !satisfied.contains(&InputContract::Accessibility),
            "no semantic tree is built yet, so Accessibility must not be reported satisfied"
        );
        assert!(
            !host().satisfies_contract(),
            "a host missing one item must not satisfy the contract"
        );
    }

    #[test]
    fn the_contract_probe_is_driven_by_the_host_and_not_by_a_fixed_list() {
        // Two hosts with different shapes must be able to produce different verdicts. If every
        // probe returned a constant, both of these would be identical and this would fail —
        // which is what makes the probe load-bearing rather than a restated `INPUT_CONTRACT`.
        let viewport = Viewport::new(LogicalSize::new(1000.0, 800.0), 1.0);
        let empty = IslandHost::new(Islands::new(viewport), Vec::new());
        // HighDpi is a property of the viewport arithmetic and needs neither an island nor a
        // row, so it is the only item a bare host can demonstrate. Resize needs an island to
        // preserve and refuse on, so an island-less host cannot show it.
        assert_eq!(
            empty.satisfied_contracts(),
            vec![InputContract::HighDpi],
            "only the item needing neither an island nor a row can be satisfied here"
        );
        assert!(!empty.satisfies_contract(), "never the whole contract");

        // A host whose island is much taller than its rows still selects and extends, so the
        // verdict does not depend on the island's *height* — only on it being tall enough to
        // render the second row, which `probe_selection` declares. (A single-row host cannot
        // demonstrate *widening* at all, so two rows is the minimum that can.)
        let viewport = Viewport::new(LogicalSize::new(1000.0, 800.0), 1.0);
        let mut islands = Islands::new(viewport);
        islands
            .add(Island::new("tall", Rect::new(0.0, 0.0, 1000.0, 600.0)))
            .expect("added");
        let tall = IslandHost::new(islands, rows(2));
        let satisfied = tall.satisfied_contracts();
        assert!(
            satisfied.contains(&InputContract::Selection)
                && satisfied.contains(&InputContract::Clipboard),
            "a two-row island in a tall viewport must still select and copy: {satisfied:?}"
        );

        // And a one-row host must report Selection unsatisfied rather than claiming a
        // widening it cannot demonstrate.
        let viewport = Viewport::new(LogicalSize::new(1000.0, 800.0), 1.0);
        let mut islands = Islands::new(viewport);
        islands
            .add(Island::new("one", Rect::new(0.0, 0.0, 1000.0, 600.0)))
            .expect("added");
        let single = IslandHost::new(islands, rows(1));
        assert!(
            !single
                .satisfied_contracts()
                .contains(&InputContract::Selection),
            "with one row there is no range to widen, so Selection is not demonstrable"
        );
        assert!(
            single
                .satisfied_contracts()
                .contains(&InputContract::Clipboard),
            "but a single row is still copyable"
        );
    }

    #[test]
    fn a_pointer_routes_to_the_island_and_arrives_in_content_coordinates() {
        let host = host();
        let event = click_at(100.0, 250.0);
        match host.route_pointer(&event) {
            Routed::Island { id, content } => {
                assert_eq!(id, "grid");
                assert_eq!(content, LogicalPoint::new(100.0, 250.0), "window space");
            }
            other => panic!("expected the island, got {other:?}"),
        }
    }

    #[test]
    fn a_pointer_outside_every_island_goes_to_the_dom() {
        let host = host();
        let event = click_at(100.0, 700.0);
        assert_eq!(host.route_pointer(&event), Routed::Dom, "below the island");
    }

    #[test]
    fn a_click_selects_the_row_under_the_pointer() {
        let mut host = host();
        // Row 3 spans content y in [60, 80).
        // The routing target is not what this test asserts; the state effect below is.
        let _ = host.click(&click_at(10.0, 70.0));
        assert_eq!(host.state(0).expect("state").selection, Some((3, 4)));
    }

    #[test]
    fn shift_click_extends_the_existing_selection() {
        let mut host = host();
        // The routing target is not what this test asserts; the state effect below is.
        let _ = host.click(&click_at(10.0, 10.0));
        assert_eq!(host.state(0).expect("state").selection, Some((0, 1)));

        let extended = PointerEvent::new(LogicalPoint::new(10.0, 70.0), PointerPhase::Down)
            .with_modifiers(true, false, false);
        // The routing target is not what this test asserts; the state effect below is.
        let _ = host.click(&extended);
        assert_eq!(
            host.state(0).expect("state").selection,
            Some((0, 4)),
            "extending backwards from the anchor must not invert the range"
        );
    }

    #[test]
    fn shift_click_upwards_from_a_later_anchor_still_gives_a_ordered_range() {
        let mut host = host();
        // The routing target is not what this test asserts; the state effect below is.
        let _ = host.click(&click_at(10.0, 70.0));
        let extended = PointerEvent::new(LogicalPoint::new(10.0, 10.0), PointerPhase::Down)
            .with_modifiers(true, false, false);
        // The routing target is not what this test asserts; the state effect below is.
        let _ = host.click(&extended);
        let selection = host.state(0).expect("state").selection.expect("selected");
        assert!(
            selection.0 <= selection.1,
            "a range must stay ordered, got {selection:?}"
        );
        assert_eq!(selection, (0, 4));
    }

    #[test]
    fn clicking_the_dom_clears_a_stale_island_selection() {
        let mut host = host();
        // The routing target is not what this test asserts; the state effect below is.
        let _ = host.click(&click_at(10.0, 70.0));
        assert!(host.state(0).expect("state").selection.is_some());
        // The routing target is not what this test asserts; the state effect below is.
        let _ = host.click(&click_at(10.0, 700.0));
        assert!(
            host.state(0).expect("state").selection.is_none(),
            "a selection the user acted elsewhere to dismiss must not persist"
        );
    }

    #[test]
    fn scrolling_moves_the_island_and_clamps_at_the_content_edges() {
        let mut host = host();
        let event = PointerEvent::new(LogicalPoint::new(10.0, 10.0), PointerPhase::Scroll)
            .scrolling(LogicalPoint::new(0.0, 5000.0));
        // The routing target is not what this test asserts; the state effect below is.
        let _ = host.scroll(&event);
        let scrolled = host.islands().all()[0].scroll.y;
        assert_eq!(
            scrolled, 1400.0,
            "clamped to content_height - viewport_height"
        );

        let back = PointerEvent::new(LogicalPoint::new(10.0, 10.0), PointerPhase::Scroll)
            .scrolling(LogicalPoint::new(0.0, -5000.0));
        // The routing target is not what this test asserts; the state effect below is.
        let _ = host.scroll(&back);
        assert_eq!(host.islands().all()[0].scroll.y, 0.0);
    }

    #[test]
    fn scrolling_a_focused_island_drops_focus_in_both_representations() {
        // Focus is stored twice: `host.focused` (the router's index) and `state[i].focused`
        // (the per-island flag). Clearing one without the other leaves the router delivering
        // keys to an island that believes it is unfocused.
        let mut host = host();
        assert!(matches!(host.focus_next(), Routed::Island { .. }));
        assert_eq!(host.focused(), Some(0));

        let event = PointerEvent::new(LogicalPoint::new(10.0, 10.0), PointerPhase::Scroll)
            .scrolling(LogicalPoint::new(0.0, 100.0));
        // The routing target is not what this test asserts; the state effect below is.
        let _ = host.scroll(&event);

        assert_eq!(
            host.focused(),
            None,
            "the router must not keep a dropped focus"
        );
        assert!(
            !host.state(0).expect("state").focused,
            "the per-island flag must be cleared too, or the two representations disagree"
        );
        assert_eq!(
            host.route_key(Key::Character('a')),
            Routed::Dom,
            "keys must follow the cleared focus rather than the stale index"
        );
    }

    #[test]
    fn focus_wraps_in_both_directions_and_marks_exactly_one_island() {
        let viewport = Viewport::new(LogicalSize::new(1000.0, 800.0), 1.0);
        let mut islands = Islands::new(viewport);
        islands
            .add(Island::new("a", Rect::new(0.0, 0.0, 100.0, 100.0)))
            .expect("a");
        islands
            .add(Island::new("b", Rect::new(200.0, 0.0, 100.0, 100.0)))
            .expect("b");
        let mut host = IslandHost::new(islands, rows(5));

        assert!(matches!(host.focus_next(), Routed::Island { .. }));
        assert_eq!(host.focused(), Some(0));
        // The routing target is not what this test asserts; the state effect below is.
        let _ = host.focus_next();
        assert_eq!(host.focused(), Some(1));
        // The routing target is not what this test asserts; the state effect below is.
        let _ = host.focus_next();
        assert_eq!(host.focused(), Some(0), "focus wraps forward");

        // The routing target is not what this test asserts; the state effect below is.
        let _ = host.focus_previous();
        assert_eq!(host.focused(), Some(1), "and wraps backward");

        let focused = host.state.iter().filter(|state| state.focused).count();
        assert_eq!(focused, 1, "exactly one island may hold focus");
    }

    #[test]
    fn a_key_with_no_focused_island_goes_to_the_document() {
        // Swallowing it would make the page unresponsive; the document owns the keyboard
        // when no island has focus.
        let mut host = host();
        assert_eq!(host.route_key(Key::Tab), Routed::Dom);
        assert_eq!(host.route_key(Key::Character('a')), Routed::Dom);
    }

    #[test]
    fn tab_and_escape_are_the_hosts_even_inside_an_island() {
        // The three keys an island must not be able to trap the user with.
        let mut host = host();
        // The routing target is not what this test asserts; the state effect below is.
        let _ = host.focus_next();
        for key in [Key::Tab, Key::ShiftTab, Key::Escape] {
            assert_eq!(
                host.route_key(key),
                Routed::Host,
                "{key:?} must be the host's, or the user is trapped"
            );
        }
    }

    #[test]
    fn ime_composition_accumulates_and_commits() {
        let mut host = host();
        // The routing target is not what this test asserts; the state effect below is.
        let _ = host.focus_next();
        for c in ['n', 'i', 'h', 'a', 'o'] {
            // The routing target is not what this test asserts; the state effect below is.
            let _ = host.route_key(Key::Character(c));
        }
        assert_eq!(
            host.state(0).expect("state").composition.as_deref(),
            Some("nihao"),
            "a composition is the IME's own text, not a keypress per letter"
        );

        // The routing target is not what this test asserts; the state effect below is.
        let _ = host.route_key(Key::CompositionCommit);
        assert_eq!(
            host.state(0).expect("state").composition,
            None,
            "committing ends the composition"
        );
        assert_eq!(
            host.state(0).expect("state").clipboard.as_deref(),
            Some("nihao"),
            "and the committed text is what the island holds"
        );
    }

    #[test]
    fn ime_cancellation_discards_the_composition() {
        let mut host = host();
        // The routing target is not what this test asserts; the state effect below is.
        let _ = host.focus_next();
        // The routing target is not what this test asserts; the state effect below is.
        let _ = host.route_key(Key::Character('a'));
        // The routing target is not what this test asserts; the state effect below is.
        let _ = host.route_key(Key::CompositionCancel);
        assert_eq!(host.state(0).expect("state").composition, None);
        assert_eq!(
            host.state(0).expect("state").clipboard,
            None,
            "nothing committed"
        );
    }

    #[test]
    fn committing_with_no_composition_leaves_an_existing_clipboard_alone() {
        // A stray Enter with nothing in flight used to assign `None` over the clipboard,
        // silently discarding text the user had copied.
        let mut host = host();
        // The routing target is not what this test asserts; the state effect below is.
        let _ = host.focus_next();
        // The routing target is not what this test asserts; the state effect below is.
        let _ = host.paste("earlier");
        assert_eq!(
            host.state(0).expect("state").clipboard.as_deref(),
            Some("earlier")
        );

        // The routing target is not what this test asserts; the state effect below is.
        let _ = host.route_key(Key::CompositionCommit);
        assert_eq!(
            host.state(0).expect("state").clipboard.as_deref(),
            Some("earlier"),
            "a commit with no composition in flight must not wipe the clipboard"
        );
    }

    #[test]
    fn copy_cut_and_paste_round_trip_the_selection() {
        let mut host = host();
        // The routing target is not what this test asserts; the state effect below is.
        let _ = host.click(&click_at(10.0, 10.0));
        let extended = PointerEvent::new(LogicalPoint::new(10.0, 50.0), PointerPhase::Down)
            .with_modifiers(true, false, false);
        // The routing target is not what this test asserts; the state effect below is.
        let _ = host.click(&extended);
        let expected = "line 0\nline 1\nline 2";

        // The routing target is not what this test asserts; the state effect below is.
        let _ = host.focus_next();
        assert_eq!(
            host.copy().as_deref(),
            Some(expected),
            "copy yields the selection"
        );
        assert!(
            host.state(0).expect("state").selection.is_some(),
            "copy keeps it"
        );

        let cut = host.cut();
        assert_eq!(cut.as_deref(), Some(expected));
        assert_eq!(
            host.state(0).expect("state").selection,
            None,
            "cut removes it"
        );

        // The routing target is not what this test asserts; the state effect below is.
        let _ = host.paste("pasted");
        assert_eq!(
            host.state(0).expect("state").clipboard.as_deref(),
            Some("pasted")
        );
    }

    #[test]
    fn a_copy_with_nothing_selected_yields_nothing_rather_than_the_whole_list() {
        let mut host = host();
        // The routing target is not what this test asserts; the state effect below is.
        let _ = host.focus_next();
        assert_eq!(
            host.copy(),
            None,
            "copying nothing must not copy everything"
        );
    }

    #[test]
    fn a_drag_over_an_island_routes_to_it_and_a_drop_transfers_text() {
        let mut host = host();
        let event = DragEvent {
            payload: DragPayload::Text("dragged".to_owned()),
            position: LogicalPoint::new(50.0, 50.0),
            dropping: true,
        };
        assert_eq!(host.drop_text(&event).as_deref(), Some("dragged"));
        assert_eq!(
            host.state(0).expect("state").clipboard.as_deref(),
            Some("dragged")
        );
    }

    #[test]
    fn a_drop_outside_every_island_goes_to_the_dom_and_transfers_nothing() {
        let mut host = host();
        let event = DragEvent {
            payload: DragPayload::Text("dragged".to_owned()),
            position: LogicalPoint::new(10.0, 700.0),
            dropping: true,
        };
        assert_eq!(host.route_drag(&event), Routed::Dom);
        assert_eq!(
            host.drop_text(&event),
            None,
            "a DOM drop must not fill an island"
        );
    }

    #[test]
    fn a_non_text_payload_is_not_pasted_as_text() {
        let mut host = host();
        let event = DragEvent {
            payload: DragPayload::File("C:/secret.txt".to_owned()),
            position: LogicalPoint::new(10.0, 10.0),
            dropping: true,
        };
        assert!(
            host.drop_text(&event).is_none(),
            "a file drop is not text; pretending otherwise would paste a path as content"
        );
    }

    #[test]
    fn resize_that_would_push_an_island_outside_is_refused() {
        let mut host = host();
        // Shrinking below the island's extent would leave it partly outside the window.
        assert!(
            host.resize(LogicalSize::new(100.0, 100.0)).is_err(),
            "a resize leaving an island outside the window must be refused, not clamped"
        );
        // And the host is unchanged after the refusal.
        assert_eq!(
            host.islands().viewport().size,
            LogicalSize::new(1000.0, 800.0)
        );

        // A resize that still contains it succeeds.
        assert!(host.resize(LogicalSize::new(1000.0, 800.0)).is_ok());
    }

    #[test]
    fn high_dpi_is_reported_per_window_rather_than_per_island() {
        let viewport = Viewport::new(LogicalSize::new(1920.0, 1080.0), 2.0);
        let mut islands = Islands::new(viewport);
        islands
            .add(Island::new("a", Rect::new(0.0, 0.0, 960.0, 540.0)))
            .expect("a");
        islands
            .add(Island::new("b", Rect::new(960.0, 0.0, 960.0, 540.0)))
            .expect("b");
        assert_eq!(islands.all().len(), 2);
        assert_eq!(
            islands.viewport().scale_factor,
            2.0,
            "one ratio for the whole window"
        );
    }

    #[test]
    fn render_mode_is_observable_per_island() {
        // ADR-002: "the render_mode of every component is observable, so a bad DOM/GPU split
        // is measurable rather than a matter of opinion".
        let host = host();
        let modes = host.render_modes();
        assert_eq!(modes, vec![("grid", RenderMode::Gpu)]);
    }

    #[test]
    fn draw_order_follows_composition_order() {
        let viewport = Viewport::new(LogicalSize::new(1000.0, 800.0), 1.0);
        let mut islands = Islands::new(viewport);
        islands
            .add(Island::new("back", Rect::new(0.0, 0.0, 100.0, 100.0)))
            .expect("a");
        islands
            .add(Island::new("front", Rect::new(200.0, 0.0, 100.0, 100.0)))
            .expect("b");
        let order = draw_order(&islands);
        assert_eq!(order.len(), 2);
        assert_eq!(order[0], Rect::new(0.0, 0.0, 100.0, 100.0));
        assert_eq!(order[1], Rect::new(200.0, 0.0, 100.0, 100.0));
    }
}
