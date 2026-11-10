//! The component host: real WASM execution, behind the ABI's gate.
//!
//! A component is a `.wasm` artifact this runtime did not write, shipped by someone else, and
//! executed inside the user's process. That is the entire problem this crate solves: the code
//! is untrusted, the authority it wants is real, and neither side can be allowed to decide the
//! question alone.
//!
//! # The two halves, and why they are separate crates
//!
//! [`ncui_abi`] holds the rules — version negotiation, capability narrowing, import
//! refusal. This crate holds the interpreter, and it has no policy of its own: it reads a
//! manifest, calls [`ncui_abi::negotiate`], and instantiates with *only* what the negotiation
//! returned. An interpreter that also decided things would be an interpreter whose decisions
//! could drift from the ones the security crate asserts.
//!
//! # The order a component goes through
//!
//! 1. **Size check.** The artifact's bytes are measured before anything parses them, so a
//!    4 GiB component is a refusal rather than an allocation.
//! 2. **Compile.** `wasmi` validates and compiles the module. No guest code runs yet, and none
//!    ever runs at instantiation: a start section is rejected by the engine configuration, so
//!    "loading" a component cannot execute it.
//! 3. **Declaration.** The manifest is read out of the artifact itself — the component is one
//!    file, not a file plus a sidecar it can disagree with. Reading it costs a bounded,
//!    fuel-limited call, and the answer is parsed by [`ncui_abi::wire`], which refuses unknown
//!    fields and unknown capability names.
//! 4. **Negotiation.** [`ncui_abi::negotiate`] compares the declaration with the host profile
//!    and produces a narrowed grant. A component asking for an ABI the host does not offer, or
//!    importing a service the host will not provide, is refused here — before any of its own
//!    code has been given a link.
//! 5. **Linking.** The host provides exactly two imports: `ncui::capability_check` and
//!    `ncui::host_call`. Any other import in the module fails to link, so a component cannot
//!    reach a runtime that happens to be present in the same process.
//! 6. **Invocation.** Each call is bounded by fuel, by the store's memory limiter, by a copy
//!    budget on every guest-memory read, and by a response-size bound on every write.
//!
//! # What bounds execution
//!
//! * **Fuel.** Every invoke gets a budget; exhausting it is a catchable trap, not a hang. A
//!   component that loops forever is a component that stops.
//! * **Memory.** The store's resource limiter caps the guest's linear memory in bytes, with
//!   `trap_on_grow_failure` set, so `memory.grow` past the cap is an error rather than a `-1`
//!   the component can ignore.
//! * **The copy budget.** A component that hands the host a length of 4 billion does not get a
//!   4 GiB host allocation: the request is refused before the buffer is created.
//! * **The declared services.** A host call is served only if the manifest declared that
//!   service *and* the negotiation granted its capability. Declaring a capability in the
//!   manifest is not enough to use it, and neither is asking nicely at call time.
//!
//! # The contract
//!
//! [`contract`] is the single place the required exports, the permitted imports, the packed
//! `i64` return convention and the refusal codes are written down. A component that does not
//! match it is refused rather than tolerated.

pub mod contract;
pub mod error;
pub mod gateway;
pub mod host;
pub mod limits;

pub use contract::{
    ABI_MAJOR, CAPABILITY_CHECK_IMPORT, HOST_CALL_IMPORT, HOST_MODULE, INVOKE_EXPORT,
    MANIFEST_EXPORT, MEMORY_EXPORT, SCRATCH_EXPORT, pack, unpack,
};
pub use error::ComponentError;
pub use gateway::ComponentGateway;
pub use host::{CompiledComponent, ComponentRuntime, Declaration, RunningComponent};
pub use limits::ComponentLimits;
