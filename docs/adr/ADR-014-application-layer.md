# ADR-014: The application layer — manifest, assets, and the page loop

**Status:** Accepted · **Date:** 2026-10-22

## Two files an application keeps about itself

An application remembers two things: where its window was, and what its page asked to keep. Both
live in the user's own state directory, under the application's name, as one JSON document each —
`window.json` written by the runtime, `state.json` written by the page through `state.save`,
`state.load` and `state.remove`.

The page's store is bounded in three directions, because a preference store that grows without
limit is a disk-space bug the user cannot see: at most 256 keys, at most 64 KiB per value, at most
1 MiB for the document. A key is bounded and may not carry a control character, and an empty key is
refused rather than treated as "the whole store" — a page that could clear everything it had
stored with one call is a page that will eventually do it by accident.

None of the three commands requires a capability. What a page remembers about itself is its own
business, and the file it writes belongs to the application it is; asking for an authority in
order to remember which column was widest would make every application request something the user
cannot evaluate. What is *not* promised is recorded in `docs/known-gaps.md`: there are no
migrations, so a page that changes the shape of what it stores has to read its own older shapes.

## A window is put back where the user left it

The manifest says what a *first* run gets. After that, the user's own arrangement wins: the
application writes the window's geometry on shutdown and creates the window from it on the next
launch.

Three details are what make it work rather than nearly work. The geometry is read from the *host*,
which owns the window and is the only place that knows its scale factor — so the port speaks
logical pixels and a remembered geometry survives a display change. The state file is treated as
input, because it is a file an application wrote: a size outside the port's bounds is refused
before a window is asked for, and a file that cannot be read is ignored rather than fatal. And the
path has to be known *before* the window is created, which is why `launch_with` takes it: a path
set after the window exists would be a path the window was never built from.

The fourth detail is the one a user notices when it is missing: a place remembered while a second
display was attached is a place on a display that no longer exists. The host is asked which
displays it is on, and a remembered place that is on none of them is moved onto the nearest one,
keeping the size the user chose. A window the user can see and drag is a window they can fix; one
that opens off-screen is a window they cannot find at all.

A page opens its own windows, and it says what they are: `create_window` takes an optional `x`
and `y` for the place, `always_on_top` to float, `decorations: false` for a window without a
frame, `resizable: false` for one the user cannot change, `min_width`/`min_height` for a floor
the user cannot drag past, `fullscreen` and `maximized` for a window that opens filling its
display, and `remember: "notes"` for a window whose place should outlive the run. A second window
therefore appears where and how the page wants it rather than jumping there after the platform has
put it somewhere.

The manifest asks for the same window options, because the application's own window is created the
same way: one request type, applied by one registry, so an application and a page cannot get
different windows out of the same words.

A page also changes how *its own* window is presented while it runs. `window_state` takes one of
`fullscreen`, `maximized`, `minimized`, `visible` and `focus` — one, because two would be a request for
whichever the platform applied last — and answers with the state the platform reports *afterwards*.
That answer is the point: the user maximises, minimises and focuses windows by hand, so a runtime
that reported what it asked for would disagree with the screen. The set is closed and its rule lives
with the type (`WindowChange::from_payload`), not in whichever layer happens to read a payload, so a
second reader cannot get a different answer.

`fullscreen` is borderless rather than exclusive. An exclusive fullscreen window takes the display
away from the user's other windows, which is a decision an application has no business making on the
user's behalf.

`window_shape` is the rest of what a window can become: its title, its place, its size, whether it
floats and whether the user may resize it. Every field is optional, and a field the caller did not
name is left alone — a page that renames itself must not drag a window the user has moved back to
where it was created. The window's own record is written only *after* the platform took the change,
so `ncui.window.list` describes the window that exists; a record written first would report a title
the window does not carry, which is the defect the summary exists to prevent.

The options are validated where the request is validated, not only where a payload is read: a
floor below the port's minimum, above the window's own size, or naming one dimension and not the
other is refused with the field that produced it. Half a floor is the case worth naming — a window
floored in one dimension and not the other can be squashed flat in the one nobody constrained,
which is a window the user can lose.

