//! The one way a component can ask the host to do real work.
//!
//! This crate deliberately does not know what a service *does*. It knows that a component asks,
//! that the manifest had to declare the asking, and that the negotiation had to grant the
//! capability behind it — and then it calls through this trait and lets the caller decide. The
//! runtime implements it against its command table; a test implements it against a `Vec` and an
//! assertion.
//!
//! The trait is per component, not global: `ComponentGateway` implementations are constructed
//! with the grant and the identity of the component they serve already closed over, so an
//! implementation cannot be tricked into acting for a different component than the one that
//! asked.

use ncui_core::Capability;
use serde_json::Value;

/// What a component may reach through `ncui::host_call`.
pub trait ComponentGateway: Send + Sync {
    /// Runs `service` with `input`.
    ///
    /// `capability` is the one [`ncui_abi::negotiate`] attached to this service when the
    /// component declared it, and it is passed through so a gateway can audit the same fact the
    /// gate used rather than re-deriving it and possibly deriving something different.
    ///
    /// An `Err` is a refusal or a failure, and the component sees it as
    /// [`crate::contract::REFUSAL_HOST_ERROR`] — the message is kept host-side for the logs,
    /// not handed to the artifact that provoked it.
    fn call(
        &self,
        service: &str,
        capability: Option<Capability>,
        input: Value,
    ) -> Result<Value, String>;
}
