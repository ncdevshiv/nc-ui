//! The capability model.
//!
//! NC-UI does not use ambient authority. Nothing in the runtime can touch the
//! filesystem, spawn a process, open a socket or read a log unless the acting principal
//! — a component, a window, or an agent session — has been granted the corresponding
//! [`Capability`] and the runtime has enforced that grant at the point of use.
//!
//! Capabilities fall into three [`CapabilityScope`]s because they answer different
//! questions and are granted by different actors:
//!
//! * [`CapabilityScope::Runtime`] — OS-level powers a component or window may use.
//! * [`CapabilityScope::Workspace`] — powers that change the user's project on disk.
//! * [`CapabilityScope::Agent`] — powers an autonomous agent session holds over the
//!   runtime's own diagnostics surface.
//!
//! Grants are represented by [`CapabilitySet`], a bitset. The encoding is private: the
//! wire and JSON representations are the sorted capability strings, so the internal
//! bit assignment is free to change without breaking the agent API.

use core::fmt;
use core::str::FromStr;
use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize, Serializer};
use std::fmt::Write as _;

/// Which class of authority a [`Capability`] represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CapabilityScope {
    /// OS-level powers available to components and windows.
    Runtime,
    /// Powers that mutate the user's project on disk.
    Workspace,
    /// Powers an agent session holds over the runtime's diagnostics surface.
    Agent,
}

impl CapabilityScope {
    /// The stable string form used on the wire.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Runtime => "runtime",
            Self::Workspace => "workspace",
            Self::Agent => "agent",
        }
    }
}

impl fmt::Display for CapabilityScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single grantable authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Capability {
    // ---- Runtime scope (spec 23) ----
    /// Read files the component is permitted to see.
    FilesystemRead,
    /// Create, modify or delete files.
    FilesystemWrite,
    /// Spawn child processes.
    ProcessSpawn,
    /// Open outbound network connections.
    Network,
    /// Read or write the system clipboard.
    Clipboard,
    /// Create or destroy windows.
    Window,
    /// Raise user notifications.
    Notifications,
    /// Use persistent storage.
    Database,
    /// Allocate GPU resources.
    Gpu,
    /// Attach debuggers and access runtime internals.
    Debug,
    /// Load and run an untrusted component artifact in this process.
    ///
    /// Separate from every other capability because it is not an authority over a resource: it
    /// is permission to execute code the runtime did not write and cannot read. A page that may
    /// write a file may not thereby run one.
    RunComponents,

    // ---- Workspace scope (spec 36) ----
    /// Modify tracked source files.
    ModifySource,
    /// Add or remove dependencies.
    InstallDependencies,
    /// Perform a release or deployment.
    Deploy,
    /// Irreversibly delete user data.
    DeleteData,

    // ---- Agent scope (spec 36) ----
    /// Read structured logs.
    ReadLogs,
    /// Read runtime metrics.
    ReadMetrics,
    /// Inspect the runtime's component and window tree.
    InspectRuntime,
    /// Inspect GPU surfaces and pipeline state.
    InspectGpu,
    /// Execute the project's test suites.
    RunTests,
    /// Execute the project's benchmarks.
    RunBenchmarks,
}

