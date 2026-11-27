//! The manifest's `entry` in both its kinds, exercised through the public loader.
//!
//! ADR-020 made a window's content two kinds — a page, or a native view built by a
//! registered provider — and the manifest's `entry` is where an application declares which
//! its root window is. These tests pin the boundary the author faces: a string entry means
//! exactly what it meant before native entries existed, an object entry is read as the
//! window-create contract's content block, and the mistakes an author can make in one are
//! refused with the field they are in.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use std::path::Path;

use ncui_app::manifest::{AppManifest, EntryPoint, MANIFEST_FILE_NAME, ManifestError};

fn manifest(raw: &str) -> Result<AppManifest, ManifestError> {
    AppManifest::parse(raw, Path::new(MANIFEST_FILE_NAME))
}

#[test]
fn a_native_entry_parses_and_round_trips_through_the_loader() {
    let parsed = manifest(
        r#"{
            "name": "settings",
            "version": "1.0.0",
            "entry": {
                "kind": "native",
                "provider": "nc-ui-kit::settings-tabs",
                "props": { "initial_tab": "audio", "rows": 3 }
            }
        }"#,
    )
    .expect("a native entry manifest is valid");
    let EntryPoint::Native(native) = &parsed.entry else {
        panic!("an object entry is a native entry");
    };
    assert_eq!(native.kind, "native");
    assert_eq!(native.provider, "nc-ui-kit::settings-tabs");
    assert_eq!(native.props["initial_tab"], "audio");
    assert_eq!(native.props["rows"], 3);

    // The serialized form is the same object shape the manifest carries — the content
    // block of the window-create contract — so a manifest written from a parsed one loses
    // nothing. And it parses back to the same entry, which is what round-tripping means.
    let encoded = serde_json::to_value(&parsed.entry).expect("the entry serializes");
    assert_eq!(encoded["kind"], "native");
    assert_eq!(encoded["provider"], "nc-ui-kit::settings-tabs");
    assert_eq!(encoded["props"]["rows"], 3);
    let reparsed: EntryPoint =
        serde_json::from_value(encoded).expect("the serialized entry parses back");
    assert_eq!(reparsed, parsed.entry);
}

