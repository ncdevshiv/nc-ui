//! NC-UI verification tooling.
//!
//! These are the executable form of the project's quality policy. Each tool turns a rule
//! that would otherwise live in a document into a check that fails CI:
//!
//! * [`scan`] — the zero-unfinished-work policy (spec 40, 78).
//! * [`arch`] — the crate layering and dependency-direction rules (spec 46).
//! * [`registry`] — the rule that a feature is either absent or fully evidenced
//!   (spec 42, 80, 81).
//!
//! They live in a workspace separate from the runtime so that a tooling bug cannot
//! affect a build of the runtime, and so the runtime's dependency tree stays small.

pub mod arch;
pub mod registry;
pub mod scan;