A name is what makes a window remembered, and the name belongs to the page: the page is what knows
that two windows it opens are two windows. The runtime keeps the places in `windows.json`, beside
the application's own `window.json`, and applies a remembered place while the window is being
created rather than moving it there afterwards. The place is written down when a window is closed
through the runtime and when the application stops — both are moments at which the platform can
still be asked where the window was, and a geometry that is no longer reportable is left alone
rather than guessed at. The geometry is treated as input in both directions, so a place no window
could have is never written and a file that cannot be parsed is no memory rather than a failure to
open a window. What the runtime still does not do is remember a window the page never names;
`docs/known-gaps.md` records that.

## Installing an application

A bundle is a directory somebody can run; an installed application is one the machine knows about.
`ncui install <bundle>` does three things in an order that matters: it copies the bundle into a
directory named after the application under this user's own applications directory, it registers
the identity and the URL scheme the bundle names, and it writes `installed.json` describing what
it did.

The order is the design. The registrations name the *installed* program's path, so they come after
the files — a registration written first would point at a directory that does not exist yet. And
the record is what makes `uninstall` exact: removing what the *bundle* says would be removing
something different from what was installed, if the bundle changed in between.

Three refusals are deliberate. The name is validated as a directory name before anything is copied,
so a name that is a path cannot escape the root. An install over an existing one is refused rather
than merged, because a half-replaced application is worse than one that is clearly still the old
version. And nothing is written outside the root the caller named.

What is not here, and is recorded in `docs/known-gaps.md`: no installer executable and no MSI, no
per-machine install, and no upgrade path — an upgrade would need to know what is running, which is
the running instance's business rather than the installer's.

## A link reaches the instance that is running

A deep link is two things: a registration, which tells the platform that a scheme belongs to this
application, and a transport, which gets the URL to the program. The transport is the single
instance hand-off described above — a link launches `ncui run <directory> "<link>"`, and that
launch hands its argument vector to the instance already serving.

The registration is a manifest field and an install step. The manifest names the scheme, which is
validated by shape because the value reaches a registry key or a desktop entry; the bundle ships
`register-protocol.ps1` (Windows) or `install-protocol.sh` (Linux), which the *user* runs. The
build never writes the user's registry: a build tool whose output depended on the machine that ran
it would make two bundles of one tree differ.

Two refusals are worth recording. A scheme the machine already uses — `http`, `https`, `mailto`
and the rest — is refused outright, because taking one would not give this application a link, it
would take a link away from everything else that uses it. And an application that declares no
scheme gets no registration, so a bundle cannot claim links its application never asked for.

macOS keeps a scheme in the bundle's `Info.plist`, which the bundler does not write yet;
`register` refuses there and says which file to change rather than pretending. `docs/known-gaps.md`
records it.

## One instance per application

An application is one thing to its user. Opening the same application twice should reach the
window that is already there rather than start a twin, and a link should arrive at the instance
that is running — otherwise a deep link opens a second process and the user has two windows onto
one document with no way to tell which is which.

`ncui run` therefore claims an instance lock before it opens anything. The lock is a file created
exclusively beside the user's other state, holding the port the instance listens on and the token
it will accept; the second launch connects over loopback, sends its argument vector, and exits.
The running instance receives it in its pump loop, counts it, logs it and reloads the page it
already has.

Three decisions are worth recording. **The lock is a file, not a platform primitive**: a named
mutex or a single-instance API would need `unsafe`, and `create_new` is atomic on every platform
this runtime supports. **A stale lock is taken over rather than trusted**, because a killed
process leaves its file behind and a launch that refused to start because of a leftover would be
a launch that never worked again. **The token is what makes it a hand-off rather than an
injection**: any local process can connect to a loopback port, so a payload without the token the
lock published is refused, and the refusal is logged.

What it does not do is raise the first instance's window. Bringing a window forward is a platform
operation with its own rules about what may steal focus, and the honest division is that the
second launch's arguments are delivered and the application decides what they mean.

## Context

