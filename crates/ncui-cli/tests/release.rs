//! Updating from a URL, end to end: a release document served over HTTP, its bundle fetched,
//! verified and applied by the real command line.
//!
//! Nothing here is a stand-in. The document and the bundle are served by the runtime's own asset
//! server over a real socket, the fetch is the platform's own tool, and the install and the update
//! are the same code paths a person runs.

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
            "ncui-release-e2e-{label}-{}-{}",
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
fn digest(path: &Path) -> String {
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

/// A bundle at `version`, with the records the bundler writes.
fn bundle(root: &Path, name: &str, version: &str) -> PathBuf {
    let directory = root.join(format!("{name}-{version}"));
    std::fs::create_dir_all(directory.join("public")).expect("the tree is created");
    std::fs::write(directory.join("ncui"), b"the runtime").expect("the runtime is written");
    std::fs::write(
        directory.join("public").join("index.html"),
        format!("<h1>{version}</h1>"),
    )
    .expect("the page is written");
    let mut files = Vec::new();
    for path in ["ncui", "public/index.html"] {
        let full = directory.join(path.replace('/', std::path::MAIN_SEPARATOR_STR));
        files.push(serde_json::json!({
            "path": path,
            "bytes": std::fs::metadata(&full).expect("the file is there").len(),
            "sha256": digest(&full),
        }));
    }
    std::fs::write(
        directory.join("bundle.json"),
        format!(
            r#"{{
                "name": "{name}",
                "version": "{version}",
                "runtime": "ncui",
                "entry": "index.html",
                "files": {}
            }}"#,
            serde_json::Value::Array(files)
        ),
    )
    .expect("the bundle manifest is written");
    directory
}

/// Whether this machine has the tool the fetch uses.
fn has_fetch_tool() -> bool {
    Command::new(ncui_cli::release::FETCH_TOOL)
        .arg("--version")
        .output()
        .is_ok()
}

#[test]
fn an_application_is_updated_from_a_release_it_fetches() {
    let scratch = Scratch::new("update");
    let program = PathBuf::from(env!("CARGO_BIN_EXE_ncui"));
    let root = scratch.path.join("apps");

    // The first version is installed from a bundle on disk.
    let first = bundle(&scratch.path, "counter", "1.0.0");
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

    // What a release pipeline publishes: the bundle as an archive, and a document naming it.
    let second = bundle(&scratch.path, "counter", "2.0.0");
    let served = scratch.path.join("served");
    std::fs::create_dir_all(&served).expect("the served directory is creatable");
    let archive = ncui_cli::installer::write_archive(&second, &served).expect("an archive");
    let server = ncui_app::AssetServer::start_with(&served, Vec::new()).expect("a server");
    let base = format!("http://{}", server.local_addr());
    std::fs::write(
        served.join("release.json"),
        format!(
            r#"{{
                "name": "counter",
                "version": "2.0.0",
                "url": "{base}/bundle.tar",
                "sha256": "{}"
            }}"#,
            digest(&archive)
        ),
    )
    .expect("the release document is written");

    let update = Command::new(&program)
        .arg("update")
        .arg("--from")
        .arg(format!("{base}/release.json"))
        .arg("--root")
        .arg(&root)
        .arg("--json")
        .output()
        .expect("the update runs");

    if !has_fetch_tool() {
        // A machine with no fetch tool is told so rather than failing obscurely. There is no other
        // branch to take: without it nothing can be fetched, and pretending otherwise would be a
        // test that passes on a machine that cannot do the thing.
        assert!(!update.status.success());
        let text = String::from_utf8_lossy(&update.stdout);
        assert!(text.contains("curl"), "{text}");
        return;
    }

    assert!(
        update.status.success(),
        "the update failed: {}",
        String::from_utf8_lossy(&update.stdout)
    );
    let answer: serde_json::Value =
        serde_json::from_slice(&update.stdout).expect("the update answers in JSON");
    assert_eq!(answer["name"], "counter");
    assert_eq!(answer["from_version"], "1.0.0");
    assert_eq!(answer["to_version"], "2.0.0");
    assert_eq!(answer["release"], format!("{base}/release.json"));

    // The installed page is the version that was fetched, and the scratch the fetch used is gone.
    let page = std::fs::read(root.join("counter").join("public").join("index.html"))
        .expect("the page is installed");
    assert_eq!(page, b"<h1>2.0.0</h1>");
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
    assert_eq!(leftovers, 0, "the fetch left its scratch behind");
}

#[test]
fn a_release_that_is_not_what_it_promised_is_refused() {
    let scratch = Scratch::new("tampered");
    let program = PathBuf::from(env!("CARGO_BIN_EXE_ncui"));
    let root = scratch.path.join("apps");
    if !has_fetch_tool() {
        return;
    }

    let first = bundle(&scratch.path, "counter", "1.0.0");
    assert!(
        Command::new(&program)
            .arg("install")
            .arg(&first)
            .arg("--root")
            .arg(&root)
            .output()
            .expect("the install runs")
            .status
            .success()
    );

    let second = bundle(&scratch.path, "counter", "2.0.0");
    let served = scratch.path.join("served");
    std::fs::create_dir_all(&served).expect("creatable");
    ncui_cli::installer::write_archive(&second, &served).expect("an archive");
    let server = ncui_app::AssetServer::start_with(&served, Vec::new()).expect("a server");
    let base = format!("http://{}", server.local_addr());
    // A document whose digest is of something else: the download arrives and is not what was named.
    std::fs::write(
        served.join("release.json"),
        format!(
            r#"{{
                "name": "counter",
                "version": "2.0.0",
                "url": "{base}/bundle.tar",
                "sha256": "{}"
            }}"#,
            "a".repeat(64)
        ),
    )
    .expect("writable");

    let update = Command::new(&program)
        .arg("update")
        .arg("--from")
        .arg(format!("{base}/release.json"))
        .arg("--root")
        .arg(&root)
        .output()
        .expect("the update runs");
    assert!(
        !update.status.success(),
        "a tampered release must be refused"
    );
    let text = String::from_utf8_lossy(&update.stdout);
    assert!(text.contains("hashes to"), "{text}");

    // The version that was installed is still installed, with its own files.
    let page = std::fs::read(root.join("counter").join("public").join("index.html"))
        .expect("the page is installed");
    assert_eq!(page, b"<h1>1.0.0</h1>");
}
