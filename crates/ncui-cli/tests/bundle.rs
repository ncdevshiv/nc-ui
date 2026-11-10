//! Building a bundle, on a real tree.
//!
//! What these assert is what a bundle has to be: a directory that holds the runtime, the
//! application and a manifest describing every file with its digest; a build that is the same
//! twice; and a check that notices when the bundle or the tree it came from has moved on.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ncui_cli::bundle::{self, BundleError};

/// A directory that is removed when the test ends.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    /// A directory under the system's own scratch area, which this test owns.
    fn new(name: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "ncui-bundle-{name}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("the directory is created");
        Self { path }
    }

    /// An application directory with a manifest, an asset and a note.
    fn application(name: &str, manifest: &str) -> Self {
        let scratch = Self::new(name);
        std::fs::create_dir_all(scratch.path.join("public")).expect("the assets directory");
        std::fs::write(scratch.path.join("ncui.app.json"), manifest).expect("the manifest");
        std::fs::write(
            scratch.path.join("public").join("index.html"),
            "<p>a page</p>",
        )
        .expect("the page");
        std::fs::write(
            scratch.path.join("notes.txt"),
            "a note the page does not serve",
        )
        .expect("the note");
        scratch
    }

    /// A runtime binary, standing in for a release build.
    fn binary(&self) -> PathBuf {
        let path = self.path.join("ncui-stand-in.exe");
        std::fs::write(&path, b"a runtime binary").expect("the binary");
        path
    }

    /// The directory itself.
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The manifest of the application under test.
const MANIFEST: &str = r#"{
    "name": "notes",
    "version": "2.1.0",
    "entry": "index.html",
    "assets": "public",
    "scheme": "ncui-notes"
}"#;

/// Every file in a directory, by path relative to it, with its bytes.
fn snapshot(directory: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut stack = vec![directory.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current).expect("the directory is readable") {
            let entry = entry.expect("an entry");
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let relative = path
                    .strip_prefix(directory)
                    .expect("inside the directory")
                    .to_string_lossy()
                    .replace('\\', "/");
                files.insert(
                    relative,
                    std::fs::read(&path).expect("the file is readable"),
                );
            }
        }
    }
    files
}

#[test]
fn a_bundle_holds_the_runtime_the_application_and_a_manifest_of_itself() {
    let scratch = Scratch::application("holds", MANIFEST);
    let out = Scratch::new("holds-out");
    let bundle = bundle::build(scratch.path(), &scratch.binary(), out.path()).expect("it bundles");

    assert_eq!(bundle.name, "notes");
    assert_eq!(bundle.version, "2.1.0");
    assert_eq!(bundle.entry, "index.html");
    assert_eq!(bundle.scheme.as_deref(), Some("ncui-notes"));
    assert_eq!(
        bundle.identity, "com.ncui.notes",
        "an application that names no identity gets one derived from its name"
    );

    let paths: Vec<&str> = bundle.files.iter().map(|file| file.path.as_str()).collect();
    assert!(paths.contains(&"ncui-stand-in.exe"), "{paths:?}");
    assert!(paths.contains(&"ncui.app.json"), "{paths:?}");
    assert!(paths.contains(&"public/index.html"), "{paths:?}");
    assert!(
        paths.contains(&"notes.txt"),
        "a file that is not an asset is part of the application too: {paths:?}"
    );
    assert_eq!(
        paths,
        {
            let mut sorted = paths.clone();
            sorted.sort_unstable();
            sorted
        },
        "the manifest describes the bundle in a fixed order, not the filesystem's"
    );

    // The digests are the files': a manifest whose digests were wrong would be a manifest that
    // proves nothing.
    for entry in &bundle.files {
        let bytes = std::fs::read(out.path().join(&entry.path)).expect("the file is in the bundle");
        assert_eq!(entry.bytes, bytes.len() as u64, "{}", entry.path);
        assert_eq!(entry.sha256.len(), 64, "a SHA-256 is 32 bytes as hex");
    }
    assert_eq!(
        bundle
            .files
            .iter()
            .find(|file| file.path == "ncui-stand-in.exe")
            .map(|file| file.bytes),
        Some(16),
        "the runtime's size is the runtime's"
    );
}

