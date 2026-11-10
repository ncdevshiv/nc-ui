# ADR-015: Native capabilities, and why the filesystem is the first one

**Status:** Accepted · **Date:** 2026-10-22

## Opening a target is the one place the platform's tool is a shell

`shell.open` hands a file or a link to whatever the platform uses for it. On macOS and Linux the
target is one argument to `open` or `xdg-open`, so nothing about it can become a second command. On
Windows the tool *is* `cmd`, because `start` is a `cmd` builtin — which means the target passes
through a command line, and a target carrying a quote, an apostrophe or a control character is
therefore refused rather than escaped. The refusal is on the target, not on the tool: the tool is
the platform's, and there is no other way to ask Windows to open something by association.

Two further refusals are worth recording. The schemes that belong to a browser rather than to the
operating system — `javascript`, `data`, `vbscript`, `blob`, `about`, `view-source`, `chrome` — are
refused outright, because handing one to a platform opener is how an application turns a link into
script execution in whatever handles it. And the command asks for the same capability as spawning a
program: opening a target starts whatever program the platform associates with it, which is a
program the page did not name.

The caveat that belongs here rather than only in the code: Windows resolves a scheme through an
association cache the shell refreshes on its own schedule, so a scheme registered during a session
is not reliably live in that session. An install that registers a scheme and then opens a link of
it may therefore need the user to log in again, or the shell to be told; `docs/known-gaps.md`
records it.

## Notifications are a message, and the payload is treated as one

A notification is raised through the tool each platform ships — PowerShell, `osascript`,
`notify-send` — for the same reason the clipboard is: every Rust crate that offers one is a
binding over WinRT, `UserNotifications` or D-Bus, which this workspace cannot reach without
`unsafe` outside a dependency.

What is worth recording is that the three tools read their payload differently, so each is
treated as what it is. On Windows the text travels on standard input and the toast is built by
setting *text nodes* on the platform's template, so markup in a message stays a message. On
macOS `osascript` is given source, so the payload is escaped into an AppleScript string literal
by a function with its own tests. On Linux the payload is an argument vector behind `--`, so a
body beginning with a dash is a message rather than a flag. The only value interpolated into a
script is the application identity, and it is validated to be an identity first.

Two things are deliberately not promised. The first is that the user sees it: every platform
honours a do-not-disturb setting, and a host cannot know what the user chose. The second is that
it arrives under the application's own name: Windows delivers a toast against an AppUserModelID,
and an application that wants its own registers one at install time, which is packaging work. The
default is PowerShell's identity, because a notification delivered under the wrong name is worth
more than one that is silently dropped. `docs/known-gaps.md` records both.

## A timeout stops the tree, not the process

The first version of `process.run` stopped the program it started and nothing else, which is the
kind of bound that reads as complete and is not: a program that starts a program leaves the second
one running, and the caller has no way to know. The runner now terminates the whole tree — on
Windows by asking `taskkill` to walk the parent chain, on Unix by starting the child in its own
process group and signalling the group. `a_timeout_stops_the_program_and_everything_it_started`
proves it with a grandchild that writes to a file once a second, and the same test fails if the
tree kill is removed.

The residual is stated rather than implied: a program that deliberately breaks away from its
parent — `start` in a new process group, `setsid`, a re-parenting daemon — is outside the reach of
both tools, and closing that needs a job object or a cgroup. `docs/known-gaps.md` records it.

## Context

A WebView gives a page the web platform. A desktop framework is judged by what a page can
do *beyond* it: read a file, save a document, show a dialog, put something on the
clipboard, notify the user, live in the tray. Every comparable framework offers these, and
NC-UI offered none of them.

The capability model was already there — `Capability::FilesystemRead`,
`Capability::ProcessSpawn`, `Capability::Clipboard` and the rest are declared in
`ncui-core`, granted by roles and manifests, and enforced by the runtime's pipeline. What
was missing was the *provider* half: nothing implemented any of those capabilities, so a
grant named an authority that no command used.

That is its own kind of dishonesty. A capability catalogue that describes authorities
nobody can exercise reads as a feature list, and the first person to write
`invoke("fs.read_text")` discovers it was fiction.

## Decision

**A native capability is implemented as a command, and the command exists only when the
page's grant covers it.** `ncui_runtime::native::register` reads the runtime's page grant
and registers the commands that grant covers. A page without `filesystem.read` is never
offered `fs.read_text`; the action table does not contain it, so the seam refuses the name
before the dispatcher is reached — and the call is authorized again when it is made, so a
grant narrowed after registration is still enforced.

**The first two capabilities are the ones that can be complete without a platform
binding.** The filesystem needs no binding and no `unsafe`: `std::fs` is the platform's own
interface, and the containment rules the project already uses apply unchanged. Running a
program is the same story with a different set of rules: no shell, an argument vector, a
scrubbed environment, a contained program path, a time limit and bounded output. The
clipboard is the third, and the exception that proves the rule: it has no `std` interface,
so it is read and written through the tool each platform ships rather than through a
binding, and the cost of that choice is recorded in `docs/known-gaps.md`. Dialogs,
notifications, the tray, menus and global shortcuts still need a binding this workspace does
not have — adding one is a dependency decision under ADR-012, with its own review. They are
recorded as absent rather than offered as commands that do nothing.

**Containment is applied twice, because a lexical check is not enough.** A request is
resolved by `SafePath`, which refuses traversal, absolute paths, drive-qualified paths and
NUL bytes. The result is then canonicalized and checked again, which is what refuses a
symlink inside the root that points outside it. A write is checked on its **parent** — the
target does not exist yet — and on the target itself when it does, so a write cannot follow
a link out of the directory either.

**Every operation is bounded.** A read is limited to 8 MiB and the limit is enforced on the
bytes actually read, not on the size the metadata claims; a write is refused before it
reaches the disk; a listing is refused rather than truncated when a directory holds more
entries than the bound. An unbounded capability is a memory-exhaustion path with a friendly
name.

**The commands are observable.** Each call increments `native.fs_calls`, labelled by the
operation, and each call — allowed, denied or failed — records an audit event through the
same pipeline an agent's request takes. The label is the operation's own name, from the
runtime's code, never text the page supplied.

**The surface is a capability, not a service.** There is no global "native module" a page
can reach; there are commands, each declaring what it requires, registered from the grant.
A new capability is a new module that exposes commands, not a new way around the gate.

**Spawning is the most dangerous of the two, and is written accordingly.** A program runs
with the user's authority, so the runner removes every way a *page* could shape what runs
beyond the program it names: there is no shell to re-parse an argument, an absolute program
path is refused, the environment is scrubbed of secret-shaped names, the output is capped
and the child is killed at its limit. It is not a sandbox, and the documentation says so —
the capability and the user's grant are the boundary.

## Consequences

- An application that grants `filesystem.read` gets three commands; one that grants
  `filesystem.write` gets a fourth, which is destructive and therefore asks the page to
  state its consent.
- The filesystem root is the application's own directory. An application that needs a
  wider root needs a different arrangement, and that is deliberate: the default should not
  be the user's home directory.
- A page that asks for a capability nobody implements is refused by the seam, and the
  refusal names the action. That is the honest answer, and it is what `describe` reports.
- The remaining native surface is listed in `docs/known-gaps.md` with what each one needs.
  Adding one does not require changing the model — it requires a binding and an ADR
  exception.
