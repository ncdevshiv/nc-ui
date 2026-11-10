//! What a page remembers between runs.
//!
//! A window's geometry is the *runtime's* memory of where the user put things. This is the
//! page's own: the preference it wants to keep — a theme, a column width, the last document it
//! opened. Without it every application writes its own storage layer, and most of them get the
//! same three things wrong: an unbounded file, a key that is really a path, and a save that
//! cannot fail loudly because it is called from a render.
//!
//! # The shape
//!
//! One JSON object per application, in the user's state directory, beside the window's geometry.
//! A page saves a key, loads a key, or removes a key. The whole document is bounded, each value
//! is bounded, and the number of keys is bounded — a preference store that grows without limit is
//! a disk-space bug the user cannot see.
//!
//! # What is refused
//!
//! A key is not a path and not a document: it is bounded, it may not carry a control character,
//! and an empty one is refused rather than treated as "the whole store". A value is stored as
//! JSON, so what a page saves is what a page reads back — no string encoding, no type guessing.
//!
//! # What a damaged file means
//!
//! Nothing. A state file that cannot be read is treated as empty rather than as a failure: a
//! preference is not worth refusing to start over, and the alternative — an application that
//! will not open because its own settings file is corrupt — is worse than losing the settings.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ncui_core::{ErrorKind, NcError, Result};
use serde_json::Value;

/// The most keys one application may store.
///
/// A preference store is not a database. A page that wants a database has the filesystem
/// capability and a reason to ask for it.
pub const MAX_STATE_KEYS: usize = 256;

/// The longest key that may be stored.
pub const MAX_KEY_BYTES: usize = 128;

/// The largest value that may be stored, as encoded JSON.
pub const MAX_VALUE_BYTES: usize = 64 * 1024;

/// The largest state file that will be read or written.
pub const MAX_STATE_BYTES: usize = 1024 * 1024;

/// One application's remembered preferences.
#[derive(Debug, Clone)]
pub struct PageState {
    path: PathBuf,
    entries: BTreeMap<String, Value>,
    writes: u64,
}

impl PageState {
    /// Opens the state stored at `path`, treating a file that cannot be read as empty.
    #[must_use]
    pub fn open(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            entries: read_entries(path),
            writes: 0,
        }
    }

    /// The file this state lives in.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How many times this state has been written.
    #[must_use]
    pub const fn writes(&self) -> u64 {
        self.writes
    }

    /// The keys this application has stored.
    #[must_use]
    pub fn keys(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    /// Stores `value` under `key`.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Validation`] for a key that is not a key, a value over its bound, a store
    /// that would hold more keys than it may, or a document that cannot be written.
    pub fn save(&mut self, key: &str, value: Value) -> Result<Value> {
        check_key(key)?;
        let encoded = serde_json::to_vec(&value).map_err(|error| {
            NcError::new(ErrorKind::Validation {
                field: "value".to_owned(),
                reason: format!("the value cannot be stored: {error}"),
            })
            .operation("state::save")
        })?;
        if encoded.len() > MAX_VALUE_BYTES {
            return Err(rejected(
                "value",
                format!(
                    "the value is {} bytes, over the limit of {MAX_VALUE_BYTES}",
                    encoded.len()
                ),
            ));
        }
        // A new key is what can push the store over its bound, so the bound is checked before
        // the insert rather than after it.
        if !self.entries.contains_key(key) && self.entries.len() >= MAX_STATE_KEYS {
            return Err(rejected(
                "key",
                format!(
                    "this application already stores {MAX_STATE_KEYS} keys; remove one before \
                     adding another"
                ),
            ));
        }
        let replaced = self.entries.insert(key.to_owned(), value);
        self.write()?;
        Ok(serde_json::json!({
            "saved": true,
            "key": key,
            "replaced": replaced.is_some(),
            "keys": self.entries.len(),
        }))
    }

    /// Reads the value stored under `key`.
    ///
    /// A key that was never stored is `found: false` rather than an error: "the user has not
    /// chosen a theme yet" is an answer, and a page that had to catch an error to learn it would
    /// treat a normal state as a failure.
    #[must_use]
    pub fn load(&self, key: &str) -> Value {
        match self.entries.get(key) {
            Some(value) => serde_json::json!({ "found": true, "key": key, "value": value }),
            None => serde_json::json!({ "found": false, "key": key, "value": Value::Null }),
        }
    }

    /// Removes the value stored under `key`.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Validation`] for a key that is not a key, or when the document cannot be
    /// written.
    pub fn remove(&mut self, key: &str) -> Result<Value> {
        check_key(key)?;
        let removed = self.entries.remove(key).is_some();
        if removed {
            self.write()?;
        }
        Ok(serde_json::json!({
            "removed": removed,
            "key": key,
            "keys": self.entries.len(),
        }))
    }

    /// Writes the whole document.
    fn write(&mut self) -> Result<()> {
        let encoded = serde_json::to_vec_pretty(&self.entries).map_err(|error| {
            NcError::new(ErrorKind::Internal {
                operation: "state::write".to_owned(),
                reason: error.to_string(),
            })
        })?;
        if encoded.len() > MAX_STATE_BYTES {
            return Err(rejected(
                "state",
                format!(
                    "the state is {} bytes, over the limit of {MAX_STATE_BYTES}",
                    encoded.len()
                ),
            ));
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| rejected("state", format!("{}: {error}", parent.display())))?;
        }
        std::fs::write(&self.path, encoded)
            .map_err(|error| rejected("state", format!("{}: {error}", self.path.display())))?;
        self.writes += 1;
        Ok(())
    }
}

