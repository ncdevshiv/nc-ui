//! The `ncui` command line interface.
//!
//! NC-UI's guiding rule is that an agent — or a person — should be able to *ask the project
//! what state it is in* rather than infer it by reading source. This binary is that
//! interface: every subcommand answers a question and can emit machine-readable JSON, so
//! the same command serves a human at a terminal and an agent in a loop.
//!
//! # Scope, honestly stated
//!
//! These commands run **in-process**: they start a local runtime, exercise it, and report.
//! They do not yet connect to a running application over a socket, because the transport
//! layer is not implemented (see the feature registry). That limitation is reported by
//! `ncui status` rather than hidden, because a command that quietly inspected a different
//! runtime would be worse than one that admits it is local.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "test assertions must abort loudly on failure"
    )
)]

pub mod commands;
pub mod render;
pub mod usage;

use core::fmt;

use serde_json::Value;

/// A parsed command line.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Print the runtime version and build information.
    Version,
    /// Report the overall health of the local runtime.
    Health {
        /// Whether to include per-subsystem detail.
        deep: bool,
    },
    /// Describe the runtime, its methods, roles and capabilities.
    Describe,
    /// List every capability, grouped by scope.
    Capabilities,
    /// List the agent roles and the grants they carry.
    Roles,
    /// Report which features are complete and which are absent.
    Status {
        /// The parsed feature registry, when one was supplied.
        registry: Option<Value>,
    },
    /// Serve the agent API on a loopback socket until interrupted.
    Serve {
        /// The agent role to grant. Defaults to `observer`.
        role: Option<String>,
        /// The feature registry to serve through `ncui.feature.status`.
        ///
        /// A running runtime has no registry on disk, so it must be supplied for that
        /// method to answer over the socket rather than reporting that none is loaded.
        registry: Option<Value>,
    },
    /// Print usage.
    Help,
}

/// How a command's result should be rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Human-readable text.
    Text,
    /// Machine-readable JSON.
    Json,
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Text => "text",
            Self::Json => "json",
        })
    }
}

/// The outcome of running a command.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The command produced `payload`, rendered as `format`.
    Ok {
        /// The raw result.
        payload: Value,
        /// The rendering chosen by the caller.
        format: Format,
        /// Whether the command signals a problem in the exit code.
        ///
        /// A `health` command that finds a degraded runtime must be able to exit non-zero
        /// so a CI job or a probe can act on it, without losing the report.
        failed: bool,
    },
    /// The command could not run.
    Err(String),
}

impl Outcome {
    /// Whether the command succeeded.
    #[must_use]
    pub const fn is_ok(&self) -> bool {
        matches!(self, Self::Ok { .. })
    }

    /// The exit code this outcome implies.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Ok { failed, .. } => i32::from(*failed),
            Self::Err(_) => 2,
        }
    }
}

/// Every subcommand `parse` accepts, with its summary.
///
/// The usage text is rendered from this table and the usage tests assert against it, so a
/// subcommand that is implemented but undocumented fails the build rather than shipping
/// undocumented.
pub const SUBCOMMANDS: &[(&str, &str)] = &[
    (
        "status",
        "Report which features are complete and which are absent",
    ),
    (
        "health",
        "Report runtime health (--deep for per-subsystem detail)",
    ),
    (
        "describe",
        "Describe the runtime, its methods, roles and capabilities",
    ),
    ("capabilities", "List every capability, grouped by scope"),
    ("roles", "List the agent roles and the grants they carry"),
    (
        "serve",
        "Serve the agent API on a loopback socket (--role <name>)",
    ),
    ("version", "Print runtime, API and JSON-RPC versions"),
    ("help", "Print the usage message"),
];

/// Flags each subcommand accepts, beyond the global ones.
///
/// A flag outside a subcommand's set is refused rather than silently ignored. `SUBCOMMANDS`
/// and the usage text both scope every flag to one command, and `ncui describe --role x`
/// used to exit 0 having discarded the flag — a caller could believe it had selected a role
/// when it had done nothing at all. The registry's own `failure_tests` claim is that "an
/// unknown command, flag or missing value is each refused", and a valid flag on the wrong
/// command is that last case wearing a disguise.
const FLAG_SCOPES: &[(&str, &[&str])] = &[
    ("status", &["--registry"]),
    ("health", &["--deep", "-d"]),
    ("serve", &["--role", "--registry"]),
    ("describe", &[]),
    ("capabilities", &[]),
    ("roles", &[]),
    ("version", &[]),
    ("help", &[]),
];

/// The flag every subcommand accepts, because it selects output format rather than a
/// subcommand's behaviour.
pub const GLOBAL_FLAGS: &[&str] = &["--json"];

