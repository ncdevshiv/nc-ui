//! The embedded child webview surface (ADR-017 Phase 3).
//!
//! A child webview is a real browser surface that lives inside a parent
//! window's frame, positioned over a rectangle the page chooses. This crate is
//! the *policy* half of that: what a child is, who may create one, where its
//! rectangle may point. Production live state belongs to the parent `Window`;
//! this crate supplies its engine-neutral DTOs and validation. The engine half —
//! actually building the surface and moving it — is implemented by the wry host
//! behind its `wry-engine` feature.
//!
//! # The contract in one paragraph
//!
//! A caller describes a child with a [`ChildWebviewRequest`]: the URL its page
//! starts at, and the rectangle of logical pixels relative to the parent window's
//! client area. The parent `Window` records the engine-issued identity and owns
//! the live child. Geometry is explicit: the page re-issues the bounds it wants,
//! and the engine applies them. Nothing here invents a layout.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use ncui_core::{Result, WindowId};

/// The longest URL a child's page may start at.
///
/// A URL is bounded for the same reason every string that crosses the seam is
/// bounded: a four-megabyte "URL" is an allocation the caller controls, not a
/// page address.
pub const MAX_CHILD_URL_BYTES: usize = 2048;

/// The lowest x or y a child's rectangle may name.
///
/// A child lives inside its parent's client area; a negative coordinate would
/// put it outside the window it belongs to, which is a different feature this
/// surface does not offer.
pub const MIN_CHILD_COORDINATE: i32 = 0;

/// The largest width or height a child's rectangle may name, in logical pixels.
pub const MAX_CHILD_EXTENT: u32 = 16_384;

/// Why a child webview could not be created or driven.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildWebviewError {
    /// The parent window's engine has no child surface — a headless backend,
    /// or a build whose binding does not implement embedding.
    NotSupported { reason: String },
    /// The parent engine surface is unavailable, so there is nothing to embed into.
    ParentUnavailable,
    /// The request was malformed: a URL over the bound, or a rectangle outside
    /// the parent's client area.
    InvalidRequest { field: String, reason: String },
    /// The engine refused the operation (a platform failure building or
    /// moving the surface).
    EngineFailed { reason: String },
}

impl ChildWebviewError {
    /// The stable wire name, e.g. `"child_not_supported"`, used in refusals.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotSupported { .. } => "child_not_supported",
            Self::ParentUnavailable => "child_parent_unavailable",
            Self::InvalidRequest { .. } => "child_request_invalid",
            Self::EngineFailed { .. } => "child_engine_failed",
        }
    }
}

impl core::fmt::Display for ChildWebviewError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotSupported { reason } => {
                write!(f, "child webviews are not supported here: {reason}")
            }
            Self::ParentUnavailable => {
                write!(f, "the parent engine surface is unavailable")
            }
            Self::InvalidRequest { field, reason } => {
                write!(f, "invalid child webview `{field}`: {reason}")
            }
            Self::EngineFailed { reason } => {
                write!(f, "the engine refused the child webview: {reason}")
            }
        }
    }
}

