//! Checking for an update: the asking half, without the applying.
//!
//! `ncui update --from <url>` applies a release somebody already found. This module is
//! the other half: given where an application's releases live, it asks whether a newer
//! one exists — once, or on a schedule — so an application that wants to know can know
//! without a person watching. It stops at the answer: downloading and applying stay with
//! `update`, and telling the user stays with the application. Those are decisions, and a
//! check that made them silently would be an updater choosing for somebody else.
//!
//! # What "newer" means
//!
//! Both versions are parsed as semantic versions — the same grammar the workspace uses
//! everywhere else — and a release is newer when its version orders above the current
//! one. A version that does not parse is not guessed at: the check reports that the two
//! are not comparable and hands back the release document itself, so an application that
//! versions differently can apply its own rule.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use ncui_core::SemVer;

use crate::release::{self, Release, ReleaseError};

/// How long one check may take.
///
/// A check fetches a document, which is bounded by the release timeout already; the
/// watcher's stop is honoured between checks, so this is also the longest a stop can
/// wait.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(release::FETCH_TIMEOUT_SECONDS);

/// How the running version and the release's version compare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionComparison {
    /// The release's version is a higher semantic version.
    Newer,
    /// The release's version is the same semantic version.
    Same,
    /// The release's version is a lower semantic version.
    Older,
    /// At least one version is not a semantic version, so precedence was not decided.
    NotComparable,
}

/// The answer to one check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCheck {
    /// The application the release document names.
    pub application: String,
    /// The version that is running now.
    pub current: String,
    /// The release that is available, as its document describes it.
    pub release: Release,
    /// Whether the available release should be treated as an update.
    ///
    /// True when the release's version orders above the current one; also true when the
    /// two are not comparable *and differ*, because a versioning scheme this module
    /// cannot read is the application's business, not a reason to stay silent.
    pub update_available: bool,
    /// How the two versions compared.
    pub comparison: VersionComparison,
}

/// Fetches the release document at `release_url` and compares it with `current`.
///
/// `scratch` is the directory the document is downloaded into; it is created when
/// missing. The document must name `application`: a check is for one application's
/// releases, and a document for another application is refused for the same reason an
/// update would refuse it.
///
/// # Errors
///
/// The fetch and document errors, unchanged: no `curl`, a failed fetch, an oversized
/// answer, a malformed document, or a document for a different application.
pub fn check(
    current: &str,
    application: &str,
    release_url: &str,
    scratch: &Path,
) -> Result<UpdateCheck, ReleaseError> {
    let document = fetch_document(release_url, scratch)?;
    if document.name != application {
        return Err(ReleaseError::NotForThisApplication {
            wanted: application.to_owned(),
            document: document.name,
        });
    }
    Ok(compare_against_document(current, document))
}

/// Fetches and validates the release document at `release_url`.
///
/// A caller that needs the document before it can say which application it is checking
/// for — the command line, which finds the installed application by the document's name —
/// uses this, then [`compare_against_document`].
///
/// # Errors
///
/// The fetch and document errors, unchanged: no `curl`, a failed fetch, an oversized
/// answer, or a malformed document.
pub fn fetch_document(release_url: &str, scratch: &Path) -> Result<Release, ReleaseError> {
    std::fs::create_dir_all(scratch).map_err(|error| ReleaseError::Io {
        path: scratch.to_path_buf(),
        reason: error.to_string(),
    })?;
    let document_path = scratch.join("release.json");
    release::fetch(release_url, release::MAX_RELEASE_BYTES, &document_path)?;
    let bytes = std::fs::read(&document_path).map_err(|error| ReleaseError::Io {
        path: document_path,
        reason: error.to_string(),
    })?;
    release::release(&bytes)
}

/// Compares a release document with the version that is running now.
///
/// The document is trusted to name the application being checked; the caller has already
/// decided that (the command line matches it against the installed application, [`check`]
/// refuses a document that names something else).
#[must_use]
pub fn compare_against_document(current: &str, document: Release) -> UpdateCheck {
    let comparison = compare_versions(current, &document.version);
    let update_available = match comparison {
        VersionComparison::Newer => true,
        VersionComparison::NotComparable => document.version != current,
        VersionComparison::Same | VersionComparison::Older => false,
    };
    UpdateCheck {
        application: document.name.clone(),
        current: current.to_owned(),
        release: document,
        update_available,
        comparison,
    }
}

