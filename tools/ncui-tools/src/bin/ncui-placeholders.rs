//! `ncui-placeholders` — fails the build when prohibited markers of unwritten work are
//! found in production source.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use ncui_tools::scan;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        usage();
        return ExitCode::SUCCESS;
    }
    if args.is_empty() {
        usage();
        return ExitCode::FAILURE;
    }

    let roots: Vec<PathBuf> = args.iter().map(PathBuf::from).collect();
    let report = scan::scan(&roots);

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = writeln!(
        out,
        "ncui-placeholders: scanned {} files",
        report.files_scanned
    );

    for exempt in &report.exempt {
        let _ = writeln!(
            out,
            "  exempt: {} (permitted: {})",
            exempt.file,
            exempt.allowed_tokens.join(", ")
        );
    }

    for bad in &report.bad_suppressions {
        let _ = writeln!(
            out,
            "  INVALID SUPPRESSION: {}:{} — {}",
            bad.file, bad.line, bad.reason
        );
    }

    for finding in &report.findings {
        let _ = writeln!(
            out,
            "  {}:{} — `{}` ({})\n    {}",
            finding.file, finding.line, finding.token, finding.rationale, finding.text
        );
    }

    if report.is_clean() {
        let _ = writeln!(out, "no prohibited markers found");
        ExitCode::SUCCESS
    } else {
        let _ = writeln!(
            out,
            "{} prohibited marker(s), {} invalid suppression(s), {} unreadable root(s)",
            report.findings.len(),
            report.bad_suppressions.len(),
            report.unreadable_roots.len()
        );
        let _ = writeln!(
            out,
            "implement the work, or annotate the line with a written justification:\n  \
             // ncui-allow-placeholder: <why this is intentional>"
        );
        ExitCode::FAILURE
    }
}

fn usage() {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = writeln!(out, "usage: ncui-placeholders <path>...");
    let _ = writeln!(
        out,
        "\nScans the given roots recursively for markers of unwritten work."
    );
}