/// A request to embed one child webview into a parent window.
///
/// Everything is explicit: a child that starts at a URL the caller did not
/// name, occupying a rectangle the caller did not choose, would be a surface
/// the runtime invented rather than one the application asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildWebviewRequest {
    /// Where the child's page starts. May be empty for a bounds-only update.
    pub url: String,
    /// The child's rectangle in logical pixels, relative to the parent
    /// window's client area: x, y, width, height.
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl ChildWebviewRequest {
    /// Validates both the URL and geometry for a creation request.
    ///
    /// The URL is bounded, and the rectangle must sit inside the parent's
    /// client area: a child with a negative origin or a zero/oversized extent
    /// is a request the runtime refuses rather than one the engine has to
    /// interpret. Bounds-only updates can use [`Self::validate_bounds`] instead,
    /// leaving the URL empty.
    ///
    /// # Errors
    ///
    /// [`ChildWebviewError::InvalidRequest`] naming the offending field.
    pub fn validate(&self) -> Result<(), ChildWebviewError> {
        self.validate_url()?;
        self.validate_bounds()
    }

    /// Validates only the child's starting URL.
    ///
    /// # Errors
    ///
    /// [`ChildWebviewError::InvalidRequest`] when the URL is empty or over its
    /// byte bound.
    pub fn validate_url(&self) -> Result<(), ChildWebviewError> {
        if self.url.is_empty() || self.url.len() > MAX_CHILD_URL_BYTES {
            return Err(ChildWebviewError::InvalidRequest {
                field: "url".to_owned(),
                reason: format!("must be a non-empty URL of at most {MAX_CHILD_URL_BYTES} bytes"),
            });
        }
        Ok(())
    }

    /// Validates only the child's rectangle geometry.
    ///
    /// # Errors
    ///
    /// [`ChildWebviewError::InvalidRequest`] naming the offending geometry
    /// field.
    pub fn validate_bounds(&self) -> Result<(), ChildWebviewError> {
        if self.x < MIN_CHILD_COORDINATE || self.y < MIN_CHILD_COORDINATE {
            return Err(ChildWebviewError::InvalidRequest {
                field: "x,y".to_owned(),
                reason:
                    "the rectangle must sit inside the parent's client area (no negative origin)"
                        .to_owned(),
            });
        }
        if self.width == 0 || self.height == 0 {
            return Err(ChildWebviewError::InvalidRequest {
                field: "width,height".to_owned(),
                reason: "the rectangle must have a non-zero extent".to_owned(),
            });
        }
        if self.width > MAX_CHILD_EXTENT || self.height > MAX_CHILD_EXTENT {
            return Err(ChildWebviewError::InvalidRequest {
                field: "width,height".to_owned(),
                reason: format!("the extent must be at most {MAX_CHILD_EXTENT} logical pixels"),
            });
        }
        Ok(())
    }
}

/// The identity of one live child webview.
///
/// Distinct from the parent's [`WindowId`]: a child is addressable in its own
/// right (a page asks to move *this* child), and a child that is dropped must
/// not be confused with its parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ChildWebviewId(u64);

impl ChildWebviewId {
    /// Wraps the raw identity issued by the child webview engine.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// The engine-issued raw identity used to address the child.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Parses the canonical `child-` plus sixteen-hex-digit wire identity.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let digits = text.strip_prefix("child-").filter(|digits| {
            digits.len() == 16 && digits.bytes().all(|byte| byte.is_ascii_hexdigit())
        })?;
        u64::from_str_radix(digits, 16).ok().map(Self)
    }
}

impl core::fmt::Display for ChildWebviewId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "child-{:016x}", self.0)
    }
}

/// One live child webview, as the registry records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildWebviewEntry {
    /// The child's identity.
    pub id: ChildWebviewId,
    /// The parent window the child is embedded in.
    pub parent: WindowId,
    /// The rectangle the child currently occupies, as it was last applied.
    pub bounds: ChildWebviewRequest,
}

/// The engine half of the surface: building and moving a real child webview.
///
/// Implemented by a host binding (the wry host, behind `wry-engine`). The
/// registry calls it; the policy stays here.
pub trait ChildWebviewEngine: Send + Sync {
    /// Builds one child webview inside `parent`'s frame.
    ///
    /// `Ok` returns the engine's own handle for the child, which the registry
    /// stores so later operations reach the same surface.
    fn create_child(
        &self,
        parent: WindowId,
        request: &ChildWebviewRequest,
    ) -> std::result::Result<u64, ChildWebviewError>;

    /// Applies a new rectangle to a live child.
    fn set_child_bounds(
        &self,
        parent: WindowId,
        child: u64,
        request: &ChildWebviewRequest,
    ) -> std::result::Result<(), ChildWebviewError>;

