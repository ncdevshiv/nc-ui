//! The manifest's JSON wire form.
//!
//! A component carries its own manifest: the artifact is one file, and the host reads the
//! declaration out of the artifact rather than out of a sidecar the artifact can disagree
//! with. That makes this module the parser for attacker-influenced bytes, so it is strict:
//!
//! * **Unknown fields are refused.** A manifest carrying a field this host does not know was
//!   written against a different ABI, and the honest response to that is a refusal, not a
//!   parse that quietly drops the part it did not understand.
//! * **An unrecognised capability name is refused.** [`ncui_core::Capability`] has no
//!   unknown-marker variant, so a name that does not parse cannot be smuggled into a grant as
//!   "something harmless". It is a hard failure that names what it did not recognise.
//! * **Nothing defaults to a grant.** Every field that may be absent is absent-meaning-nothing:
//!   no requested capabilities, no imports, no exports. A manifest that omits a section gets
//!   less, never more.
//!
//! The [`ComponentManifest`] type itself stays the host-facing shape: this module converts
//! to and from it instead of giving it a serialiser, so the rules above cannot be bypassed by
//! deserialising the internal type directly.

use ncui_core::{Capability, CapabilitySet};
use serde::{Deserialize, Serialize};

use crate::manifest::ComponentManifest;
use crate::surface::{Export, ExportKind, Import, ImportKind, Surface};

/// The module namespace every NC-UI host import lives under.
///
/// A component imports its services from here and from nowhere else; the host refuses to link
/// an import from any other module, so a component cannot reach a runtime that happens to have
/// been linked into the same instance.
pub const HOST_MODULE: &str = "ncui";

/// Why a manifest could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    /// The bytes were not JSON, or not the shape this module defines.
    Malformed(String),
    /// A capability name the runtime does not know.
    UnknownCapability(String),
    /// A structural field that parsed but is not usable.
    Invalid(String),
}

impl core::fmt::Display for WireError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Malformed(reason) => write!(f, "the manifest is not well-formed: {reason}"),
            Self::UnknownCapability(name) => {
                write!(f, "the manifest names `{name}`, which is not a capability")
            }
            Self::Invalid(reason) => write!(f, "the manifest is not usable: {reason}"),
        }
    }
}

impl std::error::Error for WireError {}

/// The JSON shape of a [`ComponentManifest`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireManifest {
    /// The ABI the component was built against.
    pub abi_version: String,
    /// Capabilities the component believes it needs.
    #[serde(default)]
    pub requested: CapabilitySet,
    /// The services it will ask the host for.
    #[serde(default)]
    pub imports: Vec<WireImport>,
    /// The methods it offers back.
    #[serde(default)]
    pub exports: Vec<WireExport>,
}

/// One declared import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireImport {
    /// The module it comes from; must be [`HOST_MODULE`].
    pub module: String,
    /// The service name within it.
    pub name: String,
    /// What kind of request it is.
    pub kind: ImportKind,
    /// The capability it needs, by stable wire name.
    #[serde(default)]
    pub requires: Option<String>,
}

/// One declared export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireExport {
    /// The method name the host calls.
    pub name: String,
    /// What it is.
    pub kind: ExportKind,
}

impl WireManifest {
    /// Reads a manifest from JSON bytes.
    ///
    /// # Errors
    ///
    /// Returns [`WireError::Malformed`] for bytes that are not a manifest of this shape,
    /// [`WireError::UnknownCapability`] for a capability name the runtime does not know, and
    /// [`WireError::Invalid`] for a field that parses but cannot be honoured.
    pub fn from_slice(bytes: &[u8]) -> Result<ComponentManifest, WireError> {
        let wire: Self = serde_json::from_slice(bytes)
            .map_err(|error| WireError::Malformed(error.to_string()))?;
        wire.into_manifest()
    }

    /// The manifest as JSON bytes, in the shape [`Self::from_slice`] accepts.
    ///
    /// # Errors
    ///
    /// Returns [`WireError::Malformed`] if the value cannot be serialised, which for these
    /// types means a capability name the serialiser could not render.
    pub fn to_vec(manifest: &ComponentManifest) -> Result<Vec<u8>, WireError> {
        let wire = Self::of(manifest);
        serde_json::to_vec(&wire).map_err(|error| WireError::Malformed(error.to_string()))
    }

    /// The wire view of a manifest, for tooling that writes one.
    #[must_use]
    pub fn of(manifest: &ComponentManifest) -> Self {
        Self {
            abi_version: manifest.abi_version.clone(),
            requested: manifest.requested,
            imports: manifest
                .surface
                .imports()
                .iter()
                .map(|import| WireImport {
                    module: import.module.clone(),
                    name: import.name.clone(),
                    kind: import.kind,
                    requires: import
                        .requires
                        .map(|capability| capability.as_str().to_owned()),
                })
                .collect(),
            exports: manifest
                .surface
                .exports()
                .iter()
                .map(|export| WireExport {
                    name: export.name.clone(),
                    kind: export.kind,
                })
                .collect(),
        }
    }

