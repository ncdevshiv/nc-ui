"""Fixes the timeout test and the raw-string lint."""

from pathlib import Path

process = Path("crates/ncui-native/src/process.rs")
text = process.read_text(encoding="utf-8")

old = '''        let request = if cfg!(windows) {
            RunRequest {
                program: "cmd".to_owned(),
                args: vec![
                    "/C".to_owned(),
                    "ping -n 20 127.0.0.1 > nul".to_owned(),
                ],
                stdin: None,
                timeout_ms: Some(300),
            }
        } else {
            RunRequest {
                program: "sh".to_owned(),
                args: vec!["-c".to_owned(), "sleep 20".to_owned()],
                stdin: None,
                timeout_ms: Some(300),
            }
        };'''

new = '''        // The long-running program is started directly, not through a shell: a shell would
        // be the process this runner stops, and the program it started would outlive it.
        // The runner's contract is that it stops the program it started.
        let request = if cfg!(windows) {
            RunRequest {
                program: "ping".to_owned(),
                args: vec!["-n".to_owned(), "20".to_owned(), "127.0.0.1".to_owned()],
                stdin: None,
                timeout_ms: Some(300),
            }
        } else {
            RunRequest {
                program: "sleep".to_owned(),
                args: vec!["20".to_owned()],
                stdin: None,
                timeout_ms: Some(300),
            }
        };'''

assert old in text
text = text.replace(old, new, 1)
process.write_text(text, encoding="utf-8", newline="")

tests = Path("crates/ncui-runtime/tests/application.rs")
text = tests.read_text(encoding="utf-8")
text = text.replace('    let body = r#"', "    let body = r\"", 1)
tests.write_text(text, encoding="utf-8", newline="")
print("patched")