/// Whether `subcommand` accepts `flag`.
fn accepts_flag(subcommand: &str, flag: &str) -> bool {
    if GLOBAL_FLAGS.contains(&flag) {
        return true;
    }
    FLAG_SCOPES
        .iter()
        .find(|(name, _)| *name == subcommand)
        .is_some_and(|(_, flags)| flags.contains(&flag))
}

/// The error naming a flag used on a subcommand that does not take it.
fn wrong_subcommand_flag(subcommand: &str, flag: &str) -> String {
    let elsewhere = FLAG_SCOPES
        .iter()
        .find(|(_, flags)| flags.contains(&flag))
        .map_or("no subcommand", |(name, _)| *name);
    format!("`{flag}` is not a flag of `{subcommand}`; it belongs to `{elsewhere}`")
}

/// Takes the value that follows `flag`, refusing a following flag rather than consuming it.
///
/// `--role --json` used to bind `--json` as the role name, so a typo produced
/// `cannot read registry --json: ...` — a missing-file error describing a malformed command
/// line. A value that begins with `-` is a flag, not a value.
fn take_value<'a>(
    rest: &[&'a String],
    index: usize,
    flag: &str,
    what: &str,
) -> Result<&'a str, String> {
    match rest.get(index + 1) {
        Some(value) if !value.starts_with('-') => Ok(value.as_str()),
        Some(_) => Err(format!(
            "`{flag}` needs {what}, but the next argument is a flag"
        )),
        None => Err(format!("`{flag}` needs {what}")),
    }
}

/// Parses command line arguments.
///
/// # Errors
///
/// Returns a message for an unknown subcommand, an unknown flag, a flag used on a
/// subcommand that does not take it, or a flag missing its value.
pub fn parse<I, S>(args: I) -> Result<Command, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args: Vec<String> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
    let mut iter = args.iter();

    let Some(subcommand) = iter.next() else {
        return Ok(Command::Help);
    };

    let rest: Vec<&String> = iter.collect();
    let mut deep = false;
    let mut role: Option<String> = None;
    let mut registry: Option<Value> = None;
    let mut index = 0;
    while index < rest.len() {
        let flag = rest[index].as_str();
        // Scoped after the unknown-flag check so a typo reports itself as unknown rather
        // than as "not a flag of this command", which would send the caller looking for a
        // flag that exists on some other subcommand.
        let known = GLOBAL_FLAGS.contains(&flag)
            || flag == "--deep"
            || flag == "-d"
            || flag == "--role"
            || flag == "--registry";
        if !known {
            return Err(format!("unknown flag `{flag}` for `{subcommand}`"));
        }
        if !accepts_flag(subcommand, flag) {
            return Err(wrong_subcommand_flag(subcommand, flag));
        }
        match flag {
            "--deep" | "-d" => deep = true,
            "--role" => {
                let value = take_value(&rest, index, "--role", "a role name")?;
                role = Some(value.to_owned());
                index += 1;
            }
            "--registry" => {
                let path =
                    take_value(&rest, index, "--registry", "a path to the feature registry")?;
                let raw = std::fs::read_to_string(path)
                    .map_err(|e| format!("cannot read registry {path}: {e}"))?;
                registry = Some(
                    serde_json::from_str(&raw)
                        .map_err(|e| format!("registry {path} is malformed: {e}"))?,
                );
                index += 1;
            }
            // `--json` selects the output format and is read separately by `format_from`,
            // which deliberately scans every argument so the flag is honoured wherever it
            // appears. It is accepted here so a command line using it is not refused; the
            // format itself is applied by the caller.
            "--json" => {}
            other => return Err(format!("unknown flag `{other}` for `{subcommand}`")),
        }
        index += 1;
    }

    match subcommand.as_str() {
        "serve" => Ok(Command::Serve { role, registry }),
        "version" | "--version" | "-V" => Ok(Command::Version),
        "health" => Ok(Command::Health { deep }),
        "describe" => Ok(Command::Describe),
        "capabilities" => Ok(Command::Capabilities),
        "roles" => Ok(Command::Roles),
        "status" => Ok(Command::Status { registry }),
        "help" | "--help" | "-h" => Ok(Command::Help),
        other => Err(format!(
            "unknown command `{other}`; run `ncui help` for the list"
        )),
    }
}

