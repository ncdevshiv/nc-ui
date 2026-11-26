# ADR-012: Dependency policy

**Status:** Accepted · **Date:** 2026-10-01 · **Amended:** 2026-10-16, 2026-10-23, 2026-11-24

## Context
NC-UI links a WebView and drives a GPU driver. A compromised transitive dependency in that
position is a compromised application, running with the user's privileges. The ecosystem also
makes it trivially easy to accumulate hundreds of transitive dependencies by typing `cargo add`.

## Decision
**Minimise dependencies as a design goal, not an accident.** `deny.toml` makes the policy
machine-checked:

- wildcards and unregistered sources are **denied**; crates.io is the only registry, so the
  runtime is buildable without reaching a private index;
- known vulnerabilities and yanked versions are **denied**;
- the licence allow-list is explicit, so a copyleft or unknown licence surfaces during CI rather
  than at release;
- `unsafe_code` is **forbidden** in the runtime workspace, except the one package-scoped Windows
  menu attachment recorded in the ninth exception below.

The count and shape of dependencies is a reviewable property. When a subsystem can be built on
`std`, it is built on `std`.

## The first exception: `getrandom`

`ncui-security` depends on `getrandom`, and nothing else in the workspace does.

The reason is specific. `ncui serve` mints a bearer token, and `std` exposes no API for
`BCryptGenRandom` on Windows. Calling it directly requires `unsafe`, which
`unsafe_code = "forbid"` rejects. The alternatives were all worse than adding a dependency:

* return an error and leave `ncui serve` unable to start on Windows — which is what the
  code did before this decision, and is defensible but leaves a headline feature unusable on
  one of the three target platforms;
* fall back to `RandomState`, `SystemTime` or `SplitMix64` — all predictable, and a
  predictable token that *looks* random is worse than no token because it destroys the
  reader's trust in every token the system mints;
* hand-roll the FFI behind a `#[allow(unsafe_code)]` — rejected, since the crate-wide ban
  exists precisely so that this kind of local exception does not accumulate.

`getrandom` performs the FFI internally, so this project's own code remains free of `unsafe`
and the ban holds where it matters. Unix keeps using `/dev/urandom` directly and needs no
dependency at all; `getrandom` is only the Windows path.

This is recorded as a deliberate exception rather than a slip. It is the difference between
"this platform cannot start" and "this platform works", and it is confined to one module for
one purpose.

## The second exception: `wgpu`

`ncui-gpu-renderer` depends on `wgpu`, and nothing else in the workspace does.

The reason is specific. `gpu.islands` — a GPU-rendered region of a page composed with the DOM —
requires talking to a GPU driver. `std` exposes no graphics API. Calling Vulkan/DirectX/Metal
directly requires `unsafe`, which `unsafe_code = "forbid"` rejects. The alternatives were all
worse than adding a dependency:

* leave `gpu.islands` as `not_implemented` indefinitely — defensible, but leaves the one
  capability that distinguishes NC-UI's rendering story unrealised;
* hand-roll FFI to a graphics API behind a `#[allow(unsafe_code)]` — rejected for the same
  reason `getrandom` rejected it: the crate-wide ban exists precisely so that this kind of
  local exception does not accumulate;
* build a software rasteriser in pure Rust — rejected because ADR-002 requires GPU device loss
  recovery as a tested path, which a pure-software implementation cannot demonstrate.

`wgpu` performs the graphics FFI internally, so this project's own code remains free of `unsafe`
and the ban holds where it matters. It is feature-gated off by default (`wgpu-device` feature
on `ncui-gpu-renderer`), so the workspace compiles on any machine without the dependency being
pulled in. It is confined to `crates/ncui-gpu-renderer` for one purpose: owning the GPU device,
queue and surface, and exposing a safe abstraction to the rest of NC-UI.

This is recorded as a deliberate exception rather than a slip. It is the difference between
"GPU rendering cannot start" and "GPU rendering works", and it is confined to one crate for
one purpose.

### The `windows` version pin this exception requires

