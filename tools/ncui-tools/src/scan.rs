//! The scanner that enforces the zero-unfinished-work policy (spec 40, 78).
//!
//! Production code in NC-UI must not contain markers of unwritten work. This scanner
//! enforces that mechanically rather than by reviewer discipline, because a policy that
//! depends on someone remembering to look is not a policy.
//!
//! Two design choices are deliberate:
//!
//! * **The banned tokens are assembled at runtime from fragments.** A scanner whose own
//!   source spells out the tokens it forbids would flag itself on every run, and the
//!   obvious "fix" — exempting the scanner's own directory — creates a blind spot where real
//!   findings hide. Assembling them keeps the rule honest. The scanner's behavioural tests
//!   live in `tests/`, which the policy exempts, so they can use real tokens without
//!   weakening the rule.
//! * **Suppression requires a written reason.** A line may opt out with an inline
//!   `ncui-allow-placeholder:` annotation, but only if the annotation carries non-empty
//!   text. An empty or reasonless suppression is itself a finding.

use std::fs;
use std::path::{Path, PathBuf};

/// Directories never scanned, because they hold build output or vendored code rather
/// than authored source.
const ALWAYS_SKIPPED: &[&str] = &[
    "target",
    "node_modules",
    ".git",
    ".nc-tools",
    "dist",
    "build",
    ".venv",
    "__pycache__",
];

/// Directories whose contents are exempt by policy (spec 40) but which are still
/// *reported*, so an exemption is visible in CI output rather than silent.
const POLICY_EXEMPT: &[&str] = &["tests", "fixtures", "examples", "docs", "benches", "fuzz"];

/// A banned marker.
#[derive(Debug, Clone, Copy)]
pub struct Pattern {
    /// The token as it is matched, reconstructed from fragments at runtime.
    token: &'static str,
    /// Why this token is banned, quoted into the finding.
    rationale: &'static str,
}

/// Builds the banned-token table.
///
/// Each token is spelled as two fragments so that the literal never appears contiguously
/// in this file.
fn patterns() -> Vec<Pattern> {
    let join = |a: &str, b: &str| -> String { format!("{a}{b}") };
    vec![
        Pattern {
            token: Box::leak(join("TO", "DO").into_boxed_str()),
            rationale: "work left undone",
        },
        Pattern {
            token: Box::leak(join("FIX", "ME").into_boxed_str()),
            rationale: "work left undone",
        },
        Pattern {
            token: Box::leak(join("TB", "D").into_boxed_str()),
            rationale: "decision not yet taken",
        },
        Pattern {
            token: Box::leak(join("XX", "X").into_boxed_str()),
            rationale: "work left undone",
        },
        Pattern {
            token: Box::leak(join("HA", "CK").into_boxed_str()),
            rationale: "work left undone",
        },
        Pattern {
            token: Box::leak(join("Not", "Implemented").into_boxed_str()),
            rationale: "trait member with no behaviour",
        },
        Pattern {
            token: Box::leak(join("unimplemented", "!()").into_boxed_str()),
            rationale: "panicking marker macro",
        },
        Pattern {
            token: Box::leak(join("to", "do!()").into_boxed_str()),
            rationale: "panicking marker macro",
        },
        Pattern {
            token: Box::leak(join("place", "holder").into_boxed_str()),
            rationale: "work left undone",
        },
        Pattern {
            token: Box::leak(join("stu", "b").into_boxed_str()),
            rationale: "non-functional stand-in",
        },
        Pattern {
            token: Box::leak(join("fa", "ke").into_boxed_str()),
            rationale: "non-functional stand-in; production must not depend on them",
        },
        Pattern {
            token: Box::leak(join("dum", "my").into_boxed_str()),
            rationale: "non-functional stand-in",
        },
        Pattern {
            token: Box::leak(join("tem", "porary").into_boxed_str()),
            rationale: "explicitly transient implementation",
        },
        Pattern {
            token: Box::leak(join("de", "mo implementation").into_boxed_str()),
            rationale: "demonstration-only implementation",
        },
        Pattern {
            token: Box::leak(join("part", "ial implementation").into_boxed_str()),
            rationale: "known to be incomplete",
        },
    ]
}

