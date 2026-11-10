//! Installing and updating an application, end to end, through the real program.
//!
//! The unit tests in `ncui_cli::installer` cover the archive and the footer, and
//! `ncui_native::install` covers the swap an update performs. This file is the part neither can
//! cover: the installer is a *program*, so it is executed, and the update is a *command line*, so
//! it is run as one, with no knowledge inside either of what the test expects.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// A directory this test owns and removes.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "ncui-installer-e2e-{label}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("the scratch directory is creatable");
        Self { path }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The SHA-256 of a file, as lowercase hex.
///
/// Written here rather than shared with the installer because a test that computed a digest with the
/// code under test would be checking that code against itself.
fn test_digest(path: &Path) -> String {
    use sha2::{Digest as _, Sha256};
    let bytes = std::fs::read(path).expect("the file is readable");
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let mut hex = String::new();
    for byte in hasher.finalize() {
        let _ = core::fmt::Write::write_fmt(&mut hex, format_args!("{byte:02x}"));
    }
    hex
}

/// The file records for a bundle's payload, in the shape the bundler writes them.
fn test_records(root: &Path, paths: &[&str]) -> serde_json::Value {
    let mut files = Vec::new();
    for path in paths {
        let full = root.join(path.replace('/', std::path::MAIN_SEPARATOR_STR));
        let bytes = std::fs::metadata(&full).expect("the file is there").len();
        files.push(serde_json::json!({
            "path": path,
            "bytes": bytes,
            "sha256": test_digest(&full),
        }));
    }
    serde_json::Value::Array(files)
}

/// A bundle with the shape the bundler writes, and no registrations.
///
/// No identity and no scheme on purpose: an installer that registered one would write to the user's
/// own registry, which a test must not do. The registrations are covered by the installer's own
/// tests, and what this test is about is the file being a program that installs.
fn bundle(root: &Path, name: &str) -> PathBuf {
    bundle_version(root, name, "1.0.0")
}

/// The same bundle at a named version, in a directory of its own.
fn bundle_version(root: &Path, name: &str, version: &str) -> PathBuf {
    let directory = root.join(format!("{name}-{version}"));
    std::fs::create_dir_all(directory.join("public")).expect("the tree is created");
    std::fs::write(directory.join("ncui"), b"the runtime").expect("the runtime is written");
    std::fs::write(
        directory.join("public").join("index.html"),
        format!("<h1>{version}</h1>"),
    )
    .expect("the page is written");
    // The records the bundle carries, written after the files they describe because that is the
    // order the bundler uses: it digests what it wrote.
    let records = test_records(&directory, &["ncui", "public/index.html"]);
    std::fs::write(
        directory.join("bundle.json"),
        format!(
            r#"{{
                "name": "{name}",
                "version": "{version}",
                "runtime": "ncui",
                "entry": "index.html",
                "files": {records}
            }}"#
        ),
    )
    .expect("the bundle manifest is written");
    directory
}

#[test]
fn an_installer_installs_the_bundle_it_carries() {
    let scratch = Scratch::new("install");
    let source = bundle(&scratch.path, "counter");
    let program = PathBuf::from(env!("CARGO_BIN_EXE_ncui"));
    let out = scratch.path.join("dist");
    let installer = ncui_cli::installer::write(&source, &program, &out).expect("an installer");

    // The installer is a program, and the program is the runtime: the first bytes of the file are
    // the runtime's own, so running it runs the code that installs.
    let installer_bytes = std::fs::read(&installer).expect("the installer is readable");
    let program_bytes = std::fs::read(&program).expect("the runtime is readable");
    assert_eq!(
        &installer_bytes[..program_bytes.len()],
        program_bytes.as_slice()
    );

    let root = scratch.path.join("apps");
    let output = Command::new(&installer)
        .arg("--root")
        .arg(&root)
        .arg("--json")
        .output()
        .expect("the installer runs");
    assert!(
        output.status.success(),
        "the installer failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let answer: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("the installer answers in JSON");
    assert_eq!(answer["installed"]["name"], "counter");

    // What the install wrote is the bundle that was carried, byte for byte.
    let installed = root.join("counter");
    assert_eq!(
        std::fs::read(installed.join("public").join("index.html")).expect("the page is installed"),
        b"<h1>1.0.0</h1>"
    );
    assert_eq!(
        std::fs::read(installed.join("bundle.json")).expect("the manifest is installed"),
        std::fs::read(source.join("bundle.json")).expect("the source manifest")
    );
    // The installer removed the copy it unpacked: a second copy in a directory the user never chose
    // would be a leak with the application's whole size.
    let leftovers = std::fs::read_dir(std::env::temp_dir())
        .expect("the scratch directory is readable")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(&format!("ncui-setup-{}-", std::process::id()))
        })
        .count();
    assert_eq!(leftovers, 0, "the installer left its unpacked copy behind");
}

