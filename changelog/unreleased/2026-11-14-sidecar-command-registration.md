- **A page can now call a command answered by any executable, in any language.** Native commands
were Rust-only: a Go, Python or Zig backend had no way to answer a page's call. An application can
now declare a **sidecar** in `ncui.app.json` — `sidecars: [{command, program, args, requires,
timeout_ms, summary}]` — and the runtime answers that command by running the named program with a
one-line-JSON-each-way contract: it writes the call payload to the program's standard input and
reads one JSON answer from its standard output. Anything that can read stdin and write stdout can
be a backend, in whatever language the author likes. This is the any-language half of C3.

The command is gated twice. An application must hold `process.spawn` for any sidecar to register
at all — declaring one is running a program — and each sidecar's `requires` capability must sit
inside the page grant and gates the call at the seam, which `describe` advertises so a caller can
see the gate before it calls. Both gates are unit-tested: an application without `process.spawn`
gets no sidecar command, and `describe` does not advertise a name that would be refused.

The sidecar runs inside the `process` runner's containment, not a new path: a root-resolved
program (a relative path is resolved inside the application, an absolute one refused), a
secret-scrubbed environment, no shell, a timeout, bounded output, and process-tree stop. Its
working directory is pinned to the app root, which is a real fix the end-to-end tests forced: an
interpreter resolves its script argument against the child's cwd, not the host's, so a script
placed next to the manifest could not be found until the runner set the child's directory. A
non-zero exit or a non-JSON answer reaches the page as an error rather than being silently
swallowed.

Tested against a real interpreter end to end — a real Python script written to a scratch
application root, invoked through the full `register` → handler → `ProcessRunner` → process path
with real stdin/stdout JSON (`crates/ncui-runtime/src/native.rs`, five sidecar tests), and the
manifest declaration driven from real `ncui.app.json` JSON through the loader
(`crates/ncui-app/src/manifest.rs`, four tests). Gate: `cargo fmt`, `cargo clippy --workspace
--all-targets` (pedantic, `unsafe_code = "forbid"`), the full `cargo test --workspace` suite, and
the layering gate all pass.

**The JS/TS half of C3 is not built.** Letting the *page* register a command handler that the
runtime calls back needs either a JS engine in the host — an ADR-012 dependency decision, not made
unilaterally — or a webview-callback round trip. The **component ABI** (already built) stays the
first-class path for precompiled native modules in an AOT language that emits WASM. The open half
is recorded in `docs/goal-prompt.md` §4 C3 and `docs/known-gaps.md` rather than claimed done.
