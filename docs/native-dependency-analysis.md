# Native dependency analysis: gpui 0.2.2

**Date:** 2026-11-24 · **Companion to:** ADR-020 (native view host), ADR-012 (dependency policy)

This is the measured record behind ADR-020's dependency consequence: what gpui 0.2.2
actually adds to the workspace, what licences it carries, where the supply-chain risk
sits, and what can be trimmed. Everything below is measured from the two lockfiles, from
`cargo tree` against the real resolver, and from one full compile — not asserted.

## 1. Methodology

Three measurement surfaces, because each answers a different question:

- **The lockfile closure** — platform-independent. `git show HEAD:Cargo.lock` is the
  pre-gpui baseline (the in-flight lock update is uncommitted); the working-tree
  `Cargo.lock` is post-gpui. The transitive closure of `gpui 0.2.2` inside each lock was
  computed by walking the lock's dependency lists. A lock closure counts everything cargo
  *resolves*, including Linux- and macOS-only crates that a Windows build never compiles
  (wayland, cocoa, metal) and optional dependencies that no enabled feature activates
  (zed-scap). It is the largest honest number, and the one cargo-deny's graph check sees.
- **The Windows compile tree** — `cargo tree -e normal --target x86_64-pc-windows-msvc`
  on a scratch crate depending on `gpui 0.2.2`, offline against the populated registry
  cache. This is what a Windows machine actually compiles. nc-ui's only target platform
  for the first native host, so this is the number that matters for compile time and for
  licence review of shipped artifacts.
- **A real compile** — `cargo check` of the scratch crate in the trimmed configuration,
  to prove the trimming recommendation builds rather than merely resolves.

The nc-ui-kit sibling lock (`G:\nc-ui-kit\Cargo.lock`) was used as a cross-check: it is a
pure gpui workspace, and its closure confirms the shape — 717 of its 719 packages are
reachable from gpui (everything except the two first-party workspace crates), 630 unique
names. The kit adds essentially nothing of its own; gpui *is* its dependency tree.

## 2. How much gpui adds

| Measure | Count |
|---|---|
| Pre-gpui workspace lock | 466 entries, 423 unique crate names |
| Post-gpui workspace lock | 935 entries, 808 unique crate names |
| **New entries in the lock** | **475** |
| **New unique crate names in the workspace** | **385** |
| gpui closure inside the nc-ui lock | 718 entries, 632 unique names |
| — of which already in the pre-gpui tree (serde, windows, etc.) | 248 names |
| Windows compile tree, default features | **390 unique crate names** |
| Windows compile tree, trimmed (§5) | **369 unique crate names** |

For scale: the largest previous ADR-012 exception (`global-hotkey`) measured 27 crates
on its platform subtree; `wasmi` measured 18. gpui is an order of magnitude beyond both —
it is the largest exception the policy has absorbed, by a factor of fourteen, and the
review standard for its updates should be set accordingly: a gpui version bump is not a
`cargo update` line, it is a re-review of a 390-crate tree.

Why a UI framework carries 390 crates, in one paragraph of decomposition: gpui ships its
own async executor ecosystem (`smol`, `async-task`, `futures`), a full HTTP/TLS client
(`zed-reqwest` → `hyper`, `rustls`, `ring`, `tokio`), a full image codec suite (`image`
with default formats including AV1 — see §4), SVG rendering (`resvg`/`usvg`/`tiny-skia`),
text layout (`fontdb`, `rustybuzz` through usvg, `unicode-*`/`icu*` segmentation data),
GPU path rendering (`lyon` tessellation), layout (`taffy`), scene atlasing (`etagere`),
and — under default features, on every platform including Windows — a Vulkan FFI stack
it never uses on Windows (`blade-graphics`/`ash`/`naga` — see §5). This is a framework
that ships its batteries; the analysis below is about which of them nc-ui actually
needs.

## 3. Licence inventory

Measured over the 390 unique crates of the Windows compile tree (default features),
using cargo's own licence metadata. Grouped by exact declared expression:

