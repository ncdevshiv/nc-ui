//! The manifest's contract, exercised through the public loader.
//!
//! Every test here drives a real `ncui.app.json` document through [`AppManifest::parse`]
//! rather than building the struct in Rust, because the manifest is a file an author
//! writes: what matters is what the loader accepts and refuses, not what the struct can
//! be made to hold. These are the same tests the module once carried inline; they live
//! beside the crate rather than inside it so the module's length is the length of what
//! it does, and nothing here needed the crate's internals to begin with.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use std::path::Path;

use ncui_app::manifest::{
    AppManifest, DEFAULT_ASSETS_DIRECTORY, EntryPoint, MANIFEST_FILE_NAME, ManifestError,
    MenuEntryConfig, WindowConfig, is_identity, is_locale_tag, is_scheme,
};
use ncui_core::{Capability, CapabilitySet};

fn manifest(raw: &str) -> Result<AppManifest, ManifestError> {
    AppManifest::parse(raw, Path::new(MANIFEST_FILE_NAME))
}

#[test]
fn a_link_scheme_is_validated_because_it_reaches_a_registry_key() {
    let base = AppManifest::parse(
        r#"{"name":"counter","version":"1.0.0"}"#,
        Path::new("ncui.app.json"),
    )
    .expect("a minimal manifest");
    assert!(is_scheme("ncui"));
    assert!(is_scheme("my-app.v2"));
    assert!(!is_scheme(""));
    assert!(!is_scheme("1app"));
    assert!(!is_scheme("app://"));
    assert!(!is_scheme("app/x"));
    assert!(!is_scheme(&"a".repeat(65)));

    let mut manifest = AppManifest {
        scheme: Some("ncui".to_owned()),
        ..base
    };
    assert!(manifest.validate().is_ok());
    manifest.scheme = Some("ncui://open".to_owned());
    let error = manifest.validate().expect_err("not a scheme");
    assert!(error.to_string().contains("scheme"), "{error}");
}

#[test]
fn a_default_locale_is_validated_as_a_language_tag() {
    // A locale decides the text direction an interface lays out in, so a manifest that
    // declares one must declare a language tag rather than a value that is not one.
    let base = AppManifest::parse(
        r#"{"name":"counter","version":"1.0.0"}"#,
        Path::new("ncui.app.json"),
    )
    .expect("a minimal manifest");
    assert!(is_locale_tag("en"));
    assert!(is_locale_tag("pt-BR"));
    assert!(is_locale_tag("he-IL"));
    assert!(is_locale_tag("zh_Hans"));
    assert!(!is_locale_tag(""));
    assert!(!is_locale_tag("en US"), "a space is not a subtag separator");
    assert!(!is_locale_tag("en.US"), "a dot is a codeset, not a tag");
    assert!(!is_locale_tag("../etc"), "a locale is not a path");
    assert!(!is_locale_tag("toolongsubtag"));

    let mut manifest = AppManifest {
        default_locale: Some("he-IL".to_owned()),
        ..base
    };
    assert!(manifest.validate().is_ok());
    manifest.default_locale = Some("not a locale".to_owned());
    let error = manifest.validate().expect_err("not a locale tag");
    assert!(error.to_string().contains("default_locale"), "{error}");
}

#[test]
fn a_manifest_declaring_a_default_locale_parses_it() {
    // The declared locale round-trips through the manifest, so an application that names
    // its interface language has it honoured rather than dropped on the floor.
    let manifest = AppManifest::parse(
        r#"{"name":"counter","version":"1.0.0","default_locale":"he"}"#,
        Path::new("ncui.app.json"),
    )
    .expect("a manifest with a default locale");
    assert_eq!(manifest.default_locale.as_deref(), Some("he"));
    // Absent stays absent: an application that declares none falls back to the platform.
    let bare = AppManifest::parse(
        r#"{"name":"counter","version":"1.0.0"}"#,
        Path::new("ncui.app.json"),
    )
    .expect("a minimal manifest");
    assert_eq!(bare.default_locale, None);
}

