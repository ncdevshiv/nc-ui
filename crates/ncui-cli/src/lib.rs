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
//! `ncui serve` is the exception — it binds the loopback socket and serves the same
//! surface to an agent in another process.
//!
//! A command that wants to inspect a *running* runtime says so rather than quietly
//! inspecting a different one: `ncui status` reports `scope: "local"`, and a command that
//! can reach a remote runtime names the address it reached. A command that quietly
//! inspected a different runtime would be worse than one that admits it is local.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "test assertions must abort loudly on failure"
    )
)]

pub mod bundle;
pub mod commands;
pub mod installer;
pub mod release;
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
    /// Run an application: serve its assets and open its window.
    Run {
        /// The directory holding the application's manifest and assets.
        directory: String,
        /// Whether to reload the page when the application's files change.
        watch: bool,
        /// The link the platform handed this launch, when it was started by one.
        ///
        /// A registered scheme runs `ncui run <directory> "<link>"`, so a deep link arrives as an
        /// argument. It is carried rather than ignored: a launch started by a link is a launch
        /// somebody asked for, and dropping the link would leave the user looking at an
        /// application that does not know why it opened.
        url: Option<String>,
    },
    /// Build a shippable bundle from an application directory.
    Build {
        /// The directory holding the application's manifest and assets.
        directory: String,
        /// Where the bundle is written. Defaults to `dist/<name>` under the working directory.
        out: Option<String>,
        /// The runtime binary to bundle. Defaults to a release build of this workspace.
        binary: Option<String>,
        /// Whether to compare the bundle already in `--out` with a fresh build instead of writing.
        check: bool,
        /// Whether to write a self-contained installer beside the bundle.
        ///
        /// The installer is this runtime with the bundle appended, so it is one file a user can run
        /// rather than a directory they have to be told what to do with.
        installer: bool,
    },
    /// Install an application bundle for this user.
    Install {
        /// The directory holding the bundle.
        bundle: String,
        /// Where applications are installed. Defaults to this user's own directory.
        root: Option<String>,
    },
    /// Replace an installed application with another version of itself.
    Update {
        /// The directory holding the bundle of the new version, when one was named.
        bundle: Option<String>,
        /// The URL of a release document to fetch the new version from, when one was named.
        from: Option<String>,
        /// Where applications are installed. Defaults to this user's own directory.
        root: Option<String>,
    },
    /// Remove an installed application, with the registrations it made.
    Uninstall {
        /// The application's name.
        name: String,
        /// Where applications are installed. Defaults to this user's own directory.
        root: Option<String>,
    },
    /// List the applications installed for this user.
    Installed {
        /// Where applications are installed. Defaults to this user's own directory.
        root: Option<String>,
    },
    /// Run an installed application by name.
    Launch {
        /// The application's name.
        name: String,
        /// Where applications are installed. Defaults to this user's own directory.
        root: Option<String>,
        /// The link this launch was started with, when it was started by one.
        link: Option<String>,
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
    (
        "build",
        "Build a shippable bundle from an application directory (--out, --binary, --check, --installer)",
    ),
    (
        "install",
        "Install an application bundle for this user (--root for somewhere else)",
    ),
    (
        "update",
        "Replace an installed application with another version of itself (--root for somewhere else, --from to fetch a release)",
    ),
    (
        "uninstall",
        "Remove an installed application, with its registrations",
    ),
    ("installed", "List the applications installed for this user"),
    ("launch", "Run an installed application by name"),
    ("capabilities", "List every capability, grouped by scope"),
    ("roles", "List the agent roles and the grants they carry"),
    (
        "serve",
        "Serve the agent API on a loopback socket (--role <name>)",
    ),
    (
        "run",
        "Run an application directory: serve its assets and open its window",
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
    ("run", &["--watch"]),
    ("build", &["--out", "--binary", "--check", "--installer"]),
    ("install", &["--root"]),
    ("update", &["--root", "--from"]),
    ("uninstall", &["--root"]),
    ("installed", &["--root"]),
    ("launch", &["--root"]),
    ("describe", &[]),
    ("capabilities", &[]),
    ("roles", &[]),
    ("version", &[]),
    ("help", &[]),
];

/// What a command line's flags said.
#[derive(Debug, Default)]
struct Flags {
    /// Whether per-subsystem detail was asked for.
    deep: bool,
    /// Whether the page should reload when the application's files change.
    watch: bool,
    /// The agent role to grant, when one was named.
    role: Option<String>,
    /// The parsed feature registry, when one was supplied.
    registry: Option<Value>,
    /// Where applications are installed, when somewhere else was named.
    root: Option<String>,
    /// Where a bundle is written, when somewhere else was named.
    out: Option<String>,
    /// The runtime binary to bundle, when one was named.
    binary: Option<String>,
    /// Whether a bundle should be compared with a fresh build rather than written.
    check: bool,
    /// Whether a self-contained installer should be written beside the bundle.
    installer: bool,
    /// The URL of a release document to update from, when one was named.
    from: Option<String>,
}

/// Reads a command line's flags, refusing any the subcommand does not accept.
///
/// Separate from `parse` because the positional arguments and the flags are different questions
/// with different rules — a positional is taken before the loop, and a flag is scoped to the
/// subcommand that advertises it — and keeping them in one function made the *reading* of a
/// command line the longest thing in it.
fn parse_flags(rest: &[&String], subcommand: &str) -> Result<Flags, String> {
    let mut flags = Flags::default();
    let mut index = 0;
    while index < rest.len() {
        let flag = rest[index].as_str();
        // Scoped after the unknown-flag check so a typo reports itself as unknown rather
        // than as "not a flag of this command", which would send the caller looking for a
        // flag that exists on some other subcommand.
        // Derived from the scopes rather than listed again: a second list of the flags that exist
        // drifted from `FLAG_SCOPES` the first time a flag was added to one and not the other, and
        // the drift showed up as a flag its own subcommand advertised being reported as unknown.
        let known = GLOBAL_FLAGS.contains(&flag)
            || FLAG_SCOPES.iter().any(|(_, flags)| flags.contains(&flag));
        if !known {
            return Err(format!("unknown flag `{flag}` for `{subcommand}`"));
        }
        if !accepts_flag(subcommand, flag) {
            return Err(wrong_subcommand_flag(subcommand, flag));
        }
        match flag {
            "--deep" | "-d" => flags.deep = true,
            "--watch" => flags.watch = true,
            "--root" => {
                index += 1;
                let value = rest
                    .get(index)
                    .ok_or_else(|| "`--root` needs a directory".to_owned())?;
                flags.root = Some((*value).clone());
            }
            "--check" => flags.check = true,
            "--from" => {
                let value = take_value(rest, index, "--from", "a URL")?;
                flags.from = Some(value.to_owned());
                index += 1;
            }
            "--installer" => flags.installer = true,
            "--out" => {
                let value = take_value(rest, index, "--out", "a directory")?;
                flags.out = Some(value.to_owned());
                index += 1;
            }
            "--binary" => {
                let value = take_value(rest, index, "--binary", "a path to the runtime")?;
                flags.binary = Some(value.to_owned());
                index += 1;
            }
            "--role" => {
                let value = take_value(rest, index, "--role", "a role name")?;
                flags.role = Some(value.to_owned());
                index += 1;
            }
            "--registry" => {
                let path = take_value(rest, index, "--registry", "a path to the feature registry")?;
                let raw = std::fs::read_to_string(path)
                    .map_err(|e| format!("cannot read registry {path}: {e}"))?;
                flags.registry = Some(
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
    Ok(flags)
}

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
/// Takes the positional argument a subcommand requires, if it requires one.
///
/// Taken before the flag loop, for the reason `run`'s directory is: a flag loop that met a bare
/// path would report it as an unknown flag, which is a message about a typo when the caller had
/// given exactly what was asked for.
fn take_positional(rest: &mut Vec<&String>, subcommand: &str) -> Result<Option<String>, String> {
    if !matches!(
        subcommand,
        "build" | "install" | "update" | "uninstall" | "launch"
    ) {
        return Ok(None);
    }
    // An update can name a URL instead of a directory, so the positional is not required when
    // `--from` is given. It is still taken when it comes *before* the first flag, because
    // `update <dir> --from <url>` names two sources and the command refuses that with a message
    // saying so rather than silently choosing one.
    if subcommand == "update" && rest.iter().any(|argument| *argument == "--from") {
        let first_flag = rest
            .iter()
            .position(|argument| argument.starts_with('-'))
            .unwrap_or(rest.len());
        return Ok(rest
            .iter()
            .position(|argument| !argument.starts_with('-'))
            .filter(|index| *index < first_flag)
            .map(|index| rest.remove(index).clone()));
    }
    match rest.iter().position(|argument| !argument.starts_with('-')) {
        Some(index) => Ok(Some(rest.remove(index).clone())),
        None => Err(format!(
            "`ncui {subcommand}` needs {}",
            match subcommand {
                "build" => "the directory of the application to build",
                "install" => "the directory of the bundle to install",
                "update" => "the directory of the bundle to update to",
                "launch" => "the name of the application to run",
                _ => "the name of the application to remove",
            }
        )),
    }
}

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

    let mut rest: Vec<&String> = iter.collect();

    // `run` takes the application's directory as a positional argument. It is found
    // before the flag loop, wherever it appears, because a flag loop that met a bare path
    // would report it as an unknown flag — a message about a typo when the user had given
    // exactly what was asked for.
    let mut directory = String::from(".");
    let mut url: Option<String> = None;
    if subcommand == "run" {
        match rest.iter().position(|argument| !argument.starts_with('-')) {
            Some(index) => directory.clone_from(rest.remove(index)),
            None => {
                return Err("`ncui run` needs the directory of the application to run".to_owned());
            }
        }
        // A second positional is the link the platform handed this launch: a registered scheme
        // runs `ncui run <directory> "<link>"`, so the link arrives as an argument rather than as
        // something the runtime has to ask the operating system for. It is taken here, before the
        // flag loop, so a URL cannot be reported as an unknown flag.
        if let Some(index) = rest.iter().position(|argument| !argument.starts_with('-')) {
            url = Some(rest.remove(index).clone());
        }
    }

    let positional = take_positional(&mut rest, subcommand)?;
    let Flags {
        deep,
        watch,
        role,
        registry,
        root,
        out,
        binary,
        check,
        installer,
        from,
    } = parse_flags(&rest, subcommand)?;

    match subcommand.as_str() {
        "serve" => Ok(Command::Serve { role, registry }),
        "run" => Ok(Command::Run {
            directory,
            watch,
            url,
        }),
        "build" => Ok(Command::Build {
            directory: positional.unwrap_or_default(),
            out,
            binary,
            check,
            installer,
        }),
        "install" => Ok(Command::Install {
            bundle: positional.unwrap_or_default(),
            root,
        }),
        "update" => Ok(Command::Update {
            bundle: positional,
            from,
            root,
        }),
        "uninstall" => Ok(Command::Uninstall {
            name: positional.unwrap_or_default(),
            root,
        }),
        "installed" => Ok(Command::Installed { root }),
        "launch" => Ok(Command::Launch {
            name: positional.unwrap_or_default(),
            root,
            // A second positional is the link the platform handed this launch, as it is for
            // `run`: a registered scheme runs the installed application with the link appended.
            link: rest
                .iter()
                .find(|argument| !argument.starts_with('-'))
                .map(|argument| (*argument).clone()),
        }),
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
    use std::path::{MAIN_SEPARATOR_STR, Path, PathBuf};

    #[test]
    fn no_arguments_asks_for_help_rather_than_guessing() {
        assert_eq!(parse(Vec::<String>::new()).expect("parse"), Command::Help);
    }

    #[test]
    fn every_advertised_subcommand_parses() {
        for (name, _) in SUBCOMMANDS {
            assert!(
                parse(args_for(name)).is_ok(),
                "`{name}` is advertised in the usage text but `parse` rejects it"
            );
        }
    }

    /// The arguments that exercise a subcommand.
    ///
    /// `run` takes the application's directory, so it cannot be parsed bare: a command
    /// line that names a subcommand and nothing else is not a complete `run`.
    fn args_for(name: &str) -> Vec<String> {
        // The commands that take a positional need one here: a test that walked the advertised
        // commands without supplying their arguments would be checking the argument check rather
        // than the command.
        match name {
            "run" => vec!["run".to_owned(), "examples/app".to_owned()],
            "build" => vec!["build".to_owned(), "examples/app".to_owned()],
            "install" => vec!["install".to_owned(), "dist/app".to_owned()],
            "update" => vec!["update".to_owned(), "dist/app".to_owned()],
            "uninstall" => vec!["uninstall".to_owned(), "app".to_owned()],
            "launch" => vec!["launch".to_owned(), "app".to_owned()],
            _ => vec![name.to_owned()],
        }
    }

    /// The SHA-256 of a file, as lowercase hex.
    ///
    /// Written here rather than shared with the installer because a test that computed a digest with the
    /// code under test would be checking that code against itself.
    fn test_digest(path: &Path) -> String {
        use sha2::{Digest as _, Sha256};
        let bytes = std::fs::read(path).expect("the file is readable");
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let mut hex = String::new();
        for byte in hasher.finalize() {
            let _ = fmt::Write::write_fmt(&mut hex, format_args!("{byte:02x}"));
        }
        hex
    }

    /// The file records for a bundle's payload, in the shape the bundler writes them.
    fn test_records(root: &Path, paths: &[&str]) -> Value {
        let mut files = Vec::new();
        for path in paths {
            let full = root.join(path.replace('/', MAIN_SEPARATOR_STR));
            let bytes = std::fs::metadata(&full).expect("the file is there").len();
            files.push(serde_json::json!({
                "path": path,
                "bytes": bytes,
                "sha256": test_digest(&full),
            }));
        }
        Value::Array(files)
    }

    /// A bundle on disk, with the shape the bundler writes.
    ///
    /// Written here rather than produced by `ncui build` because what this test needs is the
    /// *shape* a bundle has, not a bundle: `crates/ncui-cli/tests/bundle.rs` builds real ones and
    /// asserts against what the bundler writes.
    fn temp_bundle(root: &Path, name: &str) -> PathBuf {
        let bundle = root.join(format!("{name}-bundle"));
        std::fs::create_dir_all(bundle.join("public")).expect("the bundle is creatable");
        std::fs::write(
            bundle.join("ncui.app.json"),
            br#"{"name":"counter","version":"1.0.0","entry":"index.html","assets":"public"}"#,
        )
        .expect("the application's manifest is writable");
        std::fs::write(bundle.join("public").join("index.html"), "<!doctype html>")
            .expect("the page is writable");
        std::fs::write(bundle.join("ncui.exe"), b"a runtime slot").expect("writable");
        // The records a bundle carries, written after the files they describe because that is the
        // order the bundler uses: it digests what it wrote.
        let records = test_records(&bundle, &["ncui.app.json", "public/index.html", "ncui.exe"]);
        std::fs::write(
            bundle.join("bundle.json"),
            format!(
                r#"{{"name":"{name}","version":"1.0.0","runtime":"ncui.exe","files":{records}}}"#
            ),
        )
        .expect("the bundle's manifest is writable");
        bundle
    }

    #[test]
    fn the_command_line_installs_lists_and_removes_an_application() {
        // The whole command path, not only the installer: parse, run the command, and read what
        // it reported. The root is a scratch directory so nothing is installed for the user
        // running the suite.
        let root = std::env::temp_dir().join(format!("ncui-cli-install-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("the scratch root is creatable");
        let bundle = temp_bundle(&root, "counter");
        let applications = root.join("applications");
        let applications_arg = applications.to_string_lossy().into_owned();
        let bundle_arg = bundle.to_string_lossy().into_owned();

        let (installed, _) =
            commands::install(&bundle_arg, Some(&applications_arg)).expect("the bundle installs");
        assert_eq!(installed["installed"]["name"], "counter");
        assert!(
            Path::new(applications_arg.as_str())
                .join("counter")
                .join("ncui.app.json")
                .is_file(),
            "the application must be where the command said it put it"
        );

        let (listed, _) = commands::installed(Some(&applications_arg)).expect("it lists");
        assert_eq!(listed["count"], 1);
        assert_eq!(listed["applications"][0]["name"], "counter");

        let (removed, _) =
            commands::uninstall("counter", Some(&applications_arg)).expect("it uninstalls");
        assert_eq!(removed["removed"]["name"], "counter");
        let (after, _) = commands::installed(Some(&applications_arg)).expect("it lists");
        assert_eq!(after["count"], 0);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_installed_application_is_resolved_by_name_and_only_by_name() {
        // What `launch` adds to `run` is the lookup, so the lookup is what this asserts: the
        // running itself is `run`, which the runtime's own tests drive end to end.
        let root = std::env::temp_dir().join(format!("ncui-cli-launch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("the scratch root is creatable");
        let bundle = temp_bundle(&root, "counter");
        let applications = root.join("applications");
        let applications_arg = applications.to_string_lossy().into_owned();

        let missing = commands::resolve_installed("counter", &applications);
        assert!(missing.is_err(), "nothing is installed yet");

        commands::install(&bundle.to_string_lossy(), Some(&applications_arg))
            .expect("the bundle installs");
        let found = commands::resolve_installed("counter", &applications)
            .expect("the installed application is found by name");
        assert_eq!(found.name, "counter");
        assert!(
            Path::new(&found.directory).join("ncui.app.json").is_file(),
            "the record must name the directory the application was installed into"
        );

        let unknown = commands::resolve_installed("something-else", &applications)
            .expect_err("an unknown name is refused");
        assert!(
            unknown.contains("is not installed under"),
            "the refusal must say what was searched: {unknown}"
        );
        assert!(
            unknown.contains("ncui installed"),
            "and point at how to find out what is: {unknown}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn run_needs_the_directory_of_the_application() {
        let error = parse(["run"]).expect_err("run without a directory");
        assert!(
            error.contains("directory"),
            "the refusal must say what is missing; got: {error}"
        );
        let error = parse(["run", "--json"]).expect_err("run with a flag instead");
        assert!(
            error.contains("directory"),
            "the refusal must say what is missing; got: {error}"
        );
        assert_eq!(
            parse(["run", "examples/app"]).expect("run parses"),
            Command::Run {
                directory: "examples/app".to_owned(),
                watch: false,
                url: None,
            }
        );
        assert_eq!(
            parse(["run", "examples/app", "--watch"]).expect("run --watch parses"),
            Command::Run {
                directory: "examples/app".to_owned(),
                watch: true,
                url: None,
            }
        );
        assert_eq!(
            parse(["run", "--watch", "examples/app"]).expect("the flag may come first"),
            Command::Run {
                directory: "examples/app".to_owned(),
                watch: true,
                url: None,
            },
            "a flag before the directory must not be mistaken for the directory"
        );
        assert_eq!(
            parse(["run", "examples/app", "ncui-counter://open/notes"]).expect("a link parses"),
            Command::Run {
                directory: "examples/app".to_owned(),
                watch: false,
                url: Some("ncui-counter://open/notes".to_owned()),
            },
            "the link a registered scheme passes must reach the command rather than be refused"
        );
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
            let mut line = args_for(name);
            line.push("--json".to_owned());
            assert!(
                parse(line).is_ok(),
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
    fn temp_registry() -> PathBuf {
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
                // A value-taking flag needs an argument before it can parse at all, and
                // `run` needs its application directory whenever it is given a flag.
                let line: Vec<String> = if *flag == "--role" {
                    vec![
                        (*name).to_owned(),
                        (*flag).to_owned(),
                        "a-role-name".to_owned(),
                    ]
                } else if *flag == "--registry" {
                    vec![(*name).to_owned(), (*flag).to_owned(), registry_arg.clone()]
                } else if matches!(*flag, "--root" | "--out" | "--binary" | "--from") {
                    // These take a value, and every command that accepts one also takes a
                    // positional: the bundle to install, the name to remove, or the application to
                    // build.
                    let mut line = vec![(*name).to_owned()];
                    if matches!(
                        *name,
                        "build" | "install" | "update" | "uninstall" | "launch"
                    ) {
                        line.push("something".to_owned());
                    }
                    line.push((*flag).to_owned());
                    line.push("C:/apps".to_owned());
                    line
                } else if matches!(*name, "run" | "build") {
                    vec![
                        (*name).to_owned(),
                        "examples/app".to_owned(),
                        (*flag).to_owned(),
                    ]
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