    /// Closes a live child, dropping its surface.
    fn close_child(
        &self,
        parent: WindowId,
        child: u64,
    ) -> std::result::Result<(), ChildWebviewError>;

    /// Every live child this engine holds.
    fn list_children(&self) -> Vec<ChildWebviewEntry> {
        Vec::new()
    }
}

/// An explicit record of live child webviews, plus the engine that builds them.
///
/// Engine-neutral registry utility; the embedded runtime's live authority is the parent `Window`.
#[derive(Clone)]
pub struct ChildWebviewRegistry {
    engine: Arc<dyn ChildWebviewEngine>,
    children: Arc<Mutex<BTreeMap<ChildWebviewId, ChildWebviewEntry>>>,
}

impl core::fmt::Debug for ChildWebviewRegistry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let children = self
            .children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f.debug_struct("ChildWebviewRegistry")
            .field("children", &children.keys().collect::<Vec<_>>())
            .field("engine", &"dyn ChildWebviewEngine")
            .finish()
    }
}

impl ChildWebviewRegistry {
    /// Creates a registry whose children are built by `engine`.
    #[must_use]
    pub fn new(engine: Arc<dyn ChildWebviewEngine>) -> Self {
        Self {
            engine,
            children: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Embeds a child webview into `parent`, validating the request first.
    ///
    /// The child inherits nothing from the page automatically — its navigation
    /// and grants are the engine's seam decision — but the request that names
    /// it is checked here, where the rules are plain Rust and testable
    /// without a display.
    ///
    /// # Errors
    ///
    /// [`ChildWebviewError::InvalidRequest`] for a malformed request,
    /// [`ChildWebviewError::NotSupported`] when the engine has no child
    /// surface, and [`ChildWebviewError::EngineFailed`] when the platform
    /// refuses the build.
    pub fn create(
        &self,
        parent: WindowId,
        request: &ChildWebviewRequest,
    ) -> std::result::Result<ChildWebviewHandle, ChildWebviewError> {
        request.validate()?;
        let child = ChildWebviewId::from_raw(self.engine.create_child(parent, request)?);
        let entry = ChildWebviewEntry {
            id: child,
            parent,
            bounds: request.clone(),
        };
        self.children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(child, entry);
        Ok(ChildWebviewHandle {
            registry: self.clone(),
            id: child,
            parent,
        })
    }

    /// Applies a new rectangle to a live child.
    ///
    /// Only the request's geometry is validated, so a bounds-only update may
    /// leave its URL empty. The child must still be live: moving a child that
    /// has been closed is a refusal, not a silent no-op.
    ///
    /// # Errors
    ///
    /// The same refusals [`ChildWebviewRegistry::create`] produces, plus
    /// [`ChildWebviewError::EngineFailed`] when the engine cannot move the
    /// surface.
    pub fn set_bounds(
        &self,
        child: ChildWebviewId,
        request: &ChildWebviewRequest,
    ) -> std::result::Result<(), ChildWebviewError> {
        request.validate_bounds()?;
        let entry = self.live_child(child)?;
        self.engine
            .set_child_bounds(entry.parent, child.raw(), request)?;
        self.children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                child,
                ChildWebviewEntry {
                    id: child,
                    parent: entry.parent,
                    bounds: ChildWebviewRequest {
                        url: entry.bounds.url,
                        ..request.clone()
                    },
                },
            );
        Ok(())
    }

    /// Closes a live child and removes it from the registry.
    ///
    /// Closing a child that is already gone is a refusal, because "the child
    /// you named does not exist" and "it is already closed" are different
    /// facts a caller may need to distinguish.
    ///
    /// # Errors
    ///
    /// [`ChildWebviewError::EngineFailed`] when the engine could not drop the
    /// surface, or the child is unknown.
    pub fn close(&self, child: ChildWebviewId) -> std::result::Result<(), ChildWebviewError> {
        let entry = self.live_child(child)?;
        self.engine.close_child(entry.parent, child.raw())?;
        self.children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&child);
        Ok(())
    }