impl Capability {
    /// Every capability, in declaration order.
    ///
    /// Used to build documentation, the `ncui describe` output and the test that
    /// asserts the string mapping is total.
    pub const ALL: &'static [Capability] = &[
        Capability::FilesystemRead,
        Capability::FilesystemWrite,
        Capability::ProcessSpawn,
        Capability::Network,
        Capability::Clipboard,
        Capability::Window,
        Capability::Notifications,
        Capability::Database,
        Capability::Gpu,
        Capability::Debug,
        Capability::RunComponents,
        Capability::ModifySource,
        Capability::InstallDependencies,
        Capability::Deploy,
        Capability::DeleteData,
        Capability::ReadLogs,
        Capability::ReadMetrics,
        Capability::InspectRuntime,
        Capability::InspectGpu,
        Capability::RunTests,
        Capability::RunBenchmarks,
    ];

    /// The stable string form, e.g. `"filesystem.read"`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FilesystemRead => "filesystem.read",
            Self::FilesystemWrite => "filesystem.write",
            Self::ProcessSpawn => "process.spawn",
            Self::Network => "network",
            Self::Clipboard => "clipboard",
            Self::Window => "window",
            Self::Notifications => "notifications",
            Self::Database => "database",
            Self::Gpu => "gpu",
            Self::Debug => "debug",
            Self::RunComponents => "run.components",
            Self::ModifySource => "modify.source",
            Self::InstallDependencies => "install.dependencies",
            Self::Deploy => "deploy",
            Self::DeleteData => "delete.data",
            Self::ReadLogs => "read.logs",
            Self::ReadMetrics => "read.metrics",
            Self::InspectRuntime => "inspect.runtime",
            Self::InspectGpu => "inspect.gpu",
            Self::RunTests => "run.tests",
            Self::RunBenchmarks => "run.benchmarks",
        }
    }

    /// Which scope grants this capability.
    #[must_use]
    pub const fn scope(self) -> CapabilityScope {
        match self {
            Self::FilesystemRead
            | Self::FilesystemWrite
            | Self::ProcessSpawn
            | Self::Network
            | Self::Clipboard
            | Self::Window
            | Self::Notifications
            | Self::Database
            | Self::Gpu
            | Self::Debug
            | Self::RunComponents => CapabilityScope::Runtime,
            Self::ModifySource | Self::InstallDependencies | Self::Deploy | Self::DeleteData => {
                CapabilityScope::Workspace
            }
            Self::ReadLogs
            | Self::ReadMetrics
            | Self::InspectRuntime
            | Self::InspectGpu
            | Self::RunTests
            | Self::RunBenchmarks => CapabilityScope::Agent,
        }
    }

    /// Whether the grant is destructive: it can destroy user data or publish changes
    /// outside the running process.
    ///
    /// The agent API requires explicit per-call authorization for these (spec 88), so
    /// this predicate is what the authorization gate consults.
    #[must_use]
    pub const fn is_destructive(self) -> bool {
        matches!(
            self,
            // Running components is destructive in the sense this list means: it is permission
            // to execute code the runtime did not write, so every call needs a recorded consent
            // rather than a silent grant.
            Self::FilesystemWrite
                | Self::ProcessSpawn
                | Self::DeleteData
                | Self::Deploy
                | Self::RunComponents
        )
    }

    const fn bit(self) -> u64 {
        // Depends on the discriminant of the enum. A static assertion keeps this
        // correct when variants are added; see `capability_fits_in_bitset`.
        1u64 << (self as u64)
    }
}

/// Error returned when a capability string cannot be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseCapabilityError {
    /// The unrecognised input, truncated.
    pub input: String,
    /// The recognised capability names, for diagnostics.
    pub known: &'static [&'static str],
}

impl fmt::Display for ParseCapabilityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown capability {:?}", self.input)?;
        let mut first = true;
        for name in self.known {
            if first {
                let _ = write!(f, "; known capabilities: {name}");
                first = false;
            } else {
                let _ = write!(f, ", {name}");
            }
        }
        Ok(())
    }
}

impl std::error::Error for ParseCapabilityError {}

impl FromStr for Capability {
    type Err = ParseCapabilityError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Capability::ALL
            .iter()
            .copied()
            .find(|capability| capability.as_str() == s)
            .ok_or_else(|| ParseCapabilityError {
                input: s.chars().take(64).collect(),
                known: CAPABILITY_NAMES,
            })
    }
}

/// Static table of capability names, used for error messages and `ncui describe`.
static CAPABILITY_NAMES: &[&str] = &[
    "filesystem.read",
    "filesystem.write",
    "process.spawn",
    "network",
    "clipboard",
    "window",
    "notifications",
    "database",
    "gpu",
    "debug",
    "run.components",
    "modify.source",
    "install.dependencies",
    "deploy",
    "delete.data",
    "read.logs",
    "read.metrics",
    "inspect.runtime",
    "inspect.gpu",
    "run.tests",
    "run.benchmarks",
];

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A set of granted capabilities.
///
/// O(1) membership and subset checks; iteration yields capabilities in declaration
/// order, which keeps serialized output stable across runs and across processes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct CapabilitySet {
    bits: u64,
}

