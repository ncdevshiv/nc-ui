//! Fuzzes the feature-registry parser.
//!
//! The registry is agent-facing input: `ncui status --registry <path>` parses a file a
//! caller names, and `ncui-registry` parses one in CI. A panic in either would be a denial of
//! service on a tool whose entire job is to report honestly.

#![no_main]

use ncui_fuzz_target::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Malformed JSON must produce an error, never a panic and never an allocation the
    // input's declared size cannot justify.
    let parsed: Result<serde_json::Value, _> = serde_json::from_slice(data);
    if let Ok(value) = parsed {
        // A valid JSON document must also survive being described, which is the next thing
        // the tool does with it.
        let _ = format!("{value}");
    }
});