/// How two version strings compare, when both are semantic versions.
fn compare_versions(current: &str, available: &str) -> VersionComparison {
    match (SemVer::parse(current), SemVer::parse(available)) {
        (Ok(current), Ok(available)) => match available.cmp(&current) {
            core::cmp::Ordering::Greater => VersionComparison::Newer,
            core::cmp::Ordering::Equal => VersionComparison::Same,
            core::cmp::Ordering::Less => VersionComparison::Older,
        },
        _ => VersionComparison::NotComparable,
    }
}

/// A running periodic check, stoppable from the caller's thread.
#[derive(Debug)]
pub struct UpdateWatcher {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl UpdateWatcher {
    /// Stops the periodic check and waits for the in-flight one to finish.
    ///
    /// A check that is mid-fetch is not interrupted — a fetch is bounded — so this
    /// returns after at most one more check completes.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Checks for an update immediately and then every `interval`, on its own thread.
///
/// `on_check` receives every answer, including a failed one: a check that could not
/// reach the release URL is a fact about the network, and an application that asked to
/// be told on a schedule asked to be told what happened, not to have a failure silently
/// skipped. The first check runs as soon as the watcher's thread starts, not after the
/// first interval.
pub fn watch<F>(
    current: String,
    application: String,
    release_url: String,
    scratch: PathBuf,
    interval: Duration,
    on_check: F,
) -> UpdateWatcher
where
    F: FnMut(Result<UpdateCheck, ReleaseError>) + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = Arc::clone(&stop);
    let handle = thread::Builder::new()
        .name("ncui-update-check".to_owned())
        .spawn(move || {
            let mut on_check = on_check;
            loop {
                if stopped.load(Ordering::SeqCst) {
                    return;
                }
                on_check(check(&current, &application, &release_url, &scratch));
                // The interval is slept in slices, so a stop is honoured within a slice
                // rather than after a whole interval of a check that no longer matters.
                let mut remaining = interval;
                while !stopped.load(Ordering::SeqCst) && !remaining.is_zero() {
                    let slice = remaining.min(Duration::from_millis(100));
                    thread::sleep(slice);
                    remaining = remaining.saturating_sub(slice);
                }
            }
        })
        .ok();
    UpdateWatcher { stop, handle }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::{UpdateCheck, VersionComparison, check, watch};
    use crate::release::ReleaseError;

    /// A directory this test owns and removes.
    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new(label: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("ncui-updater-{label}-{}", std::process::id()));
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

    /// Serves one release document over the runtime's own asset server, the way a real
    /// release pipeline would, and returns the URL to fetch it from.
    ///
    /// The server is returned with the URL because it must stay up for the fetches to
    /// connect to: a server that is dropped when this returns is a port nothing listens
    /// on, which is exactly the failure a connection refused reports.
    fn serve_release(served: &PathBuf, document: &str) -> (String, ncui_app::AssetServer) {
        std::fs::create_dir_all(served).expect("the served directory is creatable");
        std::fs::write(served.join("release.json"), document).expect("the document is written");
        let server = ncui_app::AssetServer::start_with(served, Vec::new()).expect("a server");
        (
            format!("http://{}/release.json", server.local_addr()),
            server,
        )
    }

    const DOCUMENT: &str = r#"{
        "name": "counter",
        "version": "2.0.0",
        "url": "https://example.test/counter.bundle",
        "sha256": "0000000000000000000000000000000000000000000000000000000000000000"
    }"#;

    #[test]
    fn a_newer_release_is_reported_as_an_update() {
        let scratch = Scratch::new("newer");
        let served = scratch.path.join("served");
        let (url, _server) = serve_release(&served, DOCUMENT);
        let answer = check("1.0.0", "counter", &url, &scratch.path).expect("the check runs");
        assert_eq!(answer.comparison, VersionComparison::Newer);
        assert!(answer.update_available, "1.0.0 -> 2.0.0 is an update");
        assert_eq!(answer.release.version, "2.0.0");
        assert_eq!(answer.application, "counter");
        assert_eq!(answer.current, "1.0.0");
    }