/// The annotation that suppresses a finding on a line.
const SUPPRESSION: &str = "ncui-allow-placeholder"; // ncui-allow-placeholder: this constant is the documented suppression protocol token

/// Source file extensions worth scanning.
const EXTENSIONS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "mjs", "cjs", "toml", "yml", "yaml", "json", "sh", "ps1", "c",
    "h", "cpp", "hpp", "swift", "kt", "go", "zig", "py",
];

/// One prohibited marker found in source.
#[derive(Debug, Clone)]
pub struct Finding {
    /// File containing the finding, relative to the scan root.
    pub file: String,
    /// 1-based line number.
    pub line: usize,
    /// The offending token.
    pub token: String,
    /// Why it is banned.
    pub rationale: &'static str,
    /// The source line, trimmed.
    pub text: String,
}

/// A suppression that was attempted but not accepted.
#[derive(Debug, Clone)]
pub struct BadSuppression {
    /// File containing the suppression.
    pub file: String,
    /// 1-based line number.
    pub line: usize,
    /// Why the suppression was rejected.
    pub reason: String,
}

/// A file that was scanned but is exempt by policy.
#[derive(Debug, Clone)]
pub struct ExemptFile {
    /// File path relative to the scan root.
    pub file: String,
    /// Markers that were present but are permitted in this location.
    pub allowed_tokens: Vec<String>,
}

/// The complete result of a scan.
#[derive(Debug, Clone, Default)]
pub struct ScanReport {
    /// Prohibited markers in non-exempt files.
    pub findings: Vec<Finding>,
    /// Suppressions that lacked a reason.
    pub bad_suppressions: Vec<BadSuppression>,
    /// Exempt files that contained permitted markers, listed for visibility.
    pub exempt: Vec<ExemptFile>,
    /// Number of files actually scanned.
    pub files_scanned: usize,
    /// Scan roots that do not exist, so the caller cannot silently skip work.
    pub unreadable_roots: Vec<String>,
}

impl ScanReport {
    /// Whether the scan found anything that must block CI.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
            && self.bad_suppressions.is_empty()
            && self.unreadable_roots.is_empty()
    }
}

/// Scans `roots` recursively for prohibited markers.
#[must_use]
pub fn scan(roots: &[PathBuf]) -> ScanReport {
    let patterns = patterns();
    let mut report = ScanReport::default();
    for root in roots {
        if !root.is_dir() {
            // A root that does not exist used to be skipped silently, which let CI name a
            // directory that was never created and pass without scanning anything. A gate
            // that cannot fail for the reason it claims is worse than no gate.
            report
                .unreadable_roots
                .push(root.to_string_lossy().replace('\\', "/"));
            continue;
        }
        scan_path(root, root, &patterns, &mut report);
    }
    report
}

fn scan_path(root: &Path, dir: &Path, patterns: &[Pattern], report: &mut ScanReport) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    // Sorted so that scan output is deterministic across runs and filesystems.
    let mut paths: Vec<PathBuf> = entries
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .collect();
    paths.sort();

    for path in paths {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if path.is_dir() {
            if ALWAYS_SKIPPED.contains(&name) {
                continue;
            }
            scan_path(root, &path, patterns, report);
        } else if is_scannable(&path) {
            scan_file(root, &path, patterns, report);
        }
    }
}

fn is_scannable(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| EXTENSIONS.contains(&ext))
}

/// Whether this file is policy-exempt.
///
/// Only the **first** path component is consulted. An earlier version asked whether *any*
/// component matched, which meant production source hid in any directory that happened to
/// be called `docs` or `tests` — `crates/ncui-foo/src/docs/lib.rs` was silently exempt.
/// Exemption is a property of where a file sits at the top of the tree, not of a word
/// appearing anywhere in its path.
fn is_policy_exempt(root: &Path, file: &Path) -> bool {
    let Ok(relative) = file.strip_prefix(root) else {
        return false;
    };
    relative
        .components()
        .next()
        .and_then(|component| component.as_os_str().to_str())
        .is_some_and(|name| POLICY_EXEMPT.contains(&name))
}