| Crates | Licence expression |
|---|---|
| 159 | `MIT OR Apache-2.0` |
| 74 | `MIT` |
| 48 | `Apache-2.0 OR MIT` |
| 18 | `Unicode-3.0` |
| 17 | `MIT/Apache-2.0` (legacy slash syntax — cargo-deny reads as OR) |
| 16 | `Apache-2.0` |
| 8 | `BSD-3-Clause` |
| 6 | `Unlicense OR MIT` |
| 4 each | `BSD-2-Clause`, `Apache-2.0/MIT`, `Apache-2.0 OR ISC OR MIT`, `MIT OR Apache-2.0 OR Zlib` |
| 3 each | `Zlib`, `ISC`, `Zlib OR Apache-2.0 OR MIT` |
| 2 each | `BSD-3-Clause OR Apache-2.0`, `Unlicense/MIT`, `(MIT OR Apache-2.0) AND Unicode-3.0`, `BSD-2-Clause OR Apache-2.0 OR MIT` |
| 1 each | `0BSD OR MIT OR Apache-2.0`, `CC0-1.0`, `CC0-1.0 OR Apache-2.0`, `CC0-1.0 OR MIT-0 OR Apache-2.0`, `(Apache-2.0 OR MIT) AND BSD-3-Clause`, `Apache-2.0 AND ISC`, `Apache-2.0 OR BSL-1.0`, `MIT OR Zlib OR Apache-2.0`, `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT`, slash-syntax variants |

The tree is dominated by the standard Rust dual-licence pattern. `Unicode-3.0` covers
the icu4x-derived segmentation crates that `url`/`idna` pull through zed-reqwest — it is
already on the allow-list, and its 18-crate presence here is the dependency that
allow-list entry now earns. `Unlicense`, `0BSD`, `MIT-0` and `BSL-1.0` appear only inside
OR-expressions that also offer an already-allowed licence, so they need no allow-list
entries of their own.

### The one genuine gap: `NCSA`

`cargo deny check licenses` against the post-gpui workspace fails on **exactly one
crate**:

```
libfuzzer-sys v0.4.13 — license = "(MIT OR Apache-2.0) AND NCSA"
  └── rav1e v0.8.1
      └── ravif v0.13.0
          └── image v0.25.10
              └── gpui v0.2.2
```

`NCSA` is not on the `deny.toml` allow-list, and because it is AND-ed rather than OR-ed,
the permissive MIT/Apache half does not satisfy it. Two mitigating facts, both verified:

1. `libfuzzer-sys` reaches the graph only through rav1e's
   `[target."cfg(fuzzing)".dependencies]` — a target configuration that no real build
   ever sets. It does **not** appear in the Windows compile tree (`cargo tree` confirms
   its absence); cargo-deny flags it because `cfg(fuzzing)` is an unknown predicate it
   must conservatively treat as possible. No shipped binary will ever contain it.
2. NCSA is itself OSI-approved and permissive (BSD-like, no copyleft), so even in the
   impossible case the licence terms are not hostile.

The correct mechanism is therefore a crate-scoped **exception**, not an allow-list
addition — see §6. Adding `NCSA` to the global allow-list would silently admit any
future NCSA crate anywhere in the tree; the exception confines the admission to the one
crate that is never compiled anyway.

`bans` and `sources` both **pass** today (`cargo deny check bans sources`): all zed
forks are published on crates.io, so no unknown-source violation exists, and the many
duplicate versions are warnings under the existing `multiple-versions = "warn"` policy.
One pre-existing failure is unrelated to gpui: `wasmi`/`wasmi_core` 2.0.2 are yanked in
the current index — present identically in the pre-gpui baseline lock, so it is not this
integration's doing, but the gate is red until it is resolved. The
`RUSTSEC-2026-0206` ignore also now reports `advisory-not-detected`; worth re-checking
when the wasmi situation is untangled.

## 4. Notable risk entries

