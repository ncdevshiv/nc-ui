//! The sidecar the supervision tests spawn: real process behavior under
//! external control, because supervision evidence with a simulated child proves
//! nothing.
//!
//! Modes, selected by argument:
//! - `echo` — copies stdin to stdout line by line, forever: the transport
//!   roundtrip child.
//! - `exit <code>` — exits immediately with the code: the crash and the
//!   clean-exit child.
//! - `silent` — reads stdin forever saying nothing: the kill-and-restart
//!   child.
//! - `echo-then-die <after>` — echoes, then exits non-zero after `after`
//!   lines: the transport-across-restart child.

use std::io::{BufRead, Write};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map_or("silent", String::as_str);
    match mode {
        "echo" => {
            let stdin = std::io::stdin();
            let stdout = std::io::stdout();
            for line in stdin.lock().lines() {
                let Ok(line) = line else { return };
                let mut out = stdout.lock();
                if out.write_all(line.as_bytes()).is_err() || out.write_all(b"\n").is_err() {
                    return;
                }
                let _ = out.flush();
            }
        }
        "echo-then-die" => {
            let after: u64 = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(3);
            let stdin = std::io::stdin();
            let stdout = std::io::stdout();
            let mut count = 0u64;
            for line in stdin.lock().lines() {
                let Ok(line) = line else { return };
                count += 1;
                {
                    let mut out = stdout.lock();
                    if out.write_all(line.as_bytes()).is_err() || out.write_all(b"\n").is_err() {
                        return;
                    }
                    let _ = out.flush();
                }
                if count >= after {
                    std::process::exit(7);
                }
            }
        }
        "exit" => {
            let code: i32 = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(1);
            std::process::exit(code);
        }
        // `silent` and anything unknown: wait on stdin forever, saying nothing.
        _ => {
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                if line.is_err() {
                    return;
                }
            }
        }
    }
}
