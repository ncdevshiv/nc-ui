//! Fuzzes path resolution and containment.
//!
//! The property is universal: **no input resolves outside the root.** A generated corpus
//! finds shapes a hand-written list of bad inputs does not, and this one had a real escape —
//! a `..` sitting past the end of the root, which the root-exhausted comparison arm accepted.

#![no_main]

use ncui_fuzz_target::fuzz_target;
use ncui_security::path::SafePath;
use std::path::{Path, PathBuf};

fuzz_target!(|data: &[u8]| {
    let Ok(requested) = std::str::from_utf8(data) else {
        return;
    };
    let root: PathBuf = if cfg!(windows) {
        PathBuf::from("C:/srv/work")
    } else {
        PathBuf::from("/srv/work")
    };

    if let Ok(resolved) = SafePath::resolve(&root, requested) {
        assert!(
            SafePath::is_contained(&root, &resolved),
            "{requested:?} resolved to {}, outside {}",
            resolved.display(),
            root.display()
        );
    }

    // `is_contained` is public and is the documented mitigation for the symlink gap, so it
    // must be conservative in its own right on unnormalised input.
    //
    // The check is for an actual `ParentDir` *component*, not for the two characters `..`
    // appearing anywhere: a perfectly ordinary file named `report..final` contains them and
    // is not a traversal. Testing the substring made this target fail on its own generator's
    // output, which is the same mistake as a corpus of "known bad" inputs — it asserts
    // something other than what it means to.
    let candidate = root.join(requested);
    let climbs = candidate
        .components()
        .any(|component| component == std::path::Component::ParentDir);
    if climbs {
        assert!(
            !SafePath::is_contained(Path::new(&root), &candidate),
            "{requested:?} carries a parent component but was judged contained"
        );
    }
});