    /// Converts into the host-facing manifest, applying every rule in this module's header.
    ///
    /// # Errors
    ///
    /// As [`Self::from_slice`].
    pub fn into_manifest(self) -> Result<ComponentManifest, WireError> {
        if self.abi_version.trim().is_empty() {
            return Err(WireError::Invalid(
                "the manifest declares no ABI version".to_owned(),
            ));
        }
        let mut surface = Surface::new();
        for import in self.imports {
            if import.module != HOST_MODULE {
                return Err(WireError::Invalid(format!(
                    "import `{}` comes from module `{}`, but a component may only import \
                     services from `{HOST_MODULE}`",
                    import.name, import.module
                )));
            }
            if import.name.trim().is_empty() {
                return Err(WireError::Invalid(
                    "an import has an empty service name".to_owned(),
                ));
            }
            let requires = match import.requires.as_deref() {
                None => None,
                Some(name) => Some(
                    name.parse::<Capability>()
                        .map_err(|_| WireError::UnknownCapability(name.to_owned()))?,
                ),
            };
            surface = surface.import(Import {
                module: import.module,
                name: import.name,
                kind: import.kind,
                requires,
            });
        }
        for export in self.exports {
            if export.name.trim().is_empty() {
                return Err(WireError::Invalid(
                    "an export has an empty method name".to_owned(),
                ));
            }
            surface = surface.export(Export {
                name: export.name,
                kind: export.kind,
            });
        }
        Ok(ComponentManifest {
            abi_version: self.abi_version,
            requested: self.requested,
            surface,
        })
    }
}

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]
#[cfg(test)]
mod tests {
    use super::*;
    use ncui_core::Capability;

    const JSON: &str = r#"{"abi_version":"1.0","requested":["read.logs"],"imports":[{"module":"ncui","name":"read.logs","kind":"capability_call","requires":"read.logs"}],"exports":[{"name":"sum","kind":"function"}]}"#;

    #[test]
    fn a_manifest_round_trips_through_the_wire_form() {
        let manifest = WireManifest::from_slice(JSON.as_bytes()).expect("well-formed");
        assert_eq!(manifest.abi_version, "1.0");
        assert!(manifest.requested.contains(Capability::ReadLogs));
        assert_eq!(manifest.surface.imports().len(), 1);
        assert_eq!(
            manifest.surface.imports()[0].requires,
            Some(Capability::ReadLogs)
        );
        assert_eq!(manifest.surface.exports().len(), 1);

        let bytes = WireManifest::to_vec(&manifest).expect("serialisable");
        assert_eq!(
            WireManifest::from_slice(&bytes).expect("re-readable"),
            manifest,
            "the wire form must be stable in both directions"
        );
    }

    #[test]
    fn absent_sections_mean_nothing_rather_than_everything() {
        let manifest =
            WireManifest::from_slice(br#"{"abi_version":"1.0"}"#).expect("minimal manifest");
        assert!(manifest.requested.is_empty());
        assert!(manifest.surface.imports().is_empty());
        assert!(manifest.surface.exports().is_empty());
        assert!(manifest.effective_requirements().is_empty());
    }

    #[test]
    fn an_unknown_capability_name_is_refused_not_ignored() {
        // A name that does not parse has no harmless reading: accepting it would mean the
        // component asked for something the grant table cannot name. The declaration field is
        // parsed by `CapabilitySet`'s own deserialiser, so the refusal arrives as a malformed
        // document; an import's requirement is parsed here, so it arrives named. Both are
        // refusals, and which parser saw it first is the only difference.
        let declared = r#"{"abi_version":"1.0","requested":["not.a.capability"]}"#;
        assert!(matches!(
            WireManifest::from_slice(declared.as_bytes()),
            Err(WireError::Malformed(_))
        ));
        let required = r#"{"abi_version":"1.0","imports":[{"module":"ncui","name":"x","kind":"capability_call","requires":"not.a.capability"}]}"#;
        assert_eq!(
            WireManifest::from_slice(required.as_bytes()).expect_err("refused"),
            WireError::UnknownCapability("not.a.capability".to_owned())
        );
    }

    #[test]
    fn an_unknown_field_is_refused() {
        // Written against a format this host does not have; a lenient parse would drop the
        // field the component actually relied on.
        let json = r#"{"abi_version":"1.0","sandboxed":false}"#;
        let error = WireManifest::from_slice(json.as_bytes()).expect_err("refused");
        assert!(matches!(error, WireError::Malformed(_)), "got {error}");
        assert!(error.to_string().contains("sandboxed"), "{error}");
    }

    #[test]
    fn an_import_from_another_module_is_refused() {
        let json = r#"{"abi_version":"1.0","imports":[{"module":"wasi_snapshot_preview1","name":"fd_write","kind":"capability_call"}]}"#;
        let error = WireManifest::from_slice(json.as_bytes()).expect_err("refused");
        assert!(
            matches!(error, WireError::Invalid(_)),
            "an import the host cannot link is invalid, got {error}"
        );
        assert!(
            error.to_string().contains("wasi_snapshot_preview1"),
            "{error}"
        );
    }

    #[test]
    fn a_blank_version_or_blank_name_is_refused() {
        for json in [
            r#"{"abi_version":"  "}"#,
            r#"{"abi_version":"1.0","imports":[{"module":"ncui","name":" ","kind":"capability_call"}]}"#,
            r#"{"abi_version":"1.0","exports":[{"name":"","kind":"function"}]}"#,
        ] {
            let error = WireManifest::from_slice(json.as_bytes()).expect_err("refused");
            assert!(
                matches!(error, WireError::Invalid(_)),
                "{json} should be structurally invalid, got {error}"
            );
        }
    }

    #[test]
    fn bytes_that_are_not_json_are_refused() {
        assert!(matches!(
            WireManifest::from_slice(b"not json at all"),
            Err(WireError::Malformed(_))
        ));
    }

    #[test]
    fn a_declared_but_unimported_capability_still_narrows() {
        // The negotiation gate reads `effective_requirements`, so the wire form has to carry
        // both halves faithfully or a component can hide a request in one of them.
        let manifest = WireManifest::from_slice(JSON.as_bytes()).expect("well-formed");
        let required = manifest.effective_requirements();
        assert!(required.contains(Capability::ReadLogs));
        assert!(!required.contains(Capability::Deploy));
    }
}
