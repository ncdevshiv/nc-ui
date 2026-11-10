//! Watching an application's files so a change can reload the page.
//!
//! A development loop is start, edit, see. The server already serves the files, so the
//! only missing piece is noticing that one changed and telling the page to reload. This
//! module is that piece, and nothing more: it is a poller, not a file-system notification
//! service. Polling is what `std` offers without a platform binding, and at the scale of an
//! application's assets it is cheap enough to be invisible.
//!
//! # What counts as a change
//!
//! A fingerprint of the tree: the newest modification time, the number of entries, and
//! their total size. The newest time alone misses a deletion, and the count alone misses an
//! edit in place; together they cover the three things an author does — add, change, remove.
//!
//! # What it deliberately does not do
//!
//! It does not watch `node_modules` or any other excluded directory, because it does not
//! know about them; the application's root is the assets directory, and that is what an
//! author edits. It does not debounce: a reload per change is what a development loop
//! wants, and an editor that writes a file four times produces four fingerprints, of which
//! at most one survives to the next poll.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// The most entries a scan will visit.
///
/// A scan that followed a large tree would cost more than the reload it exists to trigger.
/// The bound is reported in the fingerprint, so a truncated scan is visible rather than
/// silently blind to part of the tree.
pub const MAX_ENTRIES: usize = 10_000;

/// What a tree looked like at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    /// The newest modification time in the tree.
    pub newest: SystemTime,
    /// How many entries the tree held, up to [`MAX_ENTRIES`].
    pub entries: usize,
    /// The total size of the files in it.
    pub bytes: u64,
    /// Whether the scan stopped at [`MAX_ENTRIES`] before the tree was exhausted.
    pub truncated: bool,
}

/// Watches one directory tree for changes.
#[derive(Debug, Clone)]
pub struct DirectoryWatcher {
    root: PathBuf,
    last: Option<Fingerprint>,
    max_entries: usize,
}

impl DirectoryWatcher {
    /// A watcher over `root`.
    ///
    /// The first fingerprint is taken here, so a change made after construction is the
    /// first thing [`DirectoryWatcher::changed`] reports.
    ///
    /// # Errors
    ///
    /// Returns an error when the root does not exist or cannot be read.
    pub fn new(root: &Path) -> std::io::Result<Self> {
        let mut watcher = Self {
            root: root.to_path_buf(),
            last: None,
            max_entries: MAX_ENTRIES,
        };
        watcher.last = Some(watcher.fingerprint()?);
        Ok(watcher)
    }

    /// The directory being watched.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether the tree changed since the last call.
    ///
    /// The first call after construction reports `false`: the fingerprint taken at
    /// construction is the baseline, and an application that has not been edited has not
    /// changed.
    ///
    /// # Errors
    ///
    /// Returns an error when the tree cannot be scanned. A scan failure is not reported as
    /// a change, because reloading a page because a directory became unreadable would hide
    /// the real problem.
    pub fn changed(&mut self) -> std::io::Result<bool> {
        let current = self.fingerprint()?;
        let changed = self.last.is_some_and(|last| last != current);
        self.last = Some(current);
        Ok(changed)
    }

    /// The fingerprint the watcher is currently comparing against.
    #[must_use]
    pub fn fingerprint_now(&self) -> Option<Fingerprint> {
        self.last
    }

    /// Scans the tree.
    fn fingerprint(&self) -> std::io::Result<Fingerprint> {
        let mut scan = Scan {
            newest: SystemTime::UNIX_EPOCH,
            entries: 0,
            bytes: 0,
            truncated: false,
            limit: self.max_entries,
        };
        scan.visit(&self.root)?;
        Ok(Fingerprint {
            newest: scan.newest,
            entries: scan.entries,
            bytes: scan.bytes,
            truncated: scan.truncated,
        })
    }
}

/// One walk of the tree.
struct Scan {
    newest: SystemTime,
    entries: usize,
    bytes: u64,
    truncated: bool,
    limit: usize,
}

impl Scan {
    /// Visits `path` and everything below it.
    fn visit(&mut self, path: &Path) -> std::io::Result<()> {
        if self.entries >= self.limit {
            self.truncated = true;
            return Ok(());
        }
        let metadata = std::fs::symlink_metadata(path)?;
        self.entries += 1;
        if let Ok(modified) = metadata.modified()
            && modified > self.newest
        {
            self.newest = modified;
        }
        if metadata.is_dir() {
            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                self.visit(&entry.path())?;
                if self.truncated {
                    return Ok(());
                }
            }
        } else {
            self.bytes += metadata.len();
        }
        Ok(())
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