#[test]
fn an_identity_is_validated_because_it_reaches_a_registry_key_and_a_script() {
    // The value is written into the platform's registry and into a generated script, so a
    // character that is not part of an identity is refused rather than escaped.
    let base = AppManifest::parse(
        r#"{"name":"counter","version":"1.0.0"}"#,
        Path::new("ncui.app.json"),
    )
    .expect("a minimal manifest");
    let mut manifest = AppManifest {
        identity: Some("com.ncui.counter".to_owned()),
        ..base
    };
    assert!(manifest.validate().is_ok());
    assert!(is_identity("com.ncui.counter"));
    assert!(is_identity("a b"));
    assert!(!is_identity(""));
    assert!(!is_identity("a'; Remove-Item C:\\; 'b"));
    assert!(!is_identity("a\nb"));

    manifest.identity = Some("com.ncui.'; rm -rf /".to_owned());
    let error = manifest
        .validate()
        .expect_err("an identity that is not one is refused");
    assert!(error.to_string().contains("identity"), "{error}");

    // An application that names none is valid: its identity is derived when a bundle is
    // built, so the field stays optional.
    manifest.identity = None;
    assert!(manifest.validate().is_ok());
}

#[test]
fn a_declared_shortcut_is_read_and_a_malformed_one_is_refused() {
    let parsed = manifest(
        r#"{"name":"a","version":"1.0.0","shortcuts":[{"accelerator":"Ctrl+Shift+K","command":"go"}]}"#,
    )
    .expect("a manifest with a shortcut");
    assert_eq!(parsed.shortcuts.len(), 1);
    assert_eq!(parsed.shortcuts[0].accelerator, "Ctrl+Shift+K");
    assert_eq!(parsed.shortcuts[0].command, "go");

    for raw in [
        // No accelerator.
        r#"{"name":"a","version":"1.0.0","shortcuts":[{"accelerator":"","command":"go"}]}"#,
        // Spaces: an accelerator is one word per part, and a space is not part of one.
        r#"{"name":"a","version":"1.0.0","shortcuts":[{"accelerator":"Ctrl Shift K","command":"go"}]}"#,
        // No command: a shortcut that does nothing is not a shortcut.
        r#"{"name":"a","version":"1.0.0","shortcuts":[{"accelerator":"Ctrl+K","command":"  "}]}"#,
        // No command at all, which is not a shortcut this can read.
        r#"{"name":"a","version":"1.0.0","shortcuts":[{"accelerator":"Ctrl+K"}]}"#,
    ] {
        let error = manifest(raw).expect_err("a malformed shortcut is refused");
        assert!(
            matches!(
                error,
                ManifestError::Invalid { .. } | ManifestError::Malformed { .. }
            ),
            "{raw} produced {error}"
        );
    }
}

#[test]
fn native_menus_are_parsed_with_nested_entries_and_enabled_defaults() {
    let parsed = manifest(
        r#"{
            "name":"a","version":"1.0.0","menus":[
                {"label":"File","items":[
                    {"type":"item","label":"Increment","command":"increment"},
                    {"type":"separator"},
                    {"type":"submenu","label":"More","enabled":false,"items":[
                        {"type":"item","label":"Summary","command":"counter.summary","enabled":false}
                    ]}
                ]}
            ]
        }"#,
    )
    .expect("a manifest with native menus");
    assert_eq!(parsed.menus.len(), 1);
    assert!(parsed.menus[0].enabled);
    let MenuEntryConfig::Submenu(submenu) = &parsed.menus[0].items[2] else {
        panic!("the third entry is a submenu");
    };
    assert!(!submenu.enabled);
    let MenuEntryConfig::Item { enabled, .. } = &submenu.items[0] else {
        panic!("the submenu contains an item");
    };
    assert!(!enabled);

    let plain = manifest(r#"{"name":"a","version":"1.0.0"}"#).expect("a minimal manifest");
    assert!(plain.menus.is_empty());
}