/// Whether `key` is a key this store accepts.
///
/// # Errors
///
/// [`ErrorKind::Validation`] naming what is wrong with it.
pub fn check_key(key: &str) -> Result<()> {
    if key.is_empty() {
        return Err(rejected(
            "key",
            "the key is empty; an empty key names nothing, and treating it as the whole store \
             would let a page replace everything it has stored with one call"
                .to_owned(),
        ));
    }
    if key.len() > MAX_KEY_BYTES {
        return Err(rejected(
            "key",
            format!(
                "the key is {} bytes, over the limit of {MAX_KEY_BYTES}",
                key.len()
            ),
        ));
    }
    if let Some(bad) = key.chars().find(|character| character.is_control()) {
        return Err(rejected(
            "key",
            format!("the key contains {bad:?}, which is not part of a name"),
        ));
    }
    Ok(())
}

/// Reads the stored entries, treating anything unreadable as empty.
fn read_entries(path: &Path) -> BTreeMap<String, Value> {
    let Ok(encoded) = std::fs::read(path) else {
        return BTreeMap::new();
    };
    if encoded.len() > MAX_STATE_BYTES {
        tracing::warn!(
            path = %path.display(),
            bytes = encoded.len(),
            "the state file is larger than this store will read, so it is treated as empty"
        );
        return BTreeMap::new();
    }
    match serde_json::from_slice::<BTreeMap<String, Value>>(&encoded) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "the state file could not be read, so it is treated as empty"
            );
            BTreeMap::new()
        }
    }
}