#[test]
fn two_builds_of_one_tree_are_byte_identical() {
    // This is what makes the digest manifest worth reading: a difference means the tree differed,
    // so a timestamp or a filesystem ordering in the bundle would make it worthless.
    let scratch = Scratch::application("twice", MANIFEST);
    let first = Scratch::new("twice-first");
    let second = Scratch::new("twice-second");
    bundle::build(scratch.path(), &scratch.binary(), first.path()).expect("it bundles");
    bundle::build(scratch.path(), &scratch.binary(), second.path()).expect("it bundles again");
    assert_eq!(
        snapshot(first.path()),
        snapshot(second.path()),
        "the same tree must produce the same bundle"
    );
}

#[test]
fn a_bundle_is_checked_against_its_own_manifest() {
    let scratch = Scratch::application("intact", MANIFEST);
    let out = Scratch::new("intact-out");
    bundle::build(scratch.path(), &scratch.binary(), out.path()).expect("it bundles");

    bundle::verify(out.path()).expect("a bundle just built is intact");

    // A file changed inside the bundle after it was built is the case this exists for: the
    // manifest still holds the digest of the file that was supposed to be there.
    std::fs::write(out.path().join("public/index.html"), "<p>tampered</p>").expect("the file");
    let error = bundle::verify(out.path()).expect_err("the bundle is no longer intact");
    assert!(
        matches!(error, BundleError::NotIntact { .. }),
        "got: {error:?}"
    );
    assert!(error.to_string().contains("index.html"), "{error}");

    // A file the manifest describes but the bundle no longer holds is the other half.
    bundle::build(scratch.path(), &scratch.binary(), out.path()).expect("it bundles again");
    std::fs::remove_file(out.path().join("notes.txt")).expect("the file is removed");
    let error = bundle::verify(out.path()).expect_err("a missing file is not intact");
    assert!(error.to_string().contains("missing"), "{error}");
}

#[test]
fn a_check_notices_a_bundle_that_has_moved_on_from_its_tree() {
    let scratch = Scratch::application("stale", MANIFEST);
    let out = Scratch::new("stale-out");
    bundle::build(scratch.path(), &scratch.binary(), out.path()).expect("it bundles");
    bundle::check(scratch.path(), &scratch.binary(), out.path()).expect("the bundle is current");

    // The application changed, so the bundle in `out` describes a tree that no longer exists.
    std::fs::write(
        scratch.path().join("public").join("index.html"),
        "<p>changed</p>",
    )
    .expect("the page is edited");
    let error = bundle::check(scratch.path(), &scratch.binary(), out.path())
        .expect_err("the bundle is stale");
    assert!(matches!(error, BundleError::Stale { .. }), "got: {error:?}");

    // And a check of a directory with no bundle at all says so rather than building one: a check
    // that wrote the thing it was asked to check would be a check nobody could trust.
    let empty = Scratch::new("stale-empty");
    let error = bundle::check(scratch.path(), &scratch.binary(), empty.path())
        .expect_err("there is no bundle to check");
    assert!(error.to_string().contains("ncui build"), "{error}");
}

#[test]
fn a_directory_that_is_not_an_application_is_refused() {
    let empty = Scratch::new("not-an-application");
    let out = Scratch::new("not-an-application-out");
    let binary = empty.path().join("ncui-stand-in.exe");
    std::fs::write(&binary, b"a runtime binary").expect("the binary");
    let error = bundle::build(empty.path(), &binary, out.path())
        .expect_err("a directory without a manifest is not an application");
    assert!(
        matches!(error, BundleError::Application { .. }),
        "got: {error:?}"
    );

    let scratch = Scratch::application("no-binary", MANIFEST);
    let error = bundle::build(scratch.path(), &scratch.path().join("ncui.exe"), out.path())
        .expect_err("a missing runtime is refused rather than bundled as nothing");
    assert!(
        matches!(error, BundleError::MissingBinary { .. }),
        "got: {error:?}"
    );
}