`wgpu` 29 pulls in `gpu-allocator`, whose D3D12 support and `wgpu-hal`'s own D3D12 support
must agree on one version of the `windows` crate: the allocator's `ID3D12Device` and the hal's
`ID3D12Device` are the same types or they are not. `gpu-allocator` 0.28 requires
`windows >= 0.53, <= 0.62` — which is `<= 0.62.0`, so it cannot use `windows` 0.62.2 — and
`wgpu-hal` 29 requires `windows ^0.62`, which excludes 0.61.

`tao` 0.33 (through `wry`) requires `windows ^0.61`, so the graph contains a `windows`
0.61.x as well. Cargo will reuse that already-activated 0.61.x for `gpu-allocator` rather
than give it its own 0.62.0, and the two halves of the D3D12 suballocator then disagree
about the type of a device.

The resolution is in `Cargo.lock`, and it is a pin rather than a hope:

```sh
cargo update -p windows@0.62.2 --precise 0.62.0
cargo update -p gpu-allocator --precise 0.28.0
```

The second command re-resolves `gpu-allocator` onto the 0.62 series, where `wgpu-hal` can
reach it. **Both commands are needed**: the first pins the 0.62 series to a version
`gpu-allocator` can use, and the second moves `gpu-allocator` onto it. Running only the
first leaves the conflict in place, and running only the second re-resolves `windows` to
0.62.2 and breaks it again.

The lockfile is committed, so the resolution is reproducible. It is recorded here because it
is not obvious: a future `cargo update` will break the build, and the error it produces —
"expected `ID3D12Device`, found `Direct3D12::ID3D12Device`" — says nothing about versions.

## The third exception: `wry`, `tao` and the Linux GTK container

`ncui-webview-wry` depends on `wry` and `tao`, and nothing else in the workspace does. On Linux
it also depends directly on the matching `gtk` bindings for one purpose: owning the `GtkFixed`
container in which an embedded child webview is placed.

The reason is the same shape as `wgpu`'s. A desktop runtime that cannot open a window is not
a desktop runtime, and no `std` API opens one. `wry` is the WebView embedding layer and
`tao` is the windowing layer; between them they cover WebView2 on Windows, WebKitGTK on
Linux and WKWebView on macOS from one API.

They are feature-gated off by default (`wry-engine` on `ncui-webview-wry`), so the workspace
compiles on a machine with no display and no `webkit2gtk`. On Linux the `webkit2gtk`
development package is needed to build the feature, which is recorded here because it is the
kind of thing that fails at build time rather than at review time.

Wry's raw-handle child API is X11-only, so the Linux child path uses wry's safe `build_gtk`
entry point with a `GtkFixed` parent instead. That is the supported route for both X11 and
Wayland; it also lets the adapter move the child with `GtkFixed::move_`, which wry's
`set_bounds` alone does not do for a fixed parent. `gtk` was already in the locked tree through
wry, and the direct edge uses that same version rather than adding a second binding stack.

All three perform FFI internally; the adapter itself uses their safe APIs. The separate `gtk`
binding is MIT-licensed, matching the allow-list, and remains pinned to the unmaintained 0.18 line
already required by this wry version. Upgrading that line requires a coordinated wry/GTK migration
rather than silently introducing a second binding stack. First-party `unsafe` is limited to the
ninth exception below.

## The fourth exception: `rustybuzz`

`ncui-gpu-renderer` depends on `rustybuzz` for the text layout path, and nothing else in the
workspace does.

ADR-002 requires a text layout path and says explicitly that the project is "not attempting
to out-engineer HarfBuzz/CoreText/DirectWrite; we integrate them". `rustybuzz` is the
maintained pure-Rust port of HarfBuzz and produces the same shaping results as the C library
on every platform; `ttf-parser`, its font reader, comes with it. Neither performs FFI, so
unlike the two exceptions above this one does not even need the FFI argument — it is here
because text shaping is a solved problem and re-solving it would be a way to be wrong.