#[test]
fn malformed_native_menus_are_refused() {
    for raw in [
        r#"{"name":"a","version":"1.0.0","menus":[{"label":"","items":[{"type":"separator"}]}]}"#,
        r#"{"name":"a","version":"1.0.0","menus":[{"label":"File","items":[]}]}"#,
        r#"{"name":"a","version":"1.0.0","menus":[{"label":"File","items":[{"type":"item","label":"Bad\nLabel","command":"go"}]}]}"#,
        r#"{"name":"a","version":"1.0.0","menus":[{"label":"File","items":[{"type":"item","label":"Bad","command":"ncui.method"}]}]}"#,
        r#"{"name":"a","version":"1.0.0","menus":[{"label":"File","items":[{"type":"item","label":"Bad","command":"Upper"}]}]}"#,
        r#"{"name":"a","version":"1.0.0","menus":[{"label":"File","items":[{"type":"unknown"}]}]}"#,
    ] {
        let error = manifest(raw).expect_err("a malformed menu is refused");
        assert!(
            matches!(
                error,
                ManifestError::Invalid { .. } | ManifestError::Malformed { .. }
            ),
            "{error}"
        );
    }
}

#[test]
fn a_native_menu_command_has_one_meaning_across_the_whole_tree() {
    let error = manifest(
        r#"{"name":"a","version":"1.0.0","menus":[
            {"label":"File","items":[{"type":"item","label":"One","command":"dup"}]},
            {"label":"Edit","items":[{"type":"item","label":"Two","command":"dup"}]}
        ]}"#,
    )
    .expect_err("duplicate menu commands are refused");
    assert!(error.to_string().contains("dup"), "{error}");
}

#[test]
fn native_menu_depth_and_size_are_bounded() {
    let mut entry = serde_json::json!({"type":"item","label":"Leaf","command":"leaf"});
    for _ in 0..8 {
        entry = serde_json::json!({"type":"submenu","label":"Nested","items":[entry]});
    }
    let raw = serde_json::json!({
        "name":"a","version":"1.0.0","menus":[{"label":"File","items":[entry]}]
    });
    let error = AppManifest::parse(&raw.to_string(), Path::new("ncui.app.json"))
        .expect_err("an over-deep menu is refused");
    assert!(error.to_string().contains("deep"), "{error}");

    let items: Vec<_> = (0..257)
        .map(|index| {
            serde_json::json!({
                "type":"item","label":format!("Item {index}"),"command":format!("command_{index}")
            })
        })
        .collect();
    let raw = serde_json::json!({
        "name":"a","version":"1.0.0","menus":[{"label":"File","items":items}]
    });
    let error = AppManifest::parse(&raw.to_string(), Path::new("ncui.app.json"))
        .expect_err("an oversized menu is refused");
    assert!(error.to_string().contains("256"), "{error}");
}

#[test]
fn the_example_application_s_manifest_is_one_this_accepts() {
    // The example is the documentation a person copies, so a manifest that no longer loads
    // would be documentation that no longer works.
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/counter")
        .join(MANIFEST_FILE_NAME);
    let raw = std::fs::read_to_string(&path).expect("the example's manifest is readable");
    let parsed = AppManifest::parse(&raw, &path).expect("the example's manifest is valid");
    assert_eq!(parsed.name, "counter");
    assert!(
        !parsed.shortcuts.is_empty(),
        "the example declares a global shortcut, and this is what keeps it honest"
    );
    assert!(
        parsed
            .shortcuts
            .iter()
            .all(|shortcut| !shortcut.command.is_empty()),
        "every declared shortcut names a command"
    );
}