#[test]
fn the_command_line_updates_an_installed_application() {
    let scratch = Scratch::new("update");
    let root = scratch.path.join("apps");
    let program = PathBuf::from(env!("CARGO_BIN_EXE_ncui"));
    let first = bundle_version(&scratch.path, "counter", "1.0.0");
    let second = bundle_version(&scratch.path, "counter", "2.0.0");

    let install = Command::new(&program)
        .arg("install")
        .arg(&first)
        .arg("--root")
        .arg(&root)
        .arg("--json")
        .output()
        .expect("the install runs");
    assert!(
        install.status.success(),
        "the install failed: {}",
        String::from_utf8_lossy(&install.stderr)
    );

    let update = Command::new(&program)
        .arg("update")
        .arg(&second)
        .arg("--root")
        .arg(&root)
        .arg("--json")
        .output()
        .expect("the update runs");
    assert!(
        update.status.success(),
        "the update failed: {}",
        String::from_utf8_lossy(&update.stderr)
    );
    let answer: serde_json::Value =
        serde_json::from_slice(&update.stdout).expect("the update answers in JSON");
    assert_eq!(answer["name"], "counter");
    assert_eq!(answer["from_version"], "1.0.0");
    assert_eq!(answer["to_version"], "2.0.0");

    // The installed page is the new version's, and the old copy is gone.
    let page = std::fs::read(root.join("counter").join("public").join("index.html"))
        .expect("the page is installed");
    assert_eq!(page, b"<h1>2.0.0</h1>");
    let entries: Vec<String> = std::fs::read_dir(&root)
        .expect("the root is readable")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, vec!["counter".to_owned()], "nothing else is left");
}

#[test]
fn the_command_line_refuses_to_update_something_that_is_not_installed() {
    let scratch = Scratch::new("update-missing");
    let root = scratch.path.join("apps");
    let program = PathBuf::from(env!("CARGO_BIN_EXE_ncui"));
    let bundle = bundle(&scratch.path, "counter");

    let update = Command::new(&program)
        .arg("update")
        .arg(&bundle)
        .arg("--root")
        .arg(&root)
        .output()
        .expect("the update runs");
    assert!(!update.status.success(), "updating nothing must fail");
    let text = String::from_utf8_lossy(&update.stdout);
    assert!(
        text.contains("counter") && text.contains("not installed"),
        "the refusal must say what is not installed: {text}"
    );
}

#[test]
fn an_installer_refuses_an_argument_it_does_not_take() {
    let scratch = Scratch::new("refuse");
    let source = bundle(&scratch.path, "counter");
    let program = PathBuf::from(env!("CARGO_BIN_EXE_ncui"));
    let installer =
        ncui_cli::installer::write(&source, &program, &scratch.path.join("dist")).expect("built");

    let output = Command::new(&installer)
        .arg("--not-a-flag")
        .output()
        .expect("the installer runs");
    assert!(!output.status.success(), "an unknown flag must be refused");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("--not-a-flag"),
        "the refusal must name the flag: {text}"
    );
}
