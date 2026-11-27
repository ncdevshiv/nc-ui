//! Bundling an application whose manifest declares a native entry.
//!
//! The bundle's own manifest carries `entry` as one string, and a native entry has no path
//! to put there: what it carries instead is the entry's own compact JSON object, which
//! loses nothing and cannot be mistaken for a page path. The application's manifest itself
//! is copied byte-for-byte either way, so the declaration the runtime reads is exactly the
//! one the author wrote.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use std::path::PathBuf;

use ncui_cli::bundle;

/// A directory that is removed when the test ends.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "ncui-bundle-native-{name}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("the directory is created");
        Self { path }
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The manifest of the application under test: a native root window, and deliberately no
/// entry page anywhere.
const MANIFEST: &str = r#"{
    "name": "settings",
    "version": "1.0.0",
    "entry": {
        "kind": "native",
        "provider": "nc-ui-kit::settings-tabs",
        "props": { "initial_tab": "audio" }
    }
}"#;

#[test]
fn a_native_entrys_bundle_metadata_is_the_entrys_own_object() {
    let app = Scratch::new("app");
    std::fs::create_dir_all(app.path().join("public")).expect("the assets directory");
    std::fs::write(app.path().join("ncui.app.json"), MANIFEST).expect("the manifest");
    let binary = app.path().join("ncui-stand-in.exe");
    std::fs::write(&binary, b"a runtime binary").expect("the binary");
    let out = Scratch::new("out");

    let bundle = bundle::build(app.path(), &binary, out.path()).expect("it bundles");

    // The entry has no path, so the metadata carries the entry itself: every byte of the
    // declaration survives, and a reader cannot mistake it for a page.
    assert_eq!(
        bundle.entry,
        r#"{"kind":"native","provider":"nc-ui-kit::settings-tabs","props":{"initial_tab":"audio"}}"#
    );

    // The manifest the runtime will read is copied byte-for-byte, object entry and all.
    let shipped =
        std::fs::read_to_string(out.path().join("ncui.app.json")).expect("the shipped manifest");
    assert_eq!(
        shipped, MANIFEST,
        "the shipped manifest is the author's own bytes"
    );

    // And a page entry still writes its bare path, byte-identical to what it always was.
    let page_app = Scratch::new("page-app");
    std::fs::create_dir_all(page_app.path().join("public")).expect("the assets directory");
    std::fs::write(
        page_app.path().join("ncui.app.json"),
        r#"{"name":"pages","version":"1.0.0","entry":"index.html"}"#,
    )
    .expect("the page manifest");
    let page_binary = page_app.path().join("ncui-stand-in.exe");
    std::fs::write(&page_binary, b"a runtime binary").expect("the binary");
    let page_out = Scratch::new("page-out");
    let page_bundle =
        bundle::build(page_app.path(), &page_binary, page_out.path()).expect("it bundles");
    assert_eq!(page_bundle.entry, "index.html");
}