    /// Every live child.
    #[must_use]
    pub fn list(&self) -> Vec<ChildWebviewEntry> {
        self.children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    /// One live child, or `None` when it is not live.
    #[must_use]
    pub fn get(&self, child: ChildWebviewId) -> Option<ChildWebviewEntry> {
        self.children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&child)
            .cloned()
    }

    fn live_child(
        &self,
        child: ChildWebviewId,
    ) -> std::result::Result<ChildWebviewEntry, ChildWebviewError> {
        self.children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&child)
            .cloned()
            .ok_or_else(|| ChildWebviewError::EngineFailed {
                reason: format!("child webview {child} is not live"),
            })
    }
}

/// A handle to one live child webview, carrying its identity and parent.
///
/// The handle is a typed convenience over the registry: operations on it
/// affect the same live child the registry records. Dropping the handle does
/// not close the child — closing is explicit, because a page that still holds
/// a handle should not lose its surface just because a wrapper went out of
/// scope.
#[derive(Debug, Clone)]
pub struct ChildWebviewHandle {
    registry: ChildWebviewRegistry,
    /// The child's identity.
    pub id: ChildWebviewId,
    /// The parent window the child is embedded in.
    pub parent: WindowId,
}

impl ChildWebviewHandle {
    /// Applies a new rectangle to this child.
    ///
    /// # Errors
    ///
    /// The same refusals [`ChildWebviewRegistry::set_bounds`] produces.
    pub fn set_bounds(
        &self,
        request: &ChildWebviewRequest,
    ) -> std::result::Result<(), ChildWebviewError> {
        self.registry.set_bounds(self.id, request)
    }

    /// Closes this child.
    ///
    /// # Errors
    ///
    /// The same refusals [`ChildWebviewRegistry::close`] produces.
    pub fn close(&self) -> std::result::Result<(), ChildWebviewError> {
        self.registry.close(self.id)
    }

    /// The child's current record, as the registry holds it.
    #[must_use]
    pub fn entry(&self) -> Option<ChildWebviewEntry> {
        self.registry.get(self.id)
    }
}

/// A no-engine registry, for tests and for builds without a display: every
/// operation refuses with [`ChildWebviewError::NotSupported`], which is the
/// honest answer of a build that cannot embed a child at all.
#[derive(Debug)]
pub struct HeadlessChildWebviewEngine;

impl ChildWebviewEngine for HeadlessChildWebviewEngine {
    fn create_child(
        &self,
        _parent: WindowId,
        _request: &ChildWebviewRequest,
    ) -> std::result::Result<u64, ChildWebviewError> {
        Err(ChildWebviewError::NotSupported {
            reason: "this build has no window engine, so it cannot embed a child webview"
                .to_owned(),
        })
    }

    fn set_child_bounds(
        &self,
        _parent: WindowId,
        _child: u64,
        _request: &ChildWebviewRequest,
    ) -> std::result::Result<(), ChildWebviewError> {
        Err(ChildWebviewError::NotSupported {
            reason: "this build has no window engine, so it cannot move a child webview".to_owned(),
        })
    }

    fn close_child(
        &self,
        _parent: WindowId,
        _child: u64,
    ) -> std::result::Result<(), ChildWebviewError> {
        Err(ChildWebviewError::NotSupported {
            reason: "this build has no window engine, so it has no child webview to close"
                .to_owned(),
        })
    }
}