It is feature-gated off by default (`text-shaping` on `ncui-gpu-renderer`), so a machine with
no fonts still compiles the workspace.

## The fifth exception: `wasmi`

`ncui-component` depends on `wasmi`, and nothing else in the workspace does.

The reason is specific. The component ABI specifies a language-neutral boundary for untrusted
code, and a specification that executes nothing is a document rather than a capability. Running
a third-party artifact needs an interpreter, and `std` exposes none. Writing one in this project
would mean a second implementation of a specification with its own bugs to find, in the one
place where a bug is an escape from the sandbox; reaching a native one through FFI would need
`unsafe`, which the workspace forbids.

The alternatives were all worse than adding a dependency:

* leave the ABI specified and unexecuted — which is what the code did before this decision, and
  is exactly the kind of claim this project treats as a defect;
* load a native library component instead — needs `libloading`, hence `unsafe`, and hands the
  component the host's address space rather than one linear memory the host can bound;
* hand-roll an interpreter behind `#[allow(unsafe_code)]` — rejected for the same reason the
  earlier two exceptions rejected it: the crate-wide ban exists precisely so that this kind of
  local exception does not accumulate.

`wasmi` is a pure-Rust interpreter: it performs no FFI, so this project's own code remains free
of `unsafe` and the ban holds where it matters. It is confined to `crates/ncui-component` for
one purpose — executing a component under a bounded store — and it is not feature-gated off,
because it has no platform requirement: unlike `wgpu`, a machine that can build this workspace
can build `wasmi`, so gating it would only create a configuration in which the tests that
matter never run.

The tree it adds is 18 crates (`wasmi`, `wasmi_collections`, `wasmi_core`, `wasmi_ir`,
`wasmparser`, `wat`, `wast`, `spin` and their small dependencies). Every one of them is on the
licence allow-list in `deny.toml`, and the check is `all-features`, so the count is measured
rather than asserted. This is recorded as a deliberate exception rather than a slip: it is the
difference between "the component ABI is a specification" and "the component ABI runs", and it
is confined to one crate for one purpose.

## The sixth exception: `global-hotkey`

`ncui-native` depends on `global-hotkey`, and nothing else in the workspace does.

The reason is specific. A global shortcut is a key combination the platform delivers to the
application whatever window has focus, and it is the one piece of an application's surface a user
can reach without finding the window first. `std` exposes no way to ask for one. The platform's own
call is `RegisterHotKey` on Windows, `XGrabKey` on X11 and `RegisterEventHotKey` on macOS — all
foreign functions, and `unsafe_code = "forbid"` rejects calling them. The alternatives were all
worse than adding a dependency:

* leave global shortcuts absent — which is what the code did before this decision, and is a
  capability every desktop framework has: Electron's `globalShortcut`, Tauri's
  `global-shortcut` plugin. A framework that cannot register one is a framework whose
  applications cannot be reached when they are not in front;
* hand-roll the FFI behind `#[allow(unsafe_code)]` — rejected for the same reason every earlier
  exception rejected it: the crate-wide ban exists precisely so that this kind of local exception
  does not accumulate, and the three platform calls it would cover are three places for a
  lifetime bug to become a use-after-free in the process that owns the user's keys.

`global-hotkey` performs the FFI internally, so this project's own code remains free of `unsafe`
and the ban holds where it matters. It is confined to `crates/ncui-native` for one purpose —
registering, unregistering and reporting the shortcuts an application declares — and it is behind
the `global-hotkeys` feature, off by default: on Linux the crate needs a connection to an X server,
which a headless machine does not have, so gating it keeps the default build and the Linux gate
exactly as they are. That is the same treatment `wry` gets, for the same reason.

The tree it adds is small and auditable: `crossbeam-channel`, `keyboard-types`, `once_cell` and
`thiserror`, plus one platform pair — `windows-sys` on Windows, `x11rb` and `xkeysym` on Linux,
`objc2` and `objc2-app-kit` on macOS — of which only the running platform's is compiled. The
Windows subtree was measured rather than asserted: 27 crates including this workspace's own, every
one on the licence allow-list in `deny.toml`. This is recorded as a deliberate exception rather than
a slip: it is the difference between "an application is reachable only through its window" and "an
application can be given a key".

