//! Fuzzes the semantic-version parser.
//!
//! Two properties: arbitrary text is answered rather than fatal, and anything accepted
//! renders to a spelling that parses back to the same version. The second is what makes a
//! version safe to carry on a wire.

#![no_main]

use ncui_core::version::SemVer;
use ncui_fuzz_target::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(version) = SemVer::parse(text) {
        let rendered = version.to_string();
        assert_eq!(
            SemVer::parse(&rendered).ok(),
            Some(version),
            "a parsed version rendered to {rendered}, which does not parse back to it"
        );
    }
});