### `zed-reqwest` — a forked HTTP/TLS client, mandatory

`gpui_http_client` is a **non-optional** dependency of gpui, and its reqwest is
`zed-reqwest 0.12.15-zed` — Zed's fork of reqwest 0.12.15, published as a separate
crates.io package, with `rustls-tls-native-roots`, `socks`, `multipart` and `http2`
enabled. This is the single most significant supply-chain fact in the tree:

- Security patches to upstream reqwest do not flow automatically into the fork; they
  flow when Zed rebases. A reqwest CVE now has a third-party latency buffer in front of
  it, and `cargo audit` does not track the fork's divergence — RUSTSEC advisories filed
  against `reqwest` will not match a package named `zed-reqwest`.
- It drags a full async network stack into a UI library: `hyper`, `hyper-rustls`,
  `tokio`, `rustls`, `ring` (assembly-backed cryptography), `tokio-socks`. Whatever
  else is trimmed, this stack is **not** trimmable — there is no feature that turns the
  HTTP client off. gpui uses it for remote image/asset loading.
- It is compiled but inert in nc-ui's use: native views load no remote content. The risk
  is dormant code with a large CVE surface, present in every gpui-enabled binary.

Mitigation available to nc-ui: none within the dependency graph — feature unification
means only upstream can make it optional. The honest record is that ADR-020's exception
includes a forked TLS-capable HTTP client as a permanent resident, and gpui version
bumps must diff `gpui_http_client`'s dependency list specifically. Companion crate
`zed-async-tar` (fork of async-tar, tar extraction) rides along with it.

### `zed-scap` — screen capture in the lockfile, not in the binary

`zed-scap 0.0.8-zed` (fork of the scap screen-capture library; the library crate inside
it is `scap_screen_capture`) **is present in both lockfiles** — nc-ui's and the kit's —
because gpui declares it and cargo's resolution records the edge. It is **not compiled**
in any default configuration: it activates only behind gpui's non-default
`screen-capture` feature, and `cargo tree` confirms its absence from the compile graph.
It matters because a screen-capture capability is exactly the kind of dependency a
reviewer wants to know exists before it is one feature flag away. The record: it exists
in the resolution, it is one feature flag away, and nc-ui must never enable that flag —
a note that costs one line here and saves a surprised audit later.

### `rustybuzz` — now duplicated with nc-ui's own text stack