impl CapabilitySet {
    /// The empty set: no authority at all.
    #[must_use]
    pub const fn empty() -> Self {
        Self { bits: 0 }
    }

    /// A set granting every known capability.
    ///
    /// Used only where a principal genuinely is fully trusted, and it is deliberately
    /// awkward to obtain: it is never the default.
    #[must_use]
    pub const fn all() -> Self {
        let mut bits = 0u64;
        let mut index = 0;
        while index < Capability::ALL.len() {
            bits |= Capability::ALL[index].bit();
            index += 1;
        }
        Self { bits }
    }

    /// Builds a set from an iterator of capabilities.
    #[must_use]
    pub fn from_iter_caps<I: IntoIterator<Item = Capability>>(iter: I) -> Self {
        let mut set = Self::empty();
        for capability in iter {
            set = set.with(capability);
        }
        set
    }

    /// Returns a copy of this set with `capability` granted.
    #[must_use]
    pub const fn with(self, capability: Capability) -> Self {
        Self {
            bits: self.bits | capability.bit(),
        }
    }

    /// Returns a copy of this set with `capability` revoked.
    #[must_use]
    pub const fn without(self, capability: Capability) -> Self {
        Self {
            bits: self.bits & !capability.bit(),
        }
    }

    /// Whether `capability` is granted.
    #[must_use]
    pub const fn contains(self, capability: Capability) -> bool {
        self.bits & capability.bit() != 0
    }

    /// Whether every capability in `self` is also granted by `other`.
    ///
    /// This is the authorization predicate: `required.is_subset_of(granted)`.
    #[must_use]
    pub const fn is_subset_of(self, other: Self) -> bool {
        self.bits & !other.bits == 0
    }

    /// The union of two sets.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self {
            bits: self.bits | other.bits,
        }
    }

    /// The intersection of two sets.
    #[must_use]
    pub const fn intersect(self, other: Self) -> Self {
        Self {
            bits: self.bits & other.bits,
        }
    }

    /// The number of granted capabilities.
    #[must_use]
    pub const fn len(&self) -> u32 {
        self.bits.count_ones()
    }

    /// Whether no capabilities are granted.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.bits == 0
    }

    /// Iterates granted capabilities in declaration order.
    pub fn iter(self) -> impl Iterator<Item = Capability> {
        Capability::ALL
            .iter()
            .copied()
            .filter(move |capability| self.contains(*capability))
    }

    /// The granted capabilities restricted to one scope.
    #[must_use]
    pub fn in_scope(self, scope: CapabilityScope) -> Self {
        Self::from_iter_caps(self.iter().filter(|capability| capability.scope() == scope))
    }

    /// Whether any granted capability is destructive.
    #[must_use]
    pub fn has_destructive(self) -> bool {
        self.iter().any(Capability::is_destructive)
    }
}

impl core::ops::BitOr for CapabilitySet {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

impl core::ops::BitAnd for CapabilitySet {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self {
        self.intersect(rhs)
    }
}

impl IntoIterator for &CapabilitySet {
    type Item = Capability;
    type IntoIter = std::vec::IntoIter<Capability>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter().collect::<Vec<_>>().into_iter()
    }
}

impl FromIterator<Capability> for CapabilitySet {
    fn from_iter<I: IntoIterator<Item = Capability>>(iter: I) -> Self {
        Self::from_iter_caps(iter)
    }
}

impl fmt::Display for CapabilitySet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for capability in self.iter() {
            if !first {
                f.write_str(", ")?;
            }
            first = false;
            write!(f, "{capability}")?;
        }
        Ok(())
    }
}

impl Serialize for CapabilitySet {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let names: Vec<&str> = self.iter().map(Capability::as_str).collect();
        names.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CapabilitySet {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let names = Vec::<String>::deserialize(deserializer)?;
        let mut set = CapabilitySet::empty();
        for name in &names {
            let capability = name.parse::<Capability>().map_err(de::Error::custom)?;
            set = set.with(capability);
        }
        Ok(set)
    }
}