/// Convenience constructor: a registry over the headless engine.
#[must_use]
pub fn headless_registry() -> ChildWebviewRegistry {
    ChildWebviewRegistry::new(Arc::new(HeadlessChildWebviewEngine))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test assertions must abort loudly on failure"
)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    /// A recording engine: it answers the seam's calls and keeps what it was
    /// asked, so the policy is tested without a display.
    #[derive(Debug, Default)]
    struct RecordingEngine {
        created: Mutex<Vec<(WindowId, ChildWebviewRequest)>>,
        moved: Mutex<Vec<(WindowId, u64, ChildWebviewRequest)>>,
        closed: Mutex<Vec<(WindowId, u64)>>,
        fail_creates: Mutex<bool>,
        issued_ids: Mutex<VecDeque<u64>>,
    }

    impl ChildWebviewEngine for RecordingEngine {
        fn create_child(
            &self,
            parent: WindowId,
            request: &ChildWebviewRequest,
        ) -> std::result::Result<u64, ChildWebviewError> {
            if *self
                .fail_creates
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
            {
                return Err(ChildWebviewError::EngineFailed {
                    reason: "the platform refused the build".to_owned(),
                });
            }
            let id = self
                .issued_ids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
                .unwrap_or_else(|| {
                    self.created
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .len() as u64
                        + 1
                });
            self.created
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((parent, request.clone()));
            Ok(id)
        }

        fn set_child_bounds(
            &self,
            parent: WindowId,
            child: u64,
            request: &ChildWebviewRequest,
        ) -> std::result::Result<(), ChildWebviewError> {
            self.moved
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((parent, child, request.clone()));
            Ok(())
        }

        fn close_child(
            &self,
            parent: WindowId,
            child: u64,
        ) -> std::result::Result<(), ChildWebviewError> {
            self.closed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((parent, child));
            Ok(())
        }
    }

    fn request_at(x: i32, y: i32, width: u32, height: u32) -> ChildWebviewRequest {
        ChildWebviewRequest {
            url: "https://example.com".to_owned(),
            x,
            y,
            width,
            height,
        }
    }

    fn bounds_at(x: i32, y: i32, width: u32, height: u32) -> ChildWebviewRequest {
        let mut request = request_at(x, y, width, height);
        request.url.clear();
        request
    }

    fn parent_id() -> WindowId {
        "11111111111111111111111111111111"
            .parse()
            .expect("window id")
    }

    #[test]
    fn child_ids_round_trip_through_their_canonical_wire_form() {
        for raw in [0, 1, 0x0123_4567_89ab_cdef, u64::MAX] {
            let id = ChildWebviewId::from_raw(raw);
            let text = id.to_string();
            assert_eq!(ChildWebviewId::parse(&text), Some(id));
        }
        for malformed in [
            "",
            "1",
            "child-1",
            "child-00000000000000001",
            "child-000000000000000g",
            "child-+000000000000001",
            "0000000000000001",
        ] {
            assert_eq!(ChildWebviewId::parse(malformed), None, "{malformed}");
        }
    }

    #[test]
    fn a_valid_request_creates_a_child_and_records_it() {
        let registry = ChildWebviewRegistry::new(Arc::new(RecordingEngine::default()));
        let parent = parent_id();
        let handle = registry
            .create(parent, &request_at(10, 20, 400, 300))
            .expect("a valid request creates");
        assert_eq!(handle.parent, parent);
        let entry = registry.get(handle.id).expect("recorded");
        assert_eq!(entry.bounds.width, 400);
        assert_eq!(registry.list().len(), 1);
    }

    #[test]
    fn failed_and_interleaved_creations_preserve_engine_issued_ids() {
        let engine = Arc::new(RecordingEngine::default());
        engine
            .issued_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend([0, u64::MAX]);
        let registry = ChildWebviewRegistry::new(engine.clone());
        let first_parent = parent_id();
        let second_parent = "22222222222222222222222222222222"
            .parse()
            .expect("window id");

        let first = registry
            .create(first_parent, &request_at(10, 20, 400, 300))
            .expect("first creation succeeds");
        assert_eq!(first.id, ChildWebviewId::from_raw(0));
        assert_eq!(first.parent, first_parent);

        *engine
            .fail_creates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        let error = registry
            .create(second_parent, &request_at(30, 40, 500, 350))
            .expect_err("interleaved creation fails");
        assert_eq!(error.code(), "child_engine_failed");
        *engine
            .fail_creates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = false;

        let second = registry
            .create(second_parent, &request_at(30, 40, 500, 350))
            .expect("creation after the failure succeeds");
        assert_eq!(second.id, ChildWebviewId::from_raw(u64::MAX));
        assert_eq!(second.parent, second_parent);
        assert_eq!(
            registry
                .list()
                .into_iter()
                .map(|entry| (entry.id, entry.parent))
                .collect::<Vec<_>>(),
            vec![
                (ChildWebviewId::from_raw(0), first_parent),
                (ChildWebviewId::from_raw(u64::MAX), second_parent),
            ]
        );
        assert_eq!(
            engine
                .created
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            2
        );
    }

    #[test]
    fn interleaved_bounds_and_close_use_engine_issued_ids() {
        const FIRST_ID: u64 = 0x0102_0304_0506_0708;
        const SECOND_ID: u64 = 0xfedc_ba98_7654_3210;
        let engine = Arc::new(RecordingEngine::default());
        engine
            .issued_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend([FIRST_ID, SECOND_ID]);
        let registry = ChildWebviewRegistry::new(engine.clone());
        let first_parent = parent_id();
        let second_parent = "22222222222222222222222222222222"
            .parse()
            .expect("window id");
        let first = registry
            .create(first_parent, &request_at(0, 0, 400, 300))
            .expect("first creation succeeds");
        let second = registry
            .create(second_parent, &request_at(0, 0, 500, 350))
            .expect("second creation succeeds");
        let first_bounds = request_at(40, 50, 640, 480);
        let second_bounds = request_at(60, 70, 720, 540);

        first.set_bounds(&first_bounds).expect("first child moves");
        second
            .set_bounds(&second_bounds)
            .expect("second child moves");
        second.close().expect("second child closes");
        first.close().expect("first child closes");

        assert_eq!(
            *engine
                .moved
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec![
                (first_parent, FIRST_ID, first_bounds),
                (second_parent, SECOND_ID, second_bounds),
            ]
        );
        assert_eq!(
            *engine
                .closed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec![(second_parent, SECOND_ID), (first_parent, FIRST_ID)]
        );
        assert!(registry.list().is_empty());
    }

    #[test]
    fn a_request_outside_the_client_area_is_refused_before_the_engine_is_asked() {
        let registry = ChildWebviewRegistry::new(Arc::new(RecordingEngine::default()));
        let error = registry
            .create(parent_id(), &request_at(-1, 0, 400, 300))
            .expect_err("negative origin refused");
        assert_eq!(error.code(), "child_request_invalid");
        assert!(registry.list().is_empty(), "nothing was created");
    }

    #[test]
    fn an_empty_or_oversized_extent_is_refused() {
        let registry = ChildWebviewRegistry::new(Arc::new(RecordingEngine::default()));
        for request in [
            request_at(0, 0, 0, 300),
            request_at(0, 0, 400, 0),
            request_at(0, 0, u32::MAX, 300),
        ] {
            let error = registry
                .create(parent_id(), &request)
                .expect_err("extent refused");
            assert_eq!(error.code(), "child_request_invalid");
        }
    }

    #[test]
    fn a_boundless_url_is_refused() {
        let registry = ChildWebviewRegistry::new(Arc::new(RecordingEngine::default()));
        let mut request = request_at(0, 0, 400, 300);
        request.url = " ".repeat(MAX_CHILD_URL_BYTES + 1);
        let error = registry
            .create(parent_id(), &request)
            .expect_err("long url refused");
        assert!(error.to_string().contains("url"));
    }

    #[test]
    fn url_and_bounds_validation_are_independent() {
        let request = bounds_at(0, 0, 400, 300);
        let error = request
            .validate_url()
            .expect_err("bounds-only request has no URL");
        assert_eq!(error.code(), "child_request_invalid");
        assert!(error.to_string().contains("url"));
        request
            .validate_bounds()
            .expect("bounds-only geometry is valid");
        assert!(request.validate().is_err(), "creation still needs a URL");

        let mut invalid_bounds = request;
        invalid_bounds.x = -1;
        let error = invalid_bounds
            .validate_bounds()
            .expect_err("negative origin is invalid");
        assert_eq!(error.code(), "child_request_invalid");
        assert!(error.to_string().contains("x,y"));
    }

    #[test]
    fn creation_validates_both_url_and_bounds() {
        let engine = Arc::new(RecordingEngine::default());
        let registry = ChildWebviewRegistry::new(engine.clone());
        let error = registry
            .create(parent_id(), &bounds_at(0, 0, 400, 300))
            .expect_err("creation requires a URL");
        assert_eq!(error.code(), "child_request_invalid");
        assert!(error.to_string().contains("url"));
        assert!(
            engine
                .created
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "invalid creation never reached the engine"
        );
    }

    #[test]
    fn set_bounds_moves_a_live_child_with_only_bounds_validation() {
        let engine = Arc::new(RecordingEngine::default());
        let registry = ChildWebviewRegistry::new(engine.clone());
        let parent = parent_id();
        let created = request_at(0, 0, 400, 300);
        let handle = registry.create(parent, &created).expect("created");
        let bounds = bounds_at(40, 50, 640, 480);
        handle.set_bounds(&bounds).expect("moved without a URL");
        let entry = registry.get(handle.id).expect("still live");
        assert_eq!(
            entry.bounds,
            ChildWebviewRequest {
                url: created.url,
                ..bounds
            }
        );
        assert_eq!(
            *engine
                .moved
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec![(parent, handle.id.raw(), bounds)]
        );

        let error = handle
            .set_bounds(&bounds_at(-1, 0, 640, 480))
            .expect_err("bounds-only update still validates geometry");
        assert_eq!(error.code(), "child_request_invalid");
        assert_eq!(
            engine
                .moved
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1,
            "invalid geometry never reached the engine"
        );
    }

    #[test]
    fn moving_or_closing_an_unknown_child_is_a_refusal() {
        let registry = ChildWebviewRegistry::new(Arc::new(RecordingEngine::default()));
        let unknown = ChildWebviewId::from_raw(999);
        let error = registry
            .set_bounds(unknown, &request_at(0, 0, 400, 300))
            .expect_err("unknown child");
        assert_eq!(error.code(), "child_engine_failed");
        assert!(registry.close(unknown).is_err());
    }

    #[test]
    fn closing_a_child_removes_it_from_the_registry() {
        let registry = ChildWebviewRegistry::new(Arc::new(RecordingEngine::default()));
        let handle = registry
            .create(parent_id(), &request_at(0, 0, 400, 300))
            .expect("created");
        handle.close().expect("closed");
        assert!(registry.get(handle.id).is_none(), "no longer live");
    }

    #[test]
    fn an_engine_failure_is_reported_under_its_own_code() {
        let engine = RecordingEngine::default();
        *engine
            .fail_creates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        let registry = ChildWebviewRegistry::new(Arc::new(engine));
        let error = registry
            .create(parent_id(), &request_at(0, 0, 400, 300))
            .expect_err("engine refused");
        assert_eq!(error.code(), "child_engine_failed");
    }

    #[test]
    fn the_headless_engine_refuses_everything_by_name() {
        let registry = headless_registry();
        let error = registry
            .create(parent_id(), &request_at(0, 0, 400, 300))
            .expect_err("headless build cannot embed");
        assert_eq!(error.code(), "child_not_supported");
    }
}