NC-UI had a runtime, a window host, a page bridge and an agent API. What it did not have
was the layer above them: the thing an author writes.

An application in every comparable framework is a directory. It has a manifest naming the
entry point and the window, assets that are served over HTTP, and a process that starts
the server, opens the window, and keeps running until the window closes. NC-UI had the
middle of that — a window could be opened with a URL, and the URL had to be something the
author had arranged by hand.

Three specific gaps followed from that:

* **The page had no origin.** A page loaded from `file://` cannot load a module, cannot
  fetch a sibling, and has no origin to be allowed by a navigation policy. Serving the
  application's own files over loopback HTTP is what gives a page an origin and makes it
  an ordinary web page.
* **The window had no shape.** Every host invented its own title and size, and an
  application could not ask for a window that looked like anything in particular.
* **Nothing ran the page loop.** The runtime could answer a page's calls, but only if
  somebody called `pump_pages`. A framework that opens a window without running that loop
  has opened a window whose native calls go nowhere.

## Decision

**An application is a directory with a manifest in it.** `ncui.app.json` names the
application, its version, its entry page, its assets directory, its window and the
capabilities its pages are granted. It is JSON rather than TOML because ADR-012 keeps the
dependency list small and JSON is already one of them; it is read once at startup, where a
parse cost is irrelevant.

**Every field of the manifest is validated before the runtime acts on it.** An entry that
is absolute, drive-qualified or traverses with `..` is refused, and the entry is resolved
through the same `SafePath` containment check every other untrusted path in the project
goes through. A manifest that is wrong is refused with a message naming the field.

**The application's assets are served over loopback HTTP by a real server.** The server
speaks HTTP/1.1, answers `GET` and `HEAD`, sets the content type from the extension,
refuses every other method, closes the connection after each response, and bounds the
request head it will read. It binds `127.0.0.1` and refuses any other address: the only
thing it serves is one application to one window on this machine.

**A request path is resolved twice.** `SafePath` resolves it lexically and refuses
traversal, absolute paths and drive-qualified paths; the resolved path is then
canonicalized and checked again, which closes the gap `SafePath` documents — a symlink
planted inside the root can point outside it, and only the second check sees that.

**The window's shape travels through the port.** `WindowRequest` carries the URL, the
title and the size from whoever asked for the window to the engine that creates it.
`HostFactory::spawn` takes the request, so a host cannot create a window the application
did not ask for. The same request is what `ncui.window.create` accepts, so an agent and an
application ask for a window the same way.

**The page loop lives in the runtime.** `Application::pump` serves whatever the pages have
sent and `Application::is_running` reports whether the window is still there. The CLI runs
them in a loop; a test runs them until a condition holds. A framework that leaves the loop
to its caller has left the one thing that cannot be left out.

**The development loop is part of running an application.** `--watch` fingerprints the
assets directory — newest modification time, entry count and total size — and reloads the
page when it changes. Polling is what `std` offers without a platform binding, and at the
size of an application's assets the scan is invisible next to a reload.

**The application is registered in the feature registry like everything else.**
`app.manifest`, `app.asset_server` and `app.run` each carry the same evidence slots every
other completed feature carries, and their platform status says what has actually been
verified.

## Consequences

- An application is now a directory an author writes and `ncui run` starts: manifest read,
  assets served, window opened, page calls answered until the window closes.
- The page's origin is a real origin on the loopback interface, and the navigation policy
  permits `http:` for a loopback host only — the origin still has to be on the allow-list,
  and a remote host is still refused.
- The application's assets cannot be reached from outside the served directory, and the
  directory is not listed: an application serves what it names, not what it happens to hold.
- The window's title and size are the application's to choose, and are reported by
  `ncui.window.list` like everything else about a window.
- The asset server is a *file* server for one directory tree. It is not a bundler, a proxy
  or a virtual file system, and it does not compile anything: `--watch` reloads what is on
  disk. `docs/known-gaps.md` records what an application still has to bring for itself.
- `ncui run` needs a window host. The release build turns the `wry-host` feature on, so
  the shipped binary can open a window; a build without it says so rather than serving
  assets nobody will look at.
