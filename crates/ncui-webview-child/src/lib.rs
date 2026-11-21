//! The embedded child webview surface (ADR-017 Phase 3).
//!
//! A child webview is a real browser surface that lives inside a parent
//! window's frame, positioned over a rectangle the page chooses. This crate is
//! the *policy* half of that: what a child is, who may create one, where its
//! rectangle may point, and how live children are recorded. The engine half —
//! actually building the surface and moving it — is implemented by a host
//! binding (the wry host, behind its `wry-engine` feature) through the
//! [`ChildWebviewEngine`] seam.
//!
//! # The contract in one paragraph
//!
//! A caller describes a child with a [`ChildWebviewRequest`]: the URL its page
//! starts at, and the rectangle of logical pixels — relative to the parent
//! window's client area — the child should occupy. The registry asks the
//! engine to build it, records the live child, and hands back a
//! [`ChildWebviewHandle`]. Geometry is explicit: the page re-issues the bounds
//! it wants (for example when the parent resizes), and the engine applies
//! them. Nothing here invents a layout.

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
    /// The parent window is not open, so there is nothing to embed into.
    ParentUnavailable { window: WindowId },
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
            Self::ParentUnavailable { .. } => "child_parent_unavailable",
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
            Self::ParentUnavailable { window } => {
                write!(f, "the parent window {window} is not open")
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
    /// Where the child's page starts.
    pub url: String,
    /// The child's rectangle in logical pixels, relative to the parent
    /// window's client area: x, y, width, height.
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl ChildWebviewRequest {
    /// Validates the request against the surface's own rules.
    ///
    /// The URL is bounded, and the rectangle must sit inside the parent's
    /// client area: a child with a negative origin or a zero/oversized extent
    /// is a request the runtime refuses rather than one the engine has to
    /// interpret.
    ///
    /// # Errors
    ///
    /// [`ChildWebviewError::InvalidRequest`] naming the offending field.
    pub fn validate(&self) -> Result<(), ChildWebviewError> {
        if self.url.is_empty() || self.url.len() > MAX_CHILD_URL_BYTES {
            return Err(ChildWebviewError::InvalidRequest {
                field: "url".to_owned(),
                reason: format!("must be a non-empty URL of at most {MAX_CHILD_URL_BYTES} bytes"),
            });
        }
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
    /// Wraps a raw counter value as an identity. The registry mints these.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw counter value, for the registry's own bookkeeping.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
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
}

/// The process-wide record of live child webviews, plus the engine that builds them.
///
/// One registry per runtime. Children are keyed by their own identity; the
/// parent's identity is recorded with them so enumeration can show the tree
/// and so a caller cannot move a child through the wrong parent's handle.
#[derive(Clone)]
pub struct ChildWebviewRegistry {
    engine: Arc<dyn ChildWebviewEngine>,
    children: Arc<Mutex<BTreeMap<ChildWebviewId, ChildWebviewEntry>>>,
    counter: Arc<Mutex<u64>>,
}

impl core::fmt::Debug for ChildWebviewRegistry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let children = self
            .children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f.debug_struct("ChildWebviewRegistry")
            .field("children", &children.keys().collect::<Vec<_>>())
            .field("next_counter", &self.counter)
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
            counter: Arc::new(Mutex::new(0)),
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
        let child = self.next_id();
        self.engine.create_child(parent, request)?;
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
    /// The request is validated the same way creation is, and the child must
    /// still be live: moving a child that has been closed is a refusal, not a
    /// silent no-op.
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
        request.validate()?;
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
                    bounds: request.clone(),
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

    /// Every live child, oldest first.
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

    fn next_id(&self) -> ChildWebviewId {
        let mut counter = self
            .counter
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *counter = counter.wrapping_add(1);
        ChildWebviewId::from_raw(*counter)
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
    use super::*;

    /// A fake engine that records what it was asked, so the policy is tested
    /// without a display.
    #[derive(Debug, Default)]
    struct FakeEngine {
        created: Mutex<Vec<(WindowId, ChildWebviewRequest)>>,
        moved: Mutex<Vec<(u64, ChildWebviewRequest)>>,
        closed: Mutex<Vec<u64>>,
        fail_creates: Mutex<bool>,
    }

    impl ChildWebviewEngine for FakeEngine {
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
                .created
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len() as u64
                + 1;
            self.created
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((parent, request.clone()));
            Ok(id)
        }

        fn set_child_bounds(
            &self,
            _parent: WindowId,
            child: u64,
            request: &ChildWebviewRequest,
        ) -> std::result::Result<(), ChildWebviewError> {
            self.moved
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((child, request.clone()));
            Ok(())
        }

        fn close_child(
            &self,
            _parent: WindowId,
            child: u64,
        ) -> std::result::Result<(), ChildWebviewError> {
            self.closed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(child);
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

    fn parent_id() -> WindowId {
        "11111111111111111111111111111111"
            .parse()
            .expect("window id")
    }

    #[test]
    fn a_valid_request_creates_a_child_and_records_it() {
        let registry = ChildWebviewRegistry::new(Arc::new(FakeEngine::default()));
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
    fn a_request_outside_the_client_area_is_refused_before_the_engine_is_asked() {
        let registry = ChildWebviewRegistry::new(Arc::new(FakeEngine::default()));
        let error = registry
            .create(parent_id(), &request_at(-1, 0, 400, 300))
            .expect_err("negative origin refused");
        assert_eq!(error.code(), "child_request_invalid");
        assert!(registry.list().is_empty(), "nothing was created");
    }

    #[test]
    fn an_empty_or_oversized_extent_is_refused() {
        let registry = ChildWebviewRegistry::new(Arc::new(FakeEngine::default()));
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
        let registry = ChildWebviewRegistry::new(Arc::new(FakeEngine::default()));
        let mut request = request_at(0, 0, 400, 300);
        request.url = " ".repeat(MAX_CHILD_URL_BYTES + 1);
        let error = registry
            .create(parent_id(), &request)
            .expect_err("long url refused");
        assert!(error.to_string().contains("url"));
    }

    #[test]
    fn set_bounds_moves_a_live_child_and_is_recorded() {
        let registry = ChildWebviewRegistry::new(Arc::new(FakeEngine::default()));
        let handle = registry
            .create(parent_id(), &request_at(0, 0, 400, 300))
            .expect("created");
        handle
            .set_bounds(&request_at(40, 50, 640, 480))
            .expect("moved");
        let entry = registry.get(handle.id).expect("still live");
        assert_eq!(entry.bounds.x, 40);
        assert_eq!(entry.bounds.height, 480);
    }

    #[test]
    fn moving_or_closing_an_unknown_child_is_a_refusal() {
        let registry = ChildWebviewRegistry::new(Arc::new(FakeEngine::default()));
        let unknown = ChildWebviewId::from_raw(999);
        let error = registry
            .set_bounds(unknown, &request_at(0, 0, 400, 300))
            .expect_err("unknown child");
        assert_eq!(error.code(), "child_engine_failed");
        assert!(registry.close(unknown).is_err());
    }

    #[test]
    fn closing_a_child_removes_it_from_the_registry() {
        let registry = ChildWebviewRegistry::new(Arc::new(FakeEngine::default()));
        let handle = registry
            .create(parent_id(), &request_at(0, 0, 400, 300))
            .expect("created");
        handle.close().expect("closed");
        assert!(registry.get(handle.id).is_none(), "no longer live");
    }

    #[test]
    fn an_engine_failure_is_reported_under_its_own_code() {
        let engine = FakeEngine::default();
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