/// Serializes a capability set as a JSON string for use as a map key.
pub fn serialize_as_key<S: Serializer>(
    set: &CapabilitySet,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let mut buffer = String::new();
    for capability in set.iter() {
        if !buffer.is_empty() {
            buffer.push(';');
        }
        let _ = write!(buffer, "{capability}");
    }
    serializer.serialize_str(&buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_capability_fits_in_the_bitset() {
        // The bitset is a u64; adding a 65th capability would silently corrupt grants.
        assert!(
            Capability::ALL.len() < 64,
            "capability set exceeds the 64-bit encoding; widen the representation"
        );
    }

    #[test]
    fn string_mapping_is_total_and_injective() {
        let mut seen = std::collections::HashSet::new();
        for capability in Capability::ALL {
            assert!(
                seen.insert(capability.as_str()),
                "duplicate name for {capability:?}"
            );
            assert_eq!(
                capability
                    .as_str()
                    .parse::<Capability>()
                    .expect("round trip"),
                *capability
            );
        }
        // The static table used in diagnostics must match the enum exactly.
        assert_eq!(CAPABILITY_NAMES.len(), Capability::ALL.len());
        for capability in Capability::ALL {
            assert!(CAPABILITY_NAMES.contains(&capability.as_str()));
        }
    }

    #[test]
    fn unknown_capability_names_are_rejected_with_a_useful_message() {
        let err = "filesystem.delete"
            .parse::<Capability>()
            .expect_err("unknown");
        let message = err.to_string();
        assert!(message.contains("unknown capability"), "got: {message}");
        assert!(message.contains("filesystem.read"), "got: {message}");
    }

    #[test]
    fn subset_is_the_authorization_predicate() {
        let required = CapabilitySet::empty()
            .with(Capability::FilesystemRead)
            .with(Capability::ReadLogs);
        let granted = CapabilitySet::all();
        let partial = CapabilitySet::empty().with(Capability::FilesystemRead);

        assert!(required.is_subset_of(granted));
        assert!(!required.is_subset_of(partial));
        assert!(CapabilitySet::empty().is_subset_of(partial));
    }

    #[test]
    fn set_algebra_matches_expectations() {
        let a = CapabilitySet::empty()
            .with(Capability::Gpu)
            .with(Capability::Debug);
        let b = CapabilitySet::empty().with(Capability::Debug);
        assert_eq!(a.union(b), a);
        assert_eq!(a.intersect(b), b);
        assert_eq!(
            a.without(Capability::Debug),
            CapabilitySet::empty().with(Capability::Gpu)
        );
        assert_eq!(a.len(), 2);
        assert!(CapabilitySet::empty().is_empty());
    }

    #[test]
    fn serialization_is_stable_and_round_trips() {
        let set = CapabilitySet::empty()
            .with(Capability::Network)
            .with(Capability::FilesystemRead);
        let json = serde_json::to_string(&set).expect("serializable");
        assert_eq!(json, r#"["filesystem.read","network"]"#);
        let back: CapabilitySet = serde_json::from_str(&json).expect("deserializable");
        assert_eq!(back, set);
    }

    #[test]
    fn scopes_partition_the_capability_space() {
        let all = CapabilitySet::all();
        let runtime = all.in_scope(CapabilityScope::Runtime);
        let workspace = all.in_scope(CapabilityScope::Workspace);
        let agent = all.in_scope(CapabilityScope::Agent);
        assert_eq!(
            runtime.union(workspace).union(agent).len(),
            all.len(),
            "scopes must partition every capability"
        );
        assert_eq!(runtime.intersect(workspace).len(), 0);
    }

    #[test]
    fn destructive_capabilities_are_identified() {
        let dangerous = CapabilitySet::all().with(Capability::Deploy);
        assert!(dangerous.has_destructive());
        assert!(
            !CapabilitySet::empty()
                .with(Capability::ReadLogs)
                .has_destructive()
        );
    }
}