## The seventh exception: `tray-icon`

`ncui-webview-wry` depends on `tray-icon`, and nothing else in the workspace does.

The reason is specific. A tray icon is the one part of an application that is visible when none of
its windows are: it is how a user sees that the application is running and how they get back to it.
`std` exposes no way to ask a platform for one. The platform's own call is `Shell_NotifyIcon` on
Windows, an `AppIndicator`/`StatusNotifierItem` on Linux and `NSStatusBar` on macOS — all foreign
functions, and `unsafe_code = "forbid"` rejects calling them. The alternatives were all worse than
adding a dependency:

* leave the tray absent — which is what the code did before this decision, and is a capability
  Electron's `Tray` and Tauri's `tray-icon` plugin both have;
* hand-roll the FFI behind `#[allow(unsafe_code)]` — rejected for the same reason every earlier
  exception rejected it, and this one is worse than most: a tray icon outlives the window it belongs
  to, so a lifetime bug in it is a use-after-free in a process the user cannot see misbehaving.

`tray-icon` performs the FFI internally, so this project's own code remains free of `unsafe` and the
ban holds where it matters. It is confined to `crates/ncui-webview-wry` for one purpose — creating
the tray icon an application declares and reporting that it was clicked — and it is behind the
`tray-icon` feature, which implies the `wry-engine` feature it needs: a tray icon without an event
loop is not a thing, so the two are gated together and the default build is unchanged.

It is maintained by the same author as `wry` and `tao`, which matters here: the tray is created on
the engine's own thread and must live beside the event loop those two own, and a crate from a
different lineage would be a second opinion about that loop's lifetime. Its licence is
`MIT OR Apache-2.0`, both of which are on the allow-list in `deny.toml`, and the tree it adds is
measured rather than asserted when the feature is enabled.

## The eighth exception: `rfd`

`ncui-native` depends on `rfd`, and nothing else in the workspace does.

The reason is specific. A native file, save or folder dialog is the platform's own picker: the
thing that puts `IFileOpenDialog` in front of a person on Windows, an `NSOpenPanel` on macOS and a
GTK file chooser on Linux, walks the filesystem under its own rules, and returns the path the
person chose. `std` exposes no way to ask a platform for one. Those pickers are all foreign
functions, and `unsafe_code = "forbid"` rejects calling them. The alternatives were all worse than
adding a dependency:

* leave the dialog absent — which is what the code did before this decision, and is a capability
  Electron's `dialog` module and Tauri's `dialog` plugin both have;
* hand-roll the FFI behind `#[allow(unsafe_code)]` — rejected for the same reason every earlier
  exception rejected it. A COM dialog on Windows in particular is a lifetime minefield: the
  `IFileOpenDialog` hands back COM pointers the caller must release, and getting that wrong in a
  process that shows one to a person is exactly the class of bug the crate-wide ban exists to
  prevent.

`rfd` performs the FFI internally, so this project's own code remains free of `unsafe` and the ban
holds where it matters. It is confined to `crates/ncui-native` for one purpose — showing a picker
and returning what the person chose — and it is behind the `dialogs` feature, off by default: a
modal dialog blocks on a person, which a headless machine does not have, so gating it keeps the
default build and the Linux gate exactly as they are. The permission rules, the request vocabulary
and the validation of the path that comes back are plain Rust in `crates/ncui-native/src/dialog.rs`
and are tested without the platform binding; the feature supplies only the thin adapter that shows
the real picker.

Its licence is `MIT`, which is on the allow-list in `deny.toml`. It is a synchronous, blocking API
by design — a dialog is a question asked of a person, and the answer comes back through the return
value — so it is called from the command handler that already runs off the page's hot path, the
same place the notification and clipboard calls run. The one step a test cannot drive is the single
real modal open, because a test has no person; that is recorded in `docs/known-gaps.md` as a
one-time human verification rather than claimed as automated.