/// Chooses the output format from the remaining arguments.
///
/// Kept separate from [`parse`] so a `--json` flag is honoured wherever it appears, rather
/// than only in one position.
#[must_use]
pub fn format_from<I, S>(args: I) -> Format
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    if args.into_iter().any(|a| a.as_ref() == "--json") {
        Format::Json
    } else {
        Format::Text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_arguments_asks_for_help_rather_than_guessing() {
        assert_eq!(parse(Vec::<String>::new()).expect("parse"), Command::Help);
    }

    #[test]
    fn every_advertised_subcommand_parses() {
        for (name, _) in SUBCOMMANDS {
            assert!(
                parse([*name]).is_ok(),
                "`{name}` is advertised in the usage text but `parse` rejects it"
            );
        }
    }

    #[test]
    fn subcommands_parse() {
        assert_eq!(parse(["version"]).expect("parse"), Command::Version);
        assert_eq!(parse(["describe"]).expect("parse"), Command::Describe);
        assert_eq!(
            parse(["capabilities"]).expect("parse"),
            Command::Capabilities
        );
        assert_eq!(parse(["roles"]).expect("parse"), Command::Roles);
        assert_eq!(
            parse(["health"]).expect("parse"),
            Command::Health { deep: false }
        );
        assert_eq!(
            parse(["health", "--deep"]).expect("parse"),
            Command::Health { deep: true }
        );
    }

    #[test]
    fn an_unknown_command_is_refused_with_a_useful_message() {
        let err = parse(["frobnicate"]).expect_err("unknown command");
        assert!(err.contains("frobnicate"), "got: {err}");
        assert!(err.contains("ncui help"), "got: {err}");
    }

    #[test]
    fn an_unknown_flag_is_refused_rather_than_ignored() {
        let err = parse(["health", "--verbose"]).expect_err("unknown flag");
        assert!(err.contains("--verbose"), "got: {err}");
    }

    #[test]
    fn a_flag_missing_its_value_is_refused() {
        let err = parse(["status", "--registry"]).expect_err("missing value");
        assert!(err.contains("--registry"), "got: {err}");
    }

    #[test]
    fn json_format_is_honoured_from_anywhere_on_the_line() {
        assert_eq!(format_from(["health", "--json"]), Format::Json);
        assert_eq!(format_from(["--json", "health"]), Format::Json);
        assert_eq!(format_from(["health"]), Format::Text);
    }

    /// `--json` is advertised by the usage text and is the flag an agent depends on.
    ///
    /// It was accepted by nothing: `format_from` scanned every argument for it while
    /// `parse` refused it as an unknown flag, so `Format::Json` was reachable only from
    /// tests that called `format_from` directly and bypassed `parse`. The usage text's own
    /// `EXAMPLE:` line — `ncui status --registry … --json` — exited 2 when run verbatim, and
    /// the registry certified this surface as available. The whole machine-readable output
    /// was dark.
    #[test]
    fn the_advertised_json_flag_is_accepted_by_every_subcommand() {
        for (name, _) in SUBCOMMANDS {
            // `serve` blocks and needs a registry-free argument shape, but parsing must
            // still accept the flag; only `parse` is exercised here.
            assert!(
                parse([*name, "--json"]).is_ok(),
                "`{name} --json` is advertised in the usage text but `parse` refuses it"
            );
        }
        // And the flag is a no-op for parsing: it must not become a subcommand's argument.
        assert_eq!(
            parse(["version", "--json"]).expect("parse"),
            Command::Version,
            "`--json` selects format and must not alter the command"
        );
    }

    /// The example the tool prints for itself must actually parse.
    ///
    /// A usage example that fails is worse than no example, because it is the first thing a
    /// reader copies. This checks *parsing* only: `--registry` reads the file as a side
    /// effect of parsing, so the example's own relative path is checked for argument shape
    /// rather than for existence — a missing registry is a working-directory question, not a
    /// defect in the usage text.
    #[test]
    fn the_usage_text_example_parses() {
        let mut examples = 0;
        for line in usage::text().lines() {
            let line = line.trim();
            let Some(rest) = line.strip_prefix("ncui ") else {
                continue;
            };
            if !rest.starts_with(char::is_alphabetic) {
                continue;
            }
            examples += 1;
            let mut args: Vec<String> = rest.split_whitespace().map(str::to_owned).collect();
            // Stand in a real file for the registry so parsing reaches the flag handling
            // rather than stopping at the filesystem.
            if let Some(at) = args.iter().position(|a| a == "--registry") {
                let fixture = temp_registry();
                if let Some(path) = args.get_mut(at + 1) {
                    *path = fixture.to_string_lossy().into_owned();
                }
            }
            let parsed = parse(&args);
            assert!(
                parsed.is_ok(),
                "the usage text advertises `{rest}`, which does not parse: {parsed:?}"
            );
        }
        assert!(
            examples > 0,
            "the usage text advertises no examples to check"
        );
    }

    /// Writes a minimal valid registry to a per-run scratch file and returns its path.
    ///
    /// The name carries a per-call nonce as well as the process id. Two tests in this binary
    /// call it and cargo runs tests in parallel, so a name derived from the pid alone had both
    /// writing the same path: one truncated the file while the other parsed it, and a flag
    /// test failed intermittently with an empty registry.
    ///
    /// The file is left in the OS temp directory rather than removed on drop. Removing it at
    /// the end of the `if` deleted it before `parse` had read the path; keeping the guard
    /// alive across `parse` needed a binding clippy rejects as unused. A uniquely-named file
    /// in the system temp directory cannot collide, so leaving it is the smaller cost.
    fn temp_registry() -> std::path::PathBuf {
        use std::io::Write;
        use std::sync::atomic::{AtomicU32, Ordering};
        static NONCE: AtomicU32 = AtomicU32::new(0);

        let path = std::env::temp_dir().join(format!(
            "ncui-cli-usage-example-{}-{}.json",
            std::process::id(),
            NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = std::fs::File::create(&path).expect("fixture file");
        write!(file, "{{\"schema_version\":1,\"features\":[]}}").expect("fixture write");
        path
    }

    /// A flag belonging to another subcommand is refused rather than silently discarded.
    ///
    /// `ncui describe --role administrator` exited 0 having thrown the flag away, so a
    /// caller could believe it had selected a role when it had changed nothing. Silently
    /// ignoring an argument is the one failure mode a command line must not have.
    #[test]
    fn a_flag_belonging_to_another_subcommand_is_refused() {
        for (args, misplaced, owner) in [
            (vec!["describe", "--role", "x"], "--role", "serve"),
            (vec!["version", "--role", "x"], "--role", "serve"),
            (
                vec!["health", "--registry", "protocols/feature-registry.json"],
                "--registry",
                "status",
            ),
            (vec!["status", "--deep"], "--deep", "health"),
            (vec!["serve", "--deep"], "--deep", "health"),
        ] {
            let err = parse(&args).expect_err(&format!("{args:?} should be refused"));
            assert!(
                err.contains(misplaced),
                "the error must name the misplaced flag; got: {err}"
            );
            assert!(
                err.contains(owner),
                "the error must say which subcommand does take it; got: {err}"
            );
        }
    }

    /// Every flag in `SUBCOMMANDS`' documented scope is accepted by that subcommand.
    ///
    /// The scoping table and the usage text are separate sources of truth today; this ties
    /// the parser to the table so a flag cannot be added to one and forgotten in the other.
    #[test]
    fn each_flag_is_accepted_by_exactly_the_subcommands_that_advertise_it() {
        let all_flags: Vec<&str> = FLAG_SCOPES
            .iter()
            .flat_map(|(_, f)| f.iter().copied())
            .collect();
        // `--registry` reads the file while parsing, so scoping it needs a real one.
        let registry = temp_registry();
        let registry_arg = registry.to_string_lossy().into_owned();
        for (name, own_flags) in FLAG_SCOPES {
            for flag in &all_flags {
                // A value-taking flag needs an argument before it can parse at all.
                let line: Vec<String> = if *flag == "--role" {
                    vec![
                        (*name).to_owned(),
                        (*flag).to_owned(),
                        "a-role-name".to_owned(),
                    ]
                } else if *flag == "--registry" {
                    vec![(*name).to_owned(), (*flag).to_owned(), registry_arg.clone()]
                } else {
                    vec![(*name).to_owned(), (*flag).to_owned()]
                };
                if own_flags.contains(flag) {
                    assert!(
                        parse(&line).is_ok(),
                        "`{name} {flag}` is scoped to `{name}` but does not parse"
                    );
                } else {
                    assert!(
                        parse(&line).is_err(),
                        "`{name} {flag}` belongs to another subcommand but was accepted"
                    );
                }
            }
        }
    }

    /// A flag does not swallow the next flag as its value.
    ///
    /// `--registry --json` bound `--json` as the registry path and then reported
    /// `cannot read registry --json: …` — a missing-file error describing a malformed
    /// command line, which sends the reader looking at the filesystem instead of the
    /// command.
    #[test]
    fn a_flag_does_not_swallow_the_next_flag_as_its_value() {
        let err = parse(["status", "--registry", "--json"]).expect_err("flag as value");
        assert!(
            err.contains("--registry") && err.contains("flag"),
            "the error must name the flag and say the next argument is a flag; got: {err}"
        );
        assert!(
            !err.contains("cannot read registry"),
            "a malformed command line must not be reported as an unreadable file; got: {err}"
        );
        // Same shape for the other value-taking flag.
        let err = parse(["serve", "--role", "--json"]).expect_err("flag as value");
        assert!(err.contains("--role") && err.contains("flag"), "got: {err}");
    }

    #[test]
    fn exit_codes_distinguish_success_failure_and_bad_usage() {
        let ok = |failed| Outcome::Ok {
            payload: Value::Null,
            format: Format::Text,
            failed,
        };
        assert_eq!(ok(false).exit_code(), 0);
        assert_eq!(ok(true).exit_code(), 1);
        assert_eq!(Outcome::Err("bad".to_owned()).exit_code(), 2);
        assert!(ok(false).is_ok());
        assert!(!Outcome::Err("x".to_owned()).is_ok());
    }
}