    #[test]
    fn the_same_and_an_older_release_are_not_updates() {
        let scratch = Scratch::new("same");
        let served = scratch.path.join("served");
        let (url, _server) = serve_release(&served, DOCUMENT);
        let answer = check("2.0.0", "counter", &url, &scratch.path).expect("the check runs");
        assert_eq!(answer.comparison, VersionComparison::Same);
        assert!(!answer.update_available);

        let older = r#"{"name":"counter","version":"1.9.0","url":"u","sha256":"0000000000000000000000000000000000000000000000000000000000000000"}"#;
        let (url, _older_server) = serve_release(&served, older);
        let answer = check("2.0.0", "counter", &url, &scratch.path).expect("the check runs");
        assert_eq!(answer.comparison, VersionComparison::Older);
        assert!(!answer.update_available, "a downgrade is not an update");
    }

    #[test]
    fn a_version_this_module_cannot_read_is_reported_as_not_comparable() {
        let scratch = Scratch::new("incomparable");
        let served = scratch.path.join("served");
        let dated = r#"{"name":"counter","version":"v2.0.0","url":"u","sha256":"0000000000000000000000000000000000000000000000000000000000000000"}"#;
        let (url, _server) = serve_release(&served, dated);
        let answer = check("1.0.0", "counter", &url, &scratch.path).expect("the check runs");
        assert_eq!(answer.comparison, VersionComparison::NotComparable);
        // A different version under a scheme this module cannot read is reported as
        // available, with the document attached so the application applies its own rule.
        assert!(answer.update_available);
        assert_eq!(answer.release.version, "v2.0.0");
    }

    #[test]
    fn a_release_for_another_application_is_refused() {
        let scratch = Scratch::new("foreign");
        let served = scratch.path.join("served");
        let (url, _server) = serve_release(&served, DOCUMENT);
        let error = check("1.0.0", "something-else", &url, &scratch.path)
            .expect_err("a release for another application is refused");
        assert!(
            matches!(error, ReleaseError::NotForThisApplication { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_document_that_is_not_a_release_is_refused() {
        let scratch = Scratch::new("malformed");
        let served = scratch.path.join("served");
        let (url, _server) = serve_release(&served, "not a release document");
        let error = check("1.0.0", "counter", &url, &scratch.path)
            .expect_err("a malformed document is refused");
        assert!(matches!(error, ReleaseError::Malformed { .. }), "{error}");
    }

    #[test]
    fn a_watcher_checks_immediately_and_then_on_its_interval() {
        let scratch = Scratch::new("watch");
        let served = scratch.path.join("served");
        let (url, _server) = serve_release(&served, DOCUMENT);
        let checks = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&checks);
        let watcher = watch(
            "1.0.0".to_owned(),
            "counter".to_owned(),
            url,
            scratch.path.clone(),
            Duration::from_millis(80),
            move |answer| {
                if let Ok(check) = answer {
                    seen.lock().expect("the tally is taken").push(check);
                }
            },
        );
        // The first check is immediate; two intervals must have produced at least three.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if checks.lock().expect("the tally is taken").len() >= 3 {
                break;
            }
            thread_sleep(Duration::from_millis(20));
        }
        let before_stop = checks.lock().expect("the tally is taken").len();
        assert!(
            before_stop >= 3,
            "an immediate check plus two intervals: {before_stop}"
        );
        assert!(
            checks
                .lock()
                .expect("the tally is taken")
                .iter()
                .all(|check: &UpdateCheck| check.update_available),
            "every check compared 1.0.0 against 2.0.0"
        );
        watcher.stop();
        let after_stop = checks.lock().expect("the tally is taken").len();
        thread_sleep(Duration::from_millis(200));
        assert_eq!(
            checks.lock().expect("the tally is taken").len(),
            after_stop,
            "a stopped watcher checks no more"
        );
        assert!(after_stop >= before_stop);
    }

    /// A sleep that clippy cannot confuse with a production blocking call.
    fn thread_sleep(duration: Duration) {
        std::thread::sleep(duration);
    }
}