## The ninth exception: `muda`

`ncui-webview-wry` depends on `muda`, and nothing else in the workspace does.

The reason is specific. A native menu bar is application chrome the platform draws and dispatches;
`std` exposes no API for one. The platform calls are `SetMenu`/`DrawMenuBar` on Windows,
`gtk_container_add` on Linux and AppKit's menu objects on macOS. Hand-rolling those bindings would
still require `unsafe`, and would add a second implementation of platform menu lifetime and event
routing to the one place where a mistake can outlive the window that owns it.

The alternatives were all worse than adding a dependency:

* leave native menus absent, despite Electron's `Menu` and Tauri's `menu` API both providing them;
* use a page-rendered menu, which would not be platform chrome and would not prove the native path;
* hand-roll the FFI behind a local allowance, which would merely move the same unsafe work into
  this repository without muda's platform coverage.

`muda` performs the platform FFI internally. NC-UI's adapter is safe on Linux through
`init_for_gtk_window`; Windows alone requires `Menu::init_for_hwnd`, which is `pub unsafe fn`.
That single unsafe block and call site is the workspace's one function-scoped exception. Cargo
passes a package-level `forbid` on the compiler command line, which no function attribute can
override. The package therefore declares `unsafe_code = "allow"`, its crate root immediately
restores `#![deny(unsafe_code)]`, and only the Windows `attach` function carries a reasoned
`#[allow(unsafe_code)]`; the block is also compiled only behind `native-menu`. Every other unsafe
block in this package remains a compile error. The HWND comes from the
live tao window on the engine thread, and `EngineWindow` declares its menu before its window so the
menu drops first and muda detaches it while the HWND is still valid. A replacement is fully built
and duplicate-validated before any attachment changes, so a validation refusal leaves the current
menu attached. On Windows the new menu is attached first, the old menu is removed from muda's HWND
tracking, and the new menu is shown and verified; removing that tracking before the old object drops
prevents its drop hook from detaching the replacement. If attachment, removal or verification fails,
the previous menu is restored. Linux attaches the new GTK menu bar before dropping the old one.

The dependency is optional and feature-gated: `native-menu` implies `wry-engine`, and the release
`wry-host` feature enables it. A default build without a window host does not pull it in. Its licence
is `Apache-2.0 OR MIT`, both on the allow-list in `deny.toml`; the dependency, licence and source
checks run with all features so the tree is measured rather than asserted. macOS uses the safe
`init_for_nsapp` path in the adapter but remains unverified until a macOS host runs it.

## The tenth exception: `sha2` for consent integrity

`ncui-security` depends directly on `sha2` to bind a consent token to the exact normalized
parameters of the operation a person approved. The crate was already in the locked graph for
bundle-file digests, so this adds no package or platform binding; it makes the security use a
reviewable direct dependency rather than an accidental transitive one.

A token bound only to a method and capability can be replayed with altered arguments. Hashing the
canonical JSON operation parameters, with the consent transport fields removed, lets the runtime
refuse that substitution while the minted-nonce ledger remains the authority. `sha2` is pure Rust,
performs no FFI, and is confined to this integrity check and the existing bundle digest path. The
alternative — comparing caller-controlled JSON without a canonical digest, or relying on token text
as proof — would either miss semantically identical requests or accept altered operations.

## Consequences
- Some conveniences are reimplemented rather than imported, which is a real cost paid for a
  smaller and more auditable attack surface. This is a deliberate trade, not an oversight.
- Supply-chain checks (audit, licence, SBOM) are CI gates, not release-week chores.
- The decision is revisited if a dependency's maintenance status or licence changes.

## Implementation status
**Implemented.** `deny.toml` is in place, the CI supply-chain job runs `cargo deny check` and
`rustsec/audit-check`, and `unsafe_code = "forbid"` is set workspace-wide except for the single
function-scoped Windows menu attachment block documented above, enforced by a crate-level deny.
