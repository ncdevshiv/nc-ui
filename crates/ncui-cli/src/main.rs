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
    // On macOS, tao requires the event loop to be created on the main thread.
    // The wry host creates the loop on a worker thread by default (with
    // `with_any_thread(true)` on Windows/Linux). On macOS, there is no such
    // switch — AppKit's NSApplication must be initialized on the main thread.
    //
    // This block ensures the main thread is ready for event loop creation
    // before any runtime code runs. The actual event loop is created later
    // by `WryHostFactory::spawn()` when the first window is opened.
    //
    // On non-macOS platforms, this is a no-op.
    #[cfg(target_os = "macos")]
    {
        // On macOS, we need to ensure the main thread is the first thread
        // to run. The tao event loop will be created by the wry host when
        // the first window is opened. If the wry-host feature is not
        // enabled, this is a no-op.
        //
        // Note: full ADR-018 implementation requires the event loop to be
        // created here and passed to the host factory, which is a larger
      
        // refactor. For now, we document the requirement and let the
        // wry host handle it when the feature is enabled.
    }

    let args: Vec<String> = std::env::args().skip(1).collect();

    // An installer is this program with a bundle appended, and it installs what it carries. Run
    // with no arguments — which is what a double-click does — it installs into the user's own
    // directory; the arguments it does take are `install`'s, so `--root` and `--json` mean what they
    // mean everywhere else and anything else is refused by the same parser.
    if let Some(bundle) = ncui_cli::installer::self_install() {
        let bundle = match bundle {
            Ok(bundle) => bundle,
            Err(error) => {
                return report_failure(&format!("ncui: this installer cannot be read: {error}"));
            }
        };
        let mut line = vec!["install".to_owned(), bundle.path().display().to_string()];
        line.extend(args.iter().cloned());
        let command = match parse(&line) {
            Ok(command) => command,
            Err(message) => {
                let _ = write_line(&format!("ncui: {message}\n"));
                let _ = write_line(&usage::text());
                return ExitCode::from(EXIT_FAILURE);
            }
        };
        let format = format_from(&line);
        let outcome = commands::run(&command, format);
        let rendered = ncui_cli::render::render(&outcome, &command);
        let code = u8::try_from(outcome.exit_code()).unwrap_or(EXIT_FAILURE);
        // The extracted copy is removed when `bundle` goes out of scope, after the install has read
        // it: an installer that left its own unpacked copy behind would leave a second copy of the
        // application in a directory the user never chose.
        return match write_line(&rendered) {
            Ok(()) => ExitCode::from(code),
            Err(error) => report_failure(&format!("ncui: cannot write output: {error}")),
        };
    }

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