    /// A directory that is removed when the test ends.
    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "ncui-watch-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("the scratch directory is created");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// Waits until the clock has moved far enough for a write to change an mtime.
    ///
    /// File-system timestamps have a coarser resolution than the loop that edits a file, so
    /// a test that writes twice in the same tick would see no change. Waiting past the
    /// resolution is what makes the test measure the watcher rather than the clock.
    fn wait_for_a_new_tick() {
        std::thread::sleep(std::time::Duration::from_millis(30));
    }

    #[test]
    fn a_fresh_watcher_reports_no_change() {
        let scratch = Scratch::new();
        std::fs::write(scratch.path().join("index.html"), "<p>one</p>")
            .expect("the file is written");
        let mut watcher = DirectoryWatcher::new(scratch.path()).expect("the watcher starts");
        assert!(
            !watcher.changed().expect("the tree is scanned"),
            "nothing has changed since the watcher was created"
        );
    }

    #[test]
    fn writing_a_file_is_reported_as_a_change() {
        let scratch = Scratch::new();
        std::fs::write(scratch.path().join("index.html"), "<p>one</p>")
            .expect("the file is written");
        let mut watcher = DirectoryWatcher::new(scratch.path()).expect("the watcher starts");
        wait_for_a_new_tick();
        std::fs::write(scratch.path().join("app.js"), "export {};").expect("the file is written");
        assert!(watcher.changed().expect("the tree is scanned"));
        assert!(
            !watcher.changed().expect("the tree is scanned"),
            "a change is reported once, not on every poll"
        );
    }

    #[test]
    fn editing_a_file_in_place_is_reported() {
        let scratch = Scratch::new();
        let file = scratch.path().join("index.html");
        std::fs::write(&file, "<p>one</p>").expect("the file is written");
        let mut watcher = DirectoryWatcher::new(scratch.path()).expect("the watcher starts");
        wait_for_a_new_tick();
        std::fs::write(&file, "<p>two, and longer</p>").expect("the file is rewritten");
        assert!(
            watcher.changed().expect("the tree is scanned"),
            "an edit that changes the size must be seen even if the time is coarse"
        );
    }

    #[test]
    fn removing_a_file_is_reported() {
        let scratch = Scratch::new();
        let file = scratch.path().join("extra.js");
        std::fs::write(&file, "export {};").expect("the file is written");
        std::fs::write(scratch.path().join("index.html"), "<p>one</p>")
            .expect("the file is written");
        let mut watcher = DirectoryWatcher::new(scratch.path()).expect("the watcher starts");
        wait_for_a_new_tick();
        std::fs::remove_file(&file).expect("the file is removed");
        assert!(
            watcher.changed().expect("the tree is scanned"),
            "a removal must be seen: the newest time does not move when a file disappears"
        );
    }

    #[test]
    fn a_change_below_a_subdirectory_is_seen() {
        let scratch = Scratch::new();
        std::fs::create_dir_all(scratch.path().join("pages")).expect("the directory is created");
        let mut watcher = DirectoryWatcher::new(scratch.path()).expect("the watcher starts");
        wait_for_a_new_tick();
        std::fs::write(
            scratch.path().join("pages").join("about.html"),
            "<p>about</p>",
        )
        .expect("the file is written");
        assert!(watcher.changed().expect("the tree is scanned"));
    }

    #[test]
    fn a_scan_stops_at_its_bound_and_says_so() {
        let scratch = Scratch::new();
        for index in 0..8 {
            std::fs::write(scratch.path().join(format!("file-{index}.txt")), "x")
                .expect("the file is written");
        }
        let mut watcher = DirectoryWatcher::new(scratch.path()).expect("the watcher starts");
        watcher.max_entries = 3;
        let fingerprint = watcher.fingerprint().expect("the tree is scanned");
        assert!(fingerprint.truncated, "the bound must be reported");
        assert!(
            fingerprint.entries <= 3,
            "the scan must not exceed its bound; visited {}",
            fingerprint.entries
        );
    }

    #[test]
    fn a_root_that_does_not_exist_is_refused() {
        let missing = std::env::temp_dir().join("ncui-watch-no-such-directory");
        let _ = std::fs::remove_dir_all(&missing);
        assert!(DirectoryWatcher::new(&missing).is_err());
    }
}