#[test]
fn a_tray_icon_is_read_and_a_malformed_one_is_refused() {
    let parsed = manifest(
        r#"{"name":"a","version":"1.0.0","tray":{"icon":"tray.bmp","tooltip":"Counter"}}"#,
    )
    .expect("a manifest with a tray icon");
    let tray = parsed.tray.as_ref().expect("the tray is read");
    assert_eq!(tray.icon, "tray.bmp");
    assert_eq!(tray.tooltip, "Counter");

    // An application that shows no tray says nothing about one.
    let plain = manifest(r#"{"name":"a","version":"1.0.0"}"#).expect("a manifest");
    assert!(plain.tray.is_none());
    assert_eq!(
        plain
            .tray_icon_path(Path::new("/srv/app"))
            .expect("no tray"),
        None
    );

    // The tray's image resolves inside the application directory, like the window's.
    let root = Path::new("/srv/app");
    assert_eq!(
        parsed.tray_icon_path(root).expect("the tray resolves"),
        Some(root.join("tray.bmp"))
    );

    for raw in [
        // An image outside the application.
        r#"{"name":"a","version":"1.0.0","tray":{"icon":"../outside.bmp","tooltip":"x"}}"#,
        // An image at an absolute path.
        r#"{"name":"a","version":"1.0.0","tray":{"icon":"/etc/tray.bmp","tooltip":"x"}}"#,
        // No image at all.
        r#"{"name":"a","version":"1.0.0","tray":{"icon":"","tooltip":"x"}}"#,
        // A tooltip with no words says nothing about what the icon belongs to.
        r#"{"name":"a","version":"1.0.0","tray":{"icon":"tray.bmp","tooltip":"  "}}"#,
        // No tooltip field at all, which is not a tray this can read.
        r#"{"name":"a","version":"1.0.0","tray":{"icon":"tray.bmp"}}"#,
    ] {
        let error = manifest(raw).expect_err("a malformed tray is refused");
        assert!(
            matches!(
                error,
                ManifestError::Invalid { .. } | ManifestError::Malformed { .. }
            ),
            "{raw} produced {error}"
        );
    }
}

#[test]
fn a_complete_manifest_is_read() {
    let parsed = manifest(
        r#"{
            "name": "counter",
            "version": "1.0.0",
            "entry": "app.html",
            "assets": "public",
            "window": { "title": "Counter", "width": 640, "height": 480 },
            "capabilities": ["read.metrics"]
        }"#,
    )
    .expect("a complete manifest is valid");
    assert_eq!(parsed.name, "counter");
    assert_eq!(parsed.version, "1.0.0");
    assert_eq!(parsed.entry, EntryPoint::Page("app.html".to_owned()));
    assert_eq!(parsed.assets, "public");
    assert_eq!(parsed.window.title.as_deref(), Some("Counter"));
    assert_eq!(parsed.window.width, Some(640));
    assert_eq!(parsed.window.height, Some(480));
    assert_eq!(
        parsed.capabilities,
        CapabilitySet::empty().with(Capability::ReadMetrics)
    );
}

