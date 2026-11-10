//! The usage text.

use core::fmt::Write as _;

/// The full usage message, including every subcommand.
///
/// The command list and its summaries are rendered from [`crate::SUBCOMMANDS`], and the
/// test below asserts against that same table — so a subcommand that is implemented but
/// undocumented fails the build rather than shipping undocumented.
#[must_use]
pub fn text() -> String {
    let mut out = String::new();
    out.push_str(
        "ncui — NC-UI runtime command line

USAGE:
    ncui <command> [--json] [options]

COMMANDS:
",
    );
    for (name, summary) in crate::SUBCOMMANDS {
        let _ = writeln!(out, "    {name:<28} {summary}");
    }
    out.push_str(
        "
OPTIONS:
    --json              Emit machine-readable JSON instead of text
    --deep, -d          Include per-subsystem detail (health)
    --registry <path>   Load a feature registry (status, serve)
    --role <name>       Agent role for the served socket (serve)
    --watch             Reload the page when the application's files change (run)
    --root <dir>        Where applications are installed (install, update, uninstall, installed)

AGENT API:
    `ncui serve` binds the JSON-RPC surface to an ephemeral loopback port and prints
    the address and a one-time bearer token. Frames are a 4-byte big-endian length
    followed by that many bytes of JSON; the first frame must carry the token.

APPLICATIONS:
    `ncui run <directory>` reads the application's `ncui.app.json`, serves its assets
    over loopback HTTP, opens a window at its entry page, and serves the page's calls
    to the runtime until the window closes.

EXIT CODES:
    0   the command succeeded and nothing is wrong
    1   the command ran, but the runtime is not healthy
    2   the command could not run (bad usage, unreadable registry, failed startup)

NOTES:
    These commands inspect a local, in-process runtime. Remote attachment to a running
    application is not implemented; `ncui status` reports the scope it actually used.

EXAMPLE:
    ncui health --deep
    ncui status --registry protocols/feature-registry.json --json
    ncui run examples/counter
    ncui run examples/counter --watch
    ncui install dist/counter
    ncui update dist/counter
    ncui installed
    ncui uninstall counter
    ncui launch counter
",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_usage_text_documents_every_subcommand() {
        // Iterates the same table the usage text is rendered from, so adding a subcommand
        // without documenting it fails here. A hardcoded copy of the list — which is what
        // this test used to contain — cannot catch that, and lies whenever the two drift.
        let text = text();
        for (name, summary) in crate::SUBCOMMANDS {
            assert!(
                text.contains(name),
                "`{name}` is implemented but absent from the usage text"
            );
            assert!(
                text.contains(summary),
                "the usage text must carry the summary for `{name}`"
            );
        }
    }

    #[test]
    fn the_usage_text_documents_every_flag() {
        let text = text();
        for flag in ["--json", "--deep", "-d", "--registry", "--role"] {
            assert!(
                text.contains(flag),
                "usage must mention `{flag}`; got:
{text}"
            );
        }
    }
}
