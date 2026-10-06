//! The `ncui` binary entry point.
//!
//! Everything of substance lives in the library so that it can be tested without spawning
//! a process; this file only wires arguments to the library and writes the result to
//! stdout.
//!
//! # Output failures are not silent
//!
//! A discarded `write_all` would let `ncui` exit successfully having printed nothing. For a
//! machine-readable CLI that is the worst possible failure: the caller sees exit 0 and
//! parses empty output. So a write failure becomes a non-zero exit, and when stdout itself
//! is unusable the diagnostic goes to stderr through the same policy that forbids
//! `print_stdout` in the library.

use std::io::Write;
use std::process::ExitCode;

use ncui_cli::{Command, commands, format_from, parse, usage};

/// The exit code for a usage error or an unwritable output stream.
const EXIT_FAILURE: u8 = 2;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let command = match parse(&args) {
        Ok(command) => command,
        Err(message) => {
            let _ = write_line(&format!("ncui: {message}\n"));
            let _ = write_line(&usage::text());
            return ExitCode::from(EXIT_FAILURE);
        }
    };

    if command == Command::Help {
        return match write_line(&usage::text()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => report_failure(&format!("ncui: cannot write usage: {error}")),
        };
    }

    let outcome = commands::run(&command, format_from(&args));
    let rendered = ncui_cli::render::render(&outcome, &command);
    let code = u8::try_from(outcome.exit_code()).unwrap_or(EXIT_FAILURE);

    match write_line(&rendered) {
        Ok(()) => ExitCode::from(code),
        Err(error) => report_failure(&format!("ncui: cannot write output: {error}")),
    }
}

/// Reports a write failure on stderr and returns the failure exit code.
///
/// Stderr is used because stdout is by definition unusable at this point. It goes through
/// an explicit write rather than `eprint!`, which the workspace denies in library code.
fn report_failure(message: &str) -> ExitCode {
    let stderr = std::io::stderr();
    let mut out = stderr.lock();
    let _ = out.write_all(message.as_bytes());
    let _ = out.write_all(b"\n");
    let _ = out.flush();
    ExitCode::from(EXIT_FAILURE)
}

/// Writes text to stdout.
fn write_line(text: &str) -> std::io::Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    out.write_all(text.as_bytes())?;
    out.flush()
}
