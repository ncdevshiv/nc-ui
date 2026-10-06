//! `ncui-registry` — validates the feature registry and prints the project truth layer.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use ncui_tools::registry::{self, Registry};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = PathBuf::from(
        args.iter()
            .find(|a| !a.starts_with('-'))
            .cloned()
            .unwrap_or_else(|| "protocols/feature-registry.json".to_owned()),
    );

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) => {
            let _ = writeln!(
                out,
                "ncui-registry: cannot read {}: {error}",
                path.display()
            );
            return ExitCode::FAILURE;
        }
    };

    let registry: Registry = match serde_json::from_str(&raw) {
        Ok(registry) => registry,
        Err(error) => {
            let _ = writeln!(
                out,
                "ncui-registry: {} is malformed: {error}",
                path.display()
            );
            return ExitCode::FAILURE;
        }
    };

    let (complete, not_implemented) = registry::summarise(&registry);
    let _ = writeln!(
        out,
        "ncui-registry: {} tracked features",
        registry.features.len()
    );
    let _ = writeln!(out, "  complete:         {complete}");
    let _ = writeln!(out, "  not implemented:  {not_implemented}");

    // The repository root is derived from the registry's own location
    // (`<root>/protocols/feature-registry.json`) so that cited paths resolve without a
    // flag the caller could forget.
    let root = path
        .parent()
        .and_then(std::path::Path::parent)
        .unwrap_or_else(|| std::path::Path::new("."));
    let result = registry::validate(&registry, Some(root));

    // Outstanding platform verification is reported prominently rather than dropped: a
    // gap nobody can see is indistinguishable from work that does not exist.
    if !result.platform_gaps.is_empty() {
        let _ = writeln!(
            out,
            "  outstanding platform verification ({}):",
            result.platform_gaps.len()
        );
        for gap in &result.platform_gaps {
            let _ = writeln!(out, "    {} on {}", gap.feature_id, gap.platform);
        }
    }

    if result.is_clean() {
        let _ = writeln!(out, "every completed feature carries its required evidence");
        return ExitCode::SUCCESS;
    }

    for violation in &result.violations {
        let _ = writeln!(
            out,
            "  VIOLATION [{}]: {}",
            violation.feature_id, violation.message
        );
    }
    let _ = writeln!(out, "{} registry violation(s)", result.violations.len());
    ExitCode::FAILURE
}