/// A rejection in the shape every command reports.
fn rejected(field: &str, reason: String) -> NcError {
    NcError::new(ErrorKind::Validation {
        field: field.to_owned(),
        reason,
    })
    .operation("state")
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
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A state file this test owns and removes.
    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let directory = std::env::temp_dir().join(format!(
                "ncui-state-{label}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&directory);
            Self {
                path: directory.join("state.json"),
            }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            if let Some(parent) = self.path.parent() {
                let _ = std::fs::remove_dir_all(parent);
            }
        }
    }

    #[test]
    fn what_a_page_saves_is_what_a_page_reads_back() {
        let scratch = Scratch::new("round-trip");
        let mut state = PageState::open(&scratch.path);
        assert_eq!(state.load("theme")["found"], false);

        let saved = state
            .save("theme", serde_json::json!("dark"))
            .expect("the value is stored");
        assert_eq!(saved["saved"], true);
        assert_eq!(saved["replaced"], false);
        assert_eq!(state.writes(), 1);

        let loaded = state.load("theme");
        assert_eq!(loaded["found"], true);
        assert_eq!(loaded["value"], "dark");
        assert_eq!(state.keys(), vec!["theme".to_owned()]);

        // A value is a value, not a string: what comes back is the shape that went in.
        state
            .save(
                "window",
                serde_json::json!({ "columns": [1, 2, 3], "zoom": 1.5 }),
            )
            .expect("a structured value is stored");
        assert_eq!(state.load("window")["value"]["columns"][2], 3);

        // And a replacement says so, because a page that thought it was adding a key would
        // otherwise never learn that it overwrote one.
        let again = state
            .save("theme", serde_json::json!("light"))
            .expect("the value is replaced");
        assert_eq!(again["replaced"], true);
        assert_eq!(again["keys"], 2);
    }

    #[test]
    fn a_second_run_reads_what_the_first_one_wrote() {
        // The whole point: a preference outlives the process that stored it.
        let scratch = Scratch::new("restart");
        {
            let mut first = PageState::open(&scratch.path);
            first
                .save("last-document", serde_json::json!("notes.txt"))
                .expect("stored");
        }
        let second = PageState::open(&scratch.path);
        assert_eq!(second.load("last-document")["value"], "notes.txt");
        assert_eq!(second.writes(), 0, "reading is not writing");
    }

    #[test]
    fn a_key_that_is_not_a_key_is_refused() {
        let scratch = Scratch::new("keys");
        let mut state = PageState::open(&scratch.path);
        for bad in ["", &"k".repeat(MAX_KEY_BYTES + 1), "a\nb", "a\tb"] {
            let error = state
                .save(bad, serde_json::json!(1))
                .expect_err("not a key");
            assert!(error.to_string().contains("key"), "{bad}: {error}");
        }
        assert!(check_key("theme").is_ok());
        assert!(check_key("a.b-c_d").is_ok());
        // A key that is not a key is refused before anything is written.
        assert_eq!(state.writes(), 0);
    }

    #[test]
    fn a_value_over_its_bound_is_refused_before_anything_is_written() {
        let scratch = Scratch::new("values");
        let mut state = PageState::open(&scratch.path);
        let big = "x".repeat(MAX_VALUE_BYTES);
        let error = state
            .save("big", serde_json::json!(big))
            .expect_err("over the bound");
        assert!(error.to_string().contains("over the limit"), "{error}");
        assert_eq!(state.writes(), 0);
    }

    #[test]
    fn a_store_at_its_key_bound_refuses_one_more() {
        let scratch = Scratch::new("keys-bound");
        let mut state = PageState::open(&scratch.path);
        for index in 0..MAX_STATE_KEYS {
            state
                .save(&format!("key-{index}"), serde_json::json!(index))
                .expect("within the bound");
        }
        let error = state
            .save("one-more", serde_json::json!(1))
            .expect_err("the store is full");
        assert!(error.to_string().contains("remove one"), "{error}");
        // Replacing an existing key is not adding one, so it still works.
        state
            .save("key-0", serde_json::json!("replaced"))
            .expect("replacing is not adding");
    }

    #[test]
    fn removing_a_key_that_is_there_removes_it_and_one_that_is_not_is_an_answer() {
        let scratch = Scratch::new("remove");
        let mut state = PageState::open(&scratch.path);
        state
            .save("theme", serde_json::json!("dark"))
            .expect("stored");
        let removed = state.remove("theme").expect("removed");
        assert_eq!(removed["removed"], true);
        assert_eq!(state.load("theme")["found"], false);

        let again = state.remove("theme").expect("nothing to remove");
        assert_eq!(again["removed"], false);
        // One write for the save, one for the removal that removed something — and none for the
        // removal that removed nothing, because a document rewritten to say what it already said
        // is a document written for no reason.
        assert_eq!(state.writes(), 2);
    }

    #[test]
    fn a_state_file_that_cannot_be_read_is_treated_as_empty() {
        // A corrupt preference file is not a reason to refuse to start: the alternative is an
        // application that will not open because its own settings are damaged.
        let scratch = Scratch::new("corrupt");
        if let Some(parent) = scratch.path.parent() {
            std::fs::create_dir_all(parent).expect("creatable");
        }
        std::fs::write(&scratch.path, b"not json at all").expect("writable");
        let mut state = PageState::open(&scratch.path);
        assert!(state.keys().is_empty());
        assert_eq!(state.load("theme")["found"], false);
        // And it is usable: saving replaces the damaged document.
        state
            .save("theme", serde_json::json!("dark"))
            .expect("stored");
        assert_eq!(
            PageState::open(&scratch.path).load("theme")["value"],
            "dark"
        );
    }
}