fn scan_file(root: &Path, file: &Path, patterns: &[Pattern], report: &mut ScanReport) {
    let Ok(contents) = fs::read_to_string(file) else {
        return;
    };
    let display = relative_display(root, file);
    let exempt = is_policy_exempt(root, file);
    let mut allowed_here: Vec<String> = Vec::new();

    for (index, line) in contents.lines().enumerate() {
        let number = index + 1;

        if suppression_reason(line).is_some_and(|reason| reason.is_empty()) {
            report.bad_suppressions.push(BadSuppression {
                file: display.clone(),
                line: number,
                reason: format!("`{SUPPRESSION}` requires a written justification after the colon"),
            });
        }
        let suppressed = suppression_reason(line).is_some_and(|r| !r.is_empty());

        for pattern in patterns {
            if !contains_token(line, pattern.token) {
                continue;
            }
            if exempt {
                allowed_here.push(pattern.token.to_owned());
                continue;
            }
            if suppressed {
                continue;
            }
            report.findings.push(Finding {
                file: display.clone(),
                line: number,
                token: pattern.token.to_owned(),
                rationale: pattern.rationale,
                text: line.trim().chars().take(200).collect(),
            });
        }
    }

    if !allowed_here.is_empty() {
        allowed_here.sort();
        allowed_here.dedup();
        report.exempt.push(ExemptFile {
            file: display,
            allowed_tokens: allowed_here,
        });
    }
    report.files_scanned += 1;
}

/// Extracts the justification from an inline suppression annotation, if present.
///
/// Returns `Some(text)` when the annotation appears; the text is the remainder after the
/// colon, trimmed. A bare annotation yields an empty string, which the caller treats as
/// an invalid suppression.
fn suppression_reason(line: &str) -> Option<String> {
    let index = line.find(SUPPRESSION)?;
    Some(
        line[index + SUPPRESSION.len()..]
            .trim_start_matches([':', '='])
            .trim()
            .to_owned(),
    )
}

/// Whether `line` contains `token`, ignoring case, delimited by non-identifier characters.
///
/// Delimiter rules matter: a banned marker must match a standalone word but not a longer
/// identifier that merely starts with it, nor a variable name embedding it.
///
/// Case is ignored because the cost is nil and the alternative is a trivial evasion:
/// differently-cased spellings of every marker used to pass this check.
fn contains_token(line: &str, token: &str) -> bool {
    let line = line.to_ascii_lowercase();
    let line = line.as_str();
    let token = token.to_ascii_lowercase();
    let token = token.as_str();
    let bytes = line.as_bytes();
    let needle = token.as_bytes();
    if needle.is_empty() || bytes.len() < needle.len() {
        return false;
    }

    for start in 0..=bytes.len() - needle.len() {
        if &bytes[start..start + needle.len()] != needle {
            continue;
        }
        let before_ok = start == 0 || !is_identifier_byte(bytes[start - 1]);
        let end = start + needle.len();
        let after_ok = end == bytes.len() || !is_identifier_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
    }
    false
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn relative_display(root: &Path, file: &Path) -> String {
    file.strip_prefix(root)
        .unwrap_or(file)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "test assertions must abort loudly on failure"
    )]

    use super::*;

    #[test]
    fn suppression_annotation_parsing_behaves() {
        assert_eq!(
            suppression_reason("let x = 1; // ncui-allow-placeholder: needs OS entropy"),
            Some("needs OS entropy".to_owned())
        );
        assert_eq!(
            suppression_reason("let x = 1; // ncui-allow-placeholder:"),
            Some(String::new())
        );
        assert_eq!(suppression_reason("let x = 1;"), None);
    }

    #[test]
    fn every_pattern_is_distinct() {
        let mut tokens: Vec<&str> = patterns().iter().map(|p| p.token).collect();
        tokens.sort_unstable();
        let before = tokens.len();
        tokens.dedup();
        assert_eq!(before, tokens.len(), "duplicate banned token");
    }
}
