//! `ncui-arch` — fails the build when the crate dependency graph violates the
//! documented layering.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use ncui_tools::arch;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let workspace_root = PathBuf::from(
        args.iter()
            .find(|a| !a.starts_with('-'))
            .cloned()
            .unwrap_or_else(|| ".".to_owned()),
    );

    let crates = match arch::load_crates(&workspace_root) {
        Ok(crates) => crates,
        Err(message) => {
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            let _ = writeln!(out, "ncui-arch: could not read the dependency graph");
            let _ = writeln!(out, "  {message}");
            return ExitCode::FAILURE;
        }
    };

    let report = arch::check(&crates);

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = writeln!(out, "ncui-arch: {} internal crates", report.crates.len());
    for info in &report.crates {
        let deps = if info.internal_deps.is_empty() {
            "-".to_owned()
        } else {
            info.internal_deps.join(", ")
        };
        let _ = writeln!(out, "  {} -> {}", info.name, deps);
    }

    if report.is_clean() {
        let _ = writeln!(out, "dependency graph satisfies every layering rule");
        return ExitCode::SUCCESS;
    }

    for violation in &report.violations {
        let _ = writeln!(
            out,
            "  VIOLATION: {} — {}",
            violation.crate_name, violation.message
        );
    }
    let _ = writeln!(out, "{} architecture violation(s)", report.violations.len());
    ExitCode::FAILURE
}