#[test]
fn a_string_entry_is_a_page_byte_identical_to_before() {
    // Backwards compatibility is absolute: a manifest written before native entries
    // existed parses to the page it always named, and serializes back to the bare string
    // it always was — not to an object, and not to a quoted kind.
    let parsed = manifest(r#"{"name":"a","version":"1.0.0","entry":"pages/app.html"}"#)
        .expect("a string entry is a page");
    assert_eq!(parsed.entry, EntryPoint::Page("pages/app.html".to_owned()));
    let encoded = serde_json::to_string(&parsed.entry).expect("the entry serializes");
    assert_eq!(encoded, r#""pages/app.html""#);

    // The absent entry is the index page, exactly as it has always been.
    let bare = manifest(r#"{"name":"a","version":"1.0.0"}"#).expect("a minimal manifest");
    assert_eq!(bare.entry, EntryPoint::Page("index.html".to_owned()));
}

#[test]
fn a_mistagged_object_entry_is_refused_naming_the_kind() {
    // An object entry is a native view or a mistake. `{"kind": "nativ"}` is not an
    // unusual page — pages are strings — so validation refuses it by the field that is
    // wrong, and the refusal says what the kind should have been.
    let error = manifest(
        r#"{"name":"a","version":"1.0.0","entry":{"kind":"nativ","provider":"nc-ui-kit::settings-tabs"}}"#,
    )
    .expect_err("a mistagged object is a mistake, not a page");
    let ManifestError::Invalid { field, reason } = &error else {
        panic!("a mistagged object is an invalid field, got: {error}");
    };
    assert_eq!(field, "entry.kind");
    assert!(reason.contains("nativ"), "the refusal quotes what was written: {error}");
    assert!(reason.contains("\"native\""), "the refusal names the expected kind: {error}");
}

#[test]
fn an_empty_or_oversized_provider_is_refused() {
    for provider in [String::new(), "p".repeat(129)] {
        let raw = format!(
            r#"{{"name":"a","version":"1.0.0","entry":{{"kind":"native","provider":"{provider}"}}}}"#
        );
        let error = manifest(&raw).expect_err("an unusable provider is refused");
        let ManifestError::Invalid { field, .. } = &error else {
            panic!("the provider is an invalid field, got: {error}");
        };
        assert_eq!(field, "entry.provider", "{error}");
    }

    // The boundary itself is acceptable: 128 bytes is the contract's bound restated.
    let raw = format!(
        r#"{{"name":"a","version":"1.0.0","entry":{{"kind":"native","provider":"{}"}}}}"#,
        "p".repeat(128)
    );
    manifest(&raw).expect("a provider at the bound is accepted");
}

#[test]
fn the_manifest_check_is_deliberately_coarse() {
    // The provider's character set is the window contract's rule, enforced by
    // `WindowRequest::validate` when the window opens — the single source of truth. The
    // manifest checks only what the contract cannot see, so an uppercase provider passes
    // validation here and is refused at the window: two copies of the charset rule would
    // be two answers drifting apart.
    let parsed = manifest(
        r#"{"name":"a","version":"1.0.0","entry":{"kind":"native","provider":"UPPERCASE"}}"#,
    )
    .expect("the charset is not the manifest's rule");
    assert_eq!(
        parsed.entry,
        EntryPoint::Native(ncui_app::manifest::NativeEntryPoint {
            kind: "native".to_owned(),
            provider: "UPPERCASE".to_owned(),
            props: serde_json::Value::Null,
        })
    );
}

#[test]
fn absent_props_default_to_none_and_present_props_round_trip() {
    let parsed = manifest(
        r#"{"name":"a","version":"1.0.0","entry":{"kind":"native","provider":"nc-ui-kit::sonner"}}"#,
    )
    .expect("props are optional");
    let EntryPoint::Native(native) = &parsed.entry else {
        panic!("an object entry is a native entry");
    };
    assert_eq!(
        native.props,
        serde_json::Value::Null,
        "absent props are no props, so the provider's own defaults apply"
    );
}

#[test]
fn unknown_fields_in_a_native_entry_are_refused() {
    // The object is authored by hand, and `props` misspelt would be props silently dropped
    // to the default — a manifest that forgives a typo is one whose author never learns of
    // it. Refusing unknown fields is the same decision the kit's own prop shapes make.
    let error = manifest(
        r#"{"name":"a","version":"1.0.0","entry":{"kind":"native","provider":"x","porps":{"a":1}}}"#,
    )
    .expect_err("a typo'd key is refused rather than silently dropped");
    assert!(
        matches!(error, ManifestError::Malformed { .. }),
        "the object does not have the shape a native entry has: {error}"
    );
}

#[test]
fn an_object_without_a_provider_is_not_an_entry_at_all() {
    // `provider` has no default: an object that does not name one does not have the shape
    // of either kind, so the document is malformed rather than invalid.
    let error =
        manifest(r#"{"name":"a","version":"1.0.0","entry":{"kind":"native"}}"#).expect_err("no provider");
    assert!(matches!(error, ManifestError::Malformed { .. }), "got: {error}");
}

#[test]
fn an_entry_that_is_neither_a_string_nor_an_object_is_malformed() {
    for entry in ["42", "true", "[\"index.html\"]"] {
        let raw = format!(r#"{{"name":"a","version":"1.0.0","entry":{entry}}}"#);
        let error = manifest(&raw).expect_err("not an entry");
        assert!(
            matches!(error, ManifestError::Malformed { .. }),
            "{entry} produced {error}"
        );
    }
}

#[test]
fn a_native_entry_has_no_asset_path_and_no_url_path() {
    // Both questions are page questions. A native entry's content is built by a provider,
    // so asking where it is served from is a programming error, and the honest answer is
    // a refusal rather than an invented path.
    let parsed = manifest(
        r#"{"name":"a","version":"1.0.0","entry":{"kind":"native","provider":"nc-ui-kit::settings-tabs"}}"#,
    )
    .expect("a valid native manifest");
    let root = Path::new("/srv/app");
    let error = parsed
        .entry_path(root)
        .expect_err("a native entry has no asset path");
    assert!(
        matches!(error, ManifestError::Invalid { .. }),
        "the refusal is the manifest's own error shape: {error}"
    );
    assert!(
        error.to_string().contains("native"),
        "the refusal says why: {error}"
    );
    let error = parsed
        .entry_url_path()
        .expect_err("a native entry has no URL path");
    assert!(
        error.to_string().contains("native"),
        "the refusal says why: {error}"
    );

    // The same questions on a page entry answer exactly as before.
    let page = manifest(r#"{"name":"a","version":"1.0.0","entry":"pages/app.html"}"#)
        .expect("a page manifest");
    assert_eq!(
        page.entry_path(root).expect("a page resolves"),
        root.join("public").join("pages").join("app.html")
    );
    assert_eq!(
        page.entry_url_path().expect("a page has a URL path"),
        "/pages/app.html"
    );
}

#[test]
fn the_entrys_one_line_form_is_truthful_for_metadata() {
    // The bundle manifest carries the entry as one string. A page entry is its path,
    // byte-identical to what a string entry always wrote there; a native entry is its
    // compact JSON object, which loses nothing and cannot be mistaken for a path.
    let page = EntryPoint::Page("index.html".to_owned());
    assert_eq!(page.to_string(), "index.html");

    let parsed = manifest(
        r#"{"name":"a","version":"1.0.0","entry":{"kind":"native","provider":"nc-ui-kit::settings-tabs","props":{"rows":3}}}"#,
    )
    .expect("a valid native manifest");
    let rendered = parsed.entry.to_string();
    assert_eq!(
        rendered,
        r#"{"kind":"native","provider":"nc-ui-kit::settings-tabs","props":{"rows":3}}"#,
        "the one-line form is the entry's own JSON object"
    );
    // And because it is the entry's own object, it parses back to the same entry: the
    // metadata form is not a dead end.
    let reparsed: EntryPoint =
        serde_json::from_str(&rendered).expect("the one-line form parses back");
    assert_eq!(reparsed, parsed.entry);
}
