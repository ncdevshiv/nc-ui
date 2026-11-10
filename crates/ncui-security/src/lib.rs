//! Security primitives for the NC-UI runtime.
//!
//! This crate is the enforcement point for NC-UI's authority model. Everything here is
//! written on the assumption that its inputs are hostile: capability requests come from
//! agent sessions that may be misconfigured or actively malicious, path components come
//! from untrusted protocol frames, tokens come from credential stores an attacker may be
//! able to read, and log records come from components that will panic and dump state.
//!
//! # Threat model
//!
//! **Trusted.**
//!
//! * The kernel and the operating system's own CSPRNG (`/dev/urandom`, `BCryptGenRandom`).
//!   Everything derived from OS entropy inherits the platform's guarantees; nothing here
//!   re-derives security from a weaker source.
//! * The process's own address space, insofar as an attacker with arbitrary code
//!   execution is out of scope. This crate defends against *hostile input*, not against
//!   a compromised process: if an attacker can run code in-process they can read the
//!   agent session's memory directly and no in-process control can stop them.
//! * The OS credential store that holds token *material*. This crate never writes that
//!   material anywhere the caller did not explicitly ask for.
//!
//! **Not trusted.**
//!
//! * **Loopback is not trusted.** Spec 27 is explicit: a connection over `127.0.0.1` or a
//!   Unix socket is authenticated only to the extent that filesystem permissions on the
//!   socket authenticate it, and on Windows any local user may be able to connect to a
//!   loopback listener. A peer that reaches us over loopback is an *unauthenticated*
//!   peer until a [`SecretToken`] has been verified against it. Nothing in this crate
//!   treats "it came from localhost" as an authorization decision.
//! * **Path strings**, always. They cross the IPC boundary and are fully attacker-chosen.
//!   [`SafePath`] treats every component of every requested path as
//!   hostile, including separators, drive prefixes and Unicode.
//! * **Capability requests.** An agent may ask for anything. [`Authorizer`]
//!   grants only what a set comparison proves is held, and destructive authority is never
//!   sufficient on its own.
//! * **Secrets, in every sink.** Spec 28 forbids a secret reaching source, logs, traces,
//!   metrics or crash reports. [`SecretToken`] implements `Debug` and
//!   `Display` itself for exactly this reason: the derived implementations would print
//!   the bytes, and a token that is `Debug`-formattable in a panic or a `tracing` field is
//!   a token that will eventually be written to disk.
//! * **The clock, for rate limiting.** [`RateLimiter`] takes time
//!   as an explicit `Duration` argument. A limiter that read `Instant::now()` internally
//!   could not be tested at its boundaries, and a test that cannot reach the boundary is
//!   not a test of the boundary.
//!
//! # What this crate does not do
//!
//! * It does not transport tokens. It models and compares them; how they cross a socket
//!   is the IPC crate's problem, and a token in a cleartext transport is a token in a log.
//! * It does not encrypt anything at rest.
//! * It does not resolve symlinks. [`SafePath`] guarantees *lexical*
//!   containment; a symlink planted inside a root can still point outside it. Callers that
//!   open attacker-influenced paths must canonicalize and re-check, or the runtime must
//!   forbid symlink creation inside a root. This is documented rather than silently
//!   assumed, because the honest statement is that `std` cannot close that race.
//!
//! # Relationship to the rest of the workspace
//!
//! This crate sits above [`ncui_core`], whose [`CapabilitySet`](ncui_core::CapabilitySet)
//! it consumes and whose [`ErrorKind`](ncui_core::ErrorKind) it reports. It depends on no
//! other `ncui-*` crate, and it introduces no new external dependency: where a secure
//! primitive is not reachable through `std` — OS entropy on Windows — the API is designed
//! so the caller must supply it rather than so this crate can pretend to provide it.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "test assertions must abort loudly on failure"
    )
)]

pub mod audit;
pub mod authz;
pub mod path;
pub mod ratelimit;
pub mod token;

pub use audit::{AuditEvent, AuditLog, AuditOutcome};
pub use authz::{AgentRole, Authorizer, ConsentToken};
pub use path::SafePath;
pub use ratelimit::RateLimiter;
pub use token::{EntropySource, SecretToken, SystemEntropy, constant_time_eq};