`ncui-gpu-renderer` already depends on `rustybuzz 0.20.1` (ADR-012's fourth exception).
gpui pulls `rustybuzz 0.14.1` through `usvg 0.45`. The post-gpui lock therefore carries
**both** — same crate, two major versions, both compiled in a build that enables both
subsystems. This is not new supply-chain surface (the crate and its unmaintained status
were already accepted, and `RUSTSEC-2026-0206` is already ignored in `deny.toml`), but
it is duplicated compile cost and a doubled audit target, and it lands in the
`multiple-versions` warning list. `ttf-parser` fares worse: **three** versions now
(0.20.0, 0.21.1, 0.25.1). There is no deduplication available to nc-ui — usvg 0.45 pins
the old line — beyond the already-planned migration of the GPU text path off rustybuzz
entirely, which this makes more attractive rather than less.

### `blade-graphics` / `ash` / `naga` — Vulkan FFI compiled on Windows, never executed

gpui's Windows platform renders through DirectX directly — `src/platform/windows/`
contains `directx_renderer.rs`, `directx_devices.rs`, `directx_atlas.rs` and HLSL
shaders compiled at build time — and the `blade` module is `cfg`-gated to Linux/FreeBSD
and macOS-only. Yet **with default features, the blade stack compiles on Windows
anyway**: `blade-graphics`, `blade-macros`, `blade-util`, `ash`, `ash-window`,
`gpu-alloc`, `naga` and their helpers, 21 crates of Vulkan/SPIR-V FFI. The mechanism is
a cargo limitation, not a zed defect: gpui's default features include `wayland` and
`x11`, those features reference `blade-graphics`, the dependency is declared in the
platform-independent table (because `macos-blade` also needs it), and cargo features
cannot be target-gated. Every Windows consumer of gpui pays for a Vulkan stack no
Windows code path can execute. This is the single largest trimming win available —
see §5.

### The weight nobody ordered: an AV1 encoder and more

Because gpui depends on `image` without trimming its defaults, `default-formats`
includes `avif`, which pulls `ravif` — which pulls **`rav1e`, a complete AV1 video
encoder** (with `rayon`, `nasm`-grade assembly paths and its own large dependency
fan-out) — into the compile tree. Verified present in the real Windows compile tree,
not merely the lock. Likewise `exr`, `tiff`, `qoi`, `gif`, `png` (twice: 0.17 and
0.18), `zune-jpeg`, `image-webp`. A native settings panel needs PNG and maybe JPEG; it
does not need to encode AV1 video. Like the HTTP stack, this is **not trimmable by
nc-ui**: gpui's own dependency edge enables the defaults, and features are additive.
Recorded here as upstream weight, and as the strongest argument that gpui version bumps
deserve real review rather than a rubber stamp.

### The zed fork inventory

Five crates in the resolution are Zed forks published under `zed-*` names, plus twelve
`gpui*`/`zed`-lineage sub-crates split out of the zed repository:

| Fork | Version | Upstream | Compiled on Windows? |
|---|---|---|---|
| `zed-reqwest` | 0.12.15-zed | reqwest 0.12.15 | yes — mandatory |
| `zed-async-tar` | 0.5.0-zed | async-tar | yes — with the http client |
| `zed-font-kit` | 0.14.1-zed | font-kit 0.14 | Linux/macOS only |
| `zed-xim` | 0.4.0-zed | xim | Linux only |
| `zed-scap` | 0.0.8-zed | scap | lockfile only — feature-gated off |

None is a `sources` violation — all are crates.io-published — but each is a place where
the upstream maintenance chain has an extra link, and `cargo audit` matches advisories
by package name, so advisories filed against the originals will not fire on the forks.

## 5. Feature trimming: what gpui exposes, and what nc-ui should use

gpui 0.2.2's complete feature surface, read from its published `Cargo.toml`:

```
default          = ["font-kit", "wayland", "x11", "windows-manifest"]
inspector        = ["gpui_macros/inspector"]
leak-detection   = ["backtrace"]
macos-blade      = ["blade-graphics", "blade-macros", "blade-util", "bytemuck", "objc2", "objc2-metal"]
runtime_shaders  = []
screen-capture   = ["scap"]
test-support     = ["leak-detection", "collections/test-support", "rand",
                    "util/test-support", "http_client/test-support", "wayland", "x11"]
wayland          = [blade stack, ashpd, cosmic-text, font-kit, wayland crates, xkbcommon, open, ...]
windows-manifest = []
x11              = [blade stack, ashpd, cosmic-text, font-kit, x11rb, xim, xkbcommon, x11-clipboard, open, ...]
```

Read against nc-ui's Windows-only scope, each default feature is:

- **`wayland`, `x11`** — pure dead weight on Windows. Every crate they name that is
  Linux-specific simply doesn't compile, but the blade/ash/naga core they activate is
  platform-independent, so it compiles everywhere. **Drop.**
- **`font-kit`** — activates zed-font-kit, which is only even declared for Linux/macOS.
  On Windows it is a no-op feature because gpui's Windows text stack is DirectWrite
  (`Win32_Graphics_DirectWrite` in its `windows` crate features). Drop for free.
- **`windows-manifest`** — the one default that matters on Windows. gpui does **not**
  set DPI awareness programmatically (verified: no `SetProcessDpiAwareness*` call in its
  Windows platform sources); it relies entirely on this feature's embedded manifest,
  which declares PerMonitorV2 and Common Controls v6. Without it, native windows are
  bitmap-scaled blur on high-DPI displays unless the embedding executable's own manifest
  declares awareness. **Keep.**

Optional-but-off features: `screen-capture` (the scap edge — nc-ui must not enable),
`inspector` (gpui's dev inspector — possibly useful later for kit development, off by
default), `leak-detection`/`test-support` (test-only), `macos-blade` (irrelevant on
Windows).

**Verified recommendation:**

```toml
gpui = { version = "0.2.2", optional = true, default-features = false, features = ["windows-manifest"] }
```

This configuration was **compile-verified on Windows**: `cargo check` of a scratch crate
against gpui 0.2.2 with exactly these flags completed clean in 1m 47s (cold cache,
including the whole transitive tree). The Windows platform module carries no feature
gates on `wayland`/`x11`/`font-kit` — confirmed by reading the sources — so nothing in
the Windows build references what trimming removes.

Measured effect on the Windows compile tree: **390 → 369 unique crates**, and the
entire dropped set is the Vulkan stack and its helpers:

```
ash, ash-window, bit-set, bit-vec, blade-graphics, blade-macros, blade-util,
bytemuck_derive, codespan-reporting, gpu-alloc, gpu-alloc-ash, gpu-alloc-types,
hexf-parse, hidden-trait, libloading, mint, naga, rustversion, spirv, termcolor,
unicode-width
```

Two honest caveats:

1. **21 crates is the whole win.** The other 369 are mandatory: gpui's HTTP client,
  image codecs, SVG stack, executor, and text pipeline have no feature switches. The
  trim is worth taking — it removes all Vulkan FFI from a DirectX application — but it
  does not change the shape of the exception, and the report should not pretend
  otherwise.
2. **The dev-dependency edge re-widens it.** `ncui-gpui`'s dev-dependencies currently
  pull `gpui` with `features = ["test-support"]`, and `test-support` itself enables
  `wayland` and `x11`. Feature unification means `cargo test -p ncui-gpui` compiles the
  blade stack even with the main edge trimmed. Shipped binaries are unaffected
  (dev-dependencies never reach them), but if test-build time matters, the dev edge can
  be trimmed too — gpui's test harness does not need wayland on Windows either, though
  it does need `test-support` itself for `#[gpui::test]`.

There is a second dev-dependency consequence worth recording: because the dev edge is
unconditional, gpui is in the workspace's resolve graph and cargo-deny evaluation
**regardless of the `gpui-engine` feature**. The feature gate controls what end-user
builds compile; it does not remove gpui from the audited graph. That is correct for
audit purposes — the tree that can ship is the tree that should be checked — but the
"feature-gated so the workspace compiles without it" property holds only for compile
time, not for resolution.

## 6. Recommended `deny.toml` changes

Concrete, annotated, in the file's own idiom. Do not apply blindly — the licence
exception is load-bearing, the rest is hygiene.

```toml
[licenses]
# 1. REQUIRED — without this the licence gate fails on gpui's tree.
#    libfuzzer-sys is AND-licensed (MIT OR Apache-2.0) AND NCSA, and reaches the
#    graph only through rav1e's cfg(fuzzing) target — a configuration no build of
#    this project ever sets. It is never compiled, verified against cargo tree for
#    x86_64-pc-windows-msvc; cargo-deny cannot evaluate the custom `fuzzing` cfg and
#    conservatively includes it. NCSA is OSI-approved and permissive. Scoped to the
#    one crate rather than allow-listed globally, so a future NCSA crate anywhere
#    else in the tree still surfaces for review.
exceptions = [
    { allow = ["NCSA"], crate = "libfuzzer-sys" },
]
```

```toml
[bans]
# 2. HYGIENE — extend the `skip` list for the duplicates gpui introduces that cannot
#    be deduplicated downstream. Only the ones that actually warn in the Windows
#    compile graph are listed here with their reasons; the full measured set of 53
#    newly-duplicated crates includes Linux/macOS-only entries (cocoa, ashpd,
#    wayland-*, nix) that warn identically and should be added as they are
#    encountered rather than pre-emptively, keeping the skip list honest.
#    Each of these is pulled by two disjoint subsystems and cannot be unified
#    without upstream changes:
skip = [
    # ... existing entries ...
    "rustybuzz",        # usvg 0.45 pins 0.14.x; ncui-gpu-renderer uses 0.20.x (ADR-012 exception 4)
    "ttf-parser",       # three versions: 0.20 (fontdb 0.16 via cosmic-text, Linux), 0.21 (rustybuzz 0.14), 0.25 (rustybuzz 0.20 + fontdb 0.23)
    "png",              # image 0.25 ecosystem split: 0.17 vs 0.18
    "rand",             # workspace already had 0.9; the gpui resolution adds 0.8 and 0.10 consumers
    "windows-registry", # gpui uses 0.5.3; zed-reqwest pins 0.4.0
    "derive_more",      # 0.99 (gpui) vs 2.x (workspace)
    "itertools",        # 0.13 (workspace) vs 0.14 (gpui)
    "event-listener",   # 2.x vs 5.x across the async ecosystems
    "fastrand",         # 1.x vs 2.x across the async ecosystems
    "pollster",         # 0.2 vs 1.0
    "quick-error",      # 1.x vs 2.x
]
```

Attribution notes, measured rather than guessed, because several of these are not what
they first appear:

- `strum` (0.26 vs 0.27) enters **entirely through `naga 25` in the blade stack** — the
  workspace has no strum of its own. If the feature trim of §5 is adopted, this duplicate
  disappears with the rest of the Vulkan tree and should not be added to the skip list at
  all. The same applies to `naga`, `spirv` and `codespan-reporting` themselves.
- `sha2` (0.10 vs 0.11) is **not** gpui's doing: the 0.11 line enters through
  `rust-embed-utils` on the wry side as a resolution ripple of the same lockfile update.
  Recorded here so the next reviewer does not spend time attributing it to gpui.
- `ttf-parser`'s 0.20 line is Linux-only (fontdb 0.16 via cosmic-text); on Windows the
  duplication is 0.21 vs 0.25, both real.
- `fontdb` itself duplicates (0.16 Linux-only via cosmic-text, 0.23 via usvg on all
  platforms) — a duplicate that mostly evaporates on Windows but warns in the
  platform-union graph cargo-deny evaluates.

No `[advisories]` changes are required for gpui itself — no advisory fires on the tree
today. The `wasmi` yank failure and the `advisory-not-detected` note on the existing
`RUSTSEC-2026-0206` ignore are pre-existing and unrelated to this integration; they are
recorded here only so this analysis does not read as having missed them.

No `[sources]` changes are required: every zed fork is crates.io-published.

## 7. Summary for the record

- gpui 0.2.2 adds **385 new unique crate names** to the workspace lockfile — the
  largest dependency exception in the project's history by a factor of fourteen over
  the previous largest. On Windows, **390 crates compile** with default features.
- Licences are dominated by the standard permissive set already allow-listed. Exactly
  one rejection exists — `NCSA` on `libfuzzer-sys`, a `cfg(fuzzing)` artifact that no
  build compiles — fixable with a one-line crate-scoped exception.
- The load-bearing risks are not licences: they are a **mandatory forked HTTP/TLS
  client** (`zed-reqwest`, invisible to RUSTSEC matching on reqwest), a **screen-capture
  dependency one feature flag away** (`zed-scap`, in the lock but not compiled), a
  **duplicated rustybuzz** (0.14 beside the workspace's 0.20), and **an AV1 encoder**
  compiled into a UI framework because gpui does not trim `image`'s default formats.
- `default-features = false, features = ["windows-manifest"]` is compile-verified on
  Windows and drops the entire 21-crate Vulkan FFI stack that Windows never executes.
  It is the only meaningful trim available; everything else in gpui is mandatory.
  Recommended for adoption in `ncui-gpui`'s main dependency edge.