#[test]
fn a_value_a_generated_script_cannot_carry_is_refused() {
    // The identity and the scheme are written into scripts that a user runs, so a value that would
    // end the quoting it sits inside is refused rather than escaped into something that runs.
    let scratch = Scratch::application(
        "hostile",
        r#"{
            "name": "notes",
            "version": "1.0.0",
            "entry": "index.html",
            "assets": "public",
            "scheme": "ncui'; Remove-Item -Recurse C:\\; 'notes"
        }"#,
    );
    let out = Scratch::new("hostile-out");
    let error = bundle::build(scratch.path(), &scratch.binary(), out.path())
        .expect_err("a scheme a script cannot carry is refused");
    // The manifest reader refuses it before the bundler sees it, which is the stronger answer: a
    // value that is not a scheme never reaches the code that writes one into a script.
    assert!(
        matches!(
            error,
            BundleError::Application { .. } | BundleError::Unscriptable { .. }
        ),
        "got: {error:?}"
    );
    assert!(error.to_string().contains("scheme"), "{error}");

    let scratch = Scratch::application(
        "hostile-name",
        r#"{
            "name": "notes'; Remove-Item -Recurse C:\\; '",
            "version": "1.0.0",
            "entry": "index.html",
            "assets": "public"
        }"#,
    );
    let error = bundle::build(scratch.path(), &scratch.binary(), out.path())
        .expect_err("a name a script cannot carry is refused");
    assert!(
        matches!(
            error,
            BundleError::Application { .. } | BundleError::Unscriptable { .. }
        ),
        "got: {error:?}"
    );
}

#[test]
fn a_name_that_is_not_an_identity_becomes_one() {
    use ncui_app::AppManifest;
    let scratch = Scratch::application(
        "derived",
        r#"{
            "name": "My Notes!",
            "version": "1.0.0",
            "entry": "index.html",
            "assets": "public"
        }"#,
    );
    let manifest = AppManifest::load(scratch.path()).expect("the manifest reads");
    let identity = bundle::identity_for(&manifest);
    assert_eq!(identity, "com.ncui.my-notes");
    assert!(
        ncui_native::identity::check(&identity).is_ok(),
        "a derived identity must be one the platform accepts"
    );
}

#[test]
fn a_declared_scheme_ships_its_registration_and_a_silent_one_ships_none() {
    // A deep link is a registration as well as a transport: the platform has to be told which
    // program a scheme belongs to, and that is a script the user runs. An application that declares
    // no scheme gets none, because a bundle that claimed links its application never asked for
    // would be claiming them from whatever handles them now.
    let declared = Scratch::application("scheme-declared", MANIFEST);
    let out = Scratch::new("scheme-declared-out");
    bundle::build(declared.path(), &declared.binary(), out.path()).expect("it bundles");
    let registration = if cfg!(windows) {
        "register-protocol.ps1"
    } else {
        "install-protocol.sh"
    };
    let script = std::fs::read_to_string(out.path().join(registration))
        .expect("the registration is in the bundle");
    assert!(script.contains("ncui-notes"), "{script}");
    assert!(
        script.contains("ncui-stand-in.exe"),
        "the registration names the bundled runtime: {script}"
    );
    assert!(
        script.contains("%1") || script.contains("%u"),
        "the registration carries the field the platform substitutes a link into: {script}"
    );

    let silent = Scratch::application(
        "scheme-silent",
        r#"{
            "name": "notes",
            "version": "1.0.0",
            "entry": "index.html",
            "assets": "public"
        }"#,
    );
    let out = Scratch::new("scheme-silent-out");
    bundle::build(silent.path(), &silent.binary(), out.path()).expect("it bundles");
    assert!(
        !out.path().join(registration).exists(),
        "an application that declares no scheme gets no registration"
    );
}

#[test]
fn a_check_names_what_differs_rather_than_only_that_something_does() {
    // A check that says "different" leaves the reader to find out what; the point of the check is
    // to make a stale or damaged bundle visible.
    let scratch = Scratch::application("names", MANIFEST);
    let out = Scratch::new("names-out");
    bundle::build(scratch.path(), &scratch.binary(), out.path()).expect("it bundles");
    std::fs::write(
        out.path().join("notes.txt"),
        "not what the bundle describes",
    )
    .expect("edited");
    let error = bundle::verify(out.path()).expect_err("the bundle is damaged");
    assert!(error.to_string().contains("notes.txt"), "{error}");
}