#[test]
fn a_manifest_with_only_a_name_and_version_takes_the_defaults() {
    let parsed =
        manifest(r#"{"name":"counter","version":"1.0.0"}"#).expect("a minimal manifest");
    assert_eq!(
        parsed.entry,
        EntryPoint::Page("index.html".to_owned()),
        "the default entry is the index page, exactly as it has always been"
    );
    assert_eq!(parsed.assets, DEFAULT_ASSETS_DIRECTORY);
    assert_eq!(parsed.window, WindowConfig::default());
    assert!(parsed.capabilities.is_empty());
}

#[test]
fn a_manifest_that_is_not_json_is_refused() {
    let error = manifest("this is not json").expect_err("not json");
    assert!(
        matches!(error, ManifestError::Malformed { .. }),
        "got: {error}"
    );
}

#[test]
fn a_manifest_with_an_unknown_capability_is_refused() {
    let error =
        manifest(r#"{"name":"a","version":"1.0.0","capabilities":["not.a.capability"]}"#)
            .expect_err("unknown capability");
    assert!(
        matches!(error, ManifestError::Malformed { .. }),
        "got: {error}"
    );
    assert!(
        error.to_string().contains("not.a.capability"),
        "the refusal must name the capability it did not recognise; got: {error}"
    );
}

#[test]
fn a_manifest_with_an_empty_name_or_version_is_refused() {
    for raw in [
        r#"{"name":"","version":"1.0.0"}"#,
        r#"{"name":"a","version":""}"#,
        r#"{"name":"a","version":"one"}"#,
        r#"{"name":"a","version":"1.0"}"#,
    ] {
        let error = manifest(raw).expect_err("an invalid manifest");
        assert!(
            matches!(error, ManifestError::Invalid { .. }),
            "{raw} produced {error}"
        );
    }
}

#[test]
fn an_entry_that_leaves_the_application_is_refused() {
    for entry in ["../outside.html", "/etc/passwd", "a/../../b.html"] {
        let raw = format!(r#"{{"name":"a","version":"1.0.0","entry":"{entry}"}}"#);
        let error = manifest(&raw).expect_err("an entry that escapes");
        assert!(
            matches!(error, ManifestError::Invalid { .. }),
            "{entry} produced {error}"
        );
    }
}

#[test]
fn the_entry_resolves_inside_the_assets_directory() {
    let parsed = manifest(r#"{"name":"a","version":"1.0.0","entry":"pages/app.html"}"#)
        .expect("manifest");
    let root = Path::new("/srv/app");
    let resolved = parsed.entry_path(root).expect("the entry resolves");
    assert_eq!(resolved, root.join("public").join("pages").join("app.html"));
}

#[test]
fn an_icon_is_named_by_a_path_inside_the_application() {
    let parsed =
        manifest(r#"{"name":"counter","version":"1.0.0","window":{"icon":"images/icon.bmp"}}"#)
            .expect("a manifest with an icon");
    assert_eq!(parsed.window.icon.as_deref(), Some("images/icon.bmp"));

    // The icon is resolved inside the application directory, not the assets directory: it
    // belongs to the application and is not something the page fetches.
    let root = Path::new("/srv/app");
    let resolved = parsed
        .icon_path(root)
        .expect("the icon resolves")
        .expect("the manifest names one");
    assert_eq!(resolved, root.join("images").join("icon.bmp"));

    // An application that names no icon has none to resolve.
    let plain = manifest(r#"{"name":"counter","version":"1.0.0"}"#).expect("a manifest");
    assert_eq!(plain.window.icon, None);
    assert_eq!(plain.icon_path(root).expect("no icon"), None);

    // An icon outside the application is refused, as any path outside it is.
    for icon in ["../outside.bmp", "/etc/icon.bmp", "a/../../icon.bmp"] {
        let raw = format!(r#"{{"name":"a","version":"1.0.0","window":{{"icon":"{icon}"}}}}"#);
        let error = manifest(&raw).expect_err("an icon that escapes");
        assert!(
            matches!(error, ManifestError::Invalid { .. }),
            "{icon} produced {error}"
        );
        assert!(error.to_string().contains("icon"), "{error}");
    }
}

#[test]
fn the_entry_url_path_is_encoded_per_component() {
    let parsed = manifest(r#"{"name":"a","version":"1.0.0","entry":"my pages/a b.html"}"#)
        .expect("manifest");
    assert_eq!(
        parsed.entry_url_path().expect("a page entry"),
        "/my%20pages/a%20b.html"
    );
}

#[test]
fn a_sidecar_declared_in_the_manifest_is_parsed_with_its_contract() {
    // The author-facing half of a sidecar is the manifest entry: the author writes the
    // command a page calls, the program that answers it, and the capability that gates it.
    // This drives a real `ncui.app.json` through the loader rather than building the
    // struct in Rust, so the declared shape is what the loader accepts.
    let parsed = manifest(
        r#"{
            "name": "searcher",
            "version": "1.0.0",
            "capabilities": ["process.spawn", "filesystem.read"],
            "sidecars": [{
                "command": "search.index",
                "program": "tools/index.py",
                "args": ["--quiet"],
                "requires": ["filesystem.read"],
                "timeout_ms": 2500,
                "summary": "build the search index"
            }]
        }"#,
    )
    .expect("a manifest declaring a sidecar");
    assert_eq!(parsed.sidecars.len(), 1, "the sidecar is declared");
    let sidecar = &parsed.sidecars[0];
    assert_eq!(sidecar.command, "search.index");
    assert_eq!(sidecar.program, "tools/index.py");
    assert_eq!(sidecar.args, vec!["--quiet".to_owned()]);
    assert!(
        sidecar.requires.contains(Capability::FilesystemRead),
        "the declared capability gates the command"
    );
    assert_eq!(sidecar.timeout_ms, Some(2500));
    assert_eq!(sidecar.summary, "build the search index");
}

#[test]
fn a_manifest_with_no_sidecars_declares_none() {
    let parsed = manifest(r#"{"name":"a","version":"1.0.0"}"#).expect("a manifest");
    assert!(
        parsed.sidecars.is_empty(),
        "a sidecar-less application declares no sidecars"
    );
}

#[test]
fn a_sidecar_with_no_command_or_no_program_is_refused() {
    for raw in [
        r#"{"name":"a","version":"1.0.0","sidecars":[{"command":"","program":"x.py"}]}"#,
        r#"{"name":"a","version":"1.0.0","sidecars":[{"command":"c","program":"  "}]}"#,
    ] {
        let error = manifest(raw).expect_err("a sidecar with a missing name");
        assert!(
            matches!(error, ManifestError::Invalid { .. }),
            "got: {error}"
        );
    }
}

#[test]
fn two_sidecars_may_not_answer_the_same_command() {
    let error = manifest(
        r#"{"name":"a","version":"1.0.0","sidecars":[
            {"command":"dup","program":"one.py"},
            {"command":"dup","program":"two.py"}]}"#,
    )
    .expect_err("a duplicate sidecar command");
    assert!(
        matches!(error, ManifestError::Invalid { .. }),
        "got: {error}"
    );
    assert!(
        error.to_string().contains("dup"),
        "the refusal names the command two sidecars claimed; got: {error}"
    );
}

#[test]
fn a_page_command_declared_in_the_manifest_is_parsed_with_its_gate() {
    // The JavaScript half of a command is declared here, not registered by the page: the
    // author names the command and the capability that gates it, so the command sits in the
    // action table pages are checked against exactly like a native one. This drives a real
    // `ncui.app.json` through the loader.
    let parsed = manifest(
        r#"{
            "name": "cart",
            "version": "1.0.0",
            "capabilities": ["inspect.runtime"],
            "page_commands": [{
                "command": "cart.total",
                "requires": ["inspect.runtime"],
                "summary": "total the cart"
            }]
        }"#,
    )
    .expect("a manifest declaring a page command");
    assert_eq!(
        parsed.page_commands.len(),
        1,
        "the page command is declared"
    );
    let command = &parsed.page_commands[0];
    assert_eq!(command.command, "cart.total");
    assert_eq!(command.summary, "total the cart");
    assert!(
        command.requires.contains(Capability::InspectRuntime),
        "the declared capability gates the command"
    );
}

#[test]
fn a_manifest_with_no_page_commands_declares_none() {
    let parsed = manifest(r#"{"name":"a","version":"1.0.0"}"#).expect("a manifest");
    assert!(
        parsed.page_commands.is_empty(),
        "an application that declares none has none"
    );
}

#[test]
fn a_page_command_with_no_name_is_refused() {
    let error = manifest(
        r#"{"name":"a","version":"1.0.0","page_commands":[{"command":"","summary":"x"}]}"#,
    )
    .expect_err("a page command with no name");
    assert!(
        matches!(error, ManifestError::Invalid { .. }),
        "got: {error}"
    );
}

#[test]
fn a_page_command_may_not_answer_the_name_a_sidecar_claims() {
    // A sidecar and a page command both become commands the page calls, registered in the
    // same action table. A name two of them claim would shadow, so it is refused here where
    // the author can see it rather than at registration.
    let error = manifest(
        r#"{"name":"a","version":"1.0.0",
            "sidecars":[{"command":"dup","program":"x.py"}],
            "page_commands":[{"command":"dup","summary":"x"}]}"#,
    )
    .expect_err("a page command shadowing a sidecar");
    assert!(
        matches!(error, ManifestError::Invalid { .. }),
        "got: {error}"
    );
    assert!(
        error.to_string().contains("dup"),
        "the refusal names the contested command; got: {error}"
    );
}
