# Tier 1 additions from rune-dev — design plan (nc-ui)

> Status: implemented. The guards described here run in CI's `policy` job
> (`scripts/file-size.sh`, `scripts/changelog-fold.py --check`, `scripts/hygiene-scan.sh --tree`)
> and in the Makefile's tier-1 appendix. Nothing below changed an existing gate: the checks
> were added beside the `policy` job and `make gate`, exactly as planned.
> Import Tier 1 *guards* (size, shape, changelog, hygiene) without importing
> any rune-dev *weakening* (see §5).

## Sources inspected (exact)

nc-ui (target):

- `G:\nc-ui\Cargo.toml` — workspace lints: `unsafe_code = "forbid"`, clippy
  `unwrap_used/expect_used/panic/todo/unimplemented/dbg_macro/print_* = "deny"`,
  `RUSTFLAGS` not here, pedantic warn; 19 `ncui-*` members via `crates/*`.
- `G:\nc-ui\.github\workflows\ci.yml` — jobs `rust`, `adapters`, `client`,
  `policy` (ncui-placeholders/ncui-arch/ncui-registry/correct_test_counts),
  `benchmark`, `soak`, `fuzz`, `release`, `supply-chain`. No size/shape/
  changelog/hygiene job.
- `G:\nc-ui\Makefile` — targets `gate: fmt lint test doc tools policy client`,
  `policy` builds `tools/` then runs placeholders/arch/registry/counts.
  No `scripts/` dir, no `changelog/` dir, no githooks.
- `G:\nc-ui\tools\Cargo.toml` — separate tool workspace, mirrors main lints
  (`unsafe_code = "forbid"`, deny unwrap/panic/todo).
- `G:\nc-ui\tools\ncui-tools\src\lib.rs`, `scan.rs`, `arch.rs`, `bin/` —
  Rust verification tools (`ncui-placeholders`, `ncui-arch`, `ncui-registry`).
  `scan.rs` assembles banned tokens at runtime, first-component exemption,
  `ALWAYS_SKIPPED`/`POLICY_EXEMPT`. Python helpers live in `tools/*.py`
  (`correct_test_counts.py`, `build_release.py`, `linux_gate.py`).
- `G:\nc-ui\crates` — 19 crates: `ncui-a11y, ncui-abi, ncui-app, ncui-bench,
  ncui-cli, ncui-component, ncui-core, ncui-gpu-renderer, ncui-gpu, ncui-ipc,
  ncui-native, ncui-observability, ncui-runtime, ncui-scheduler,
  ncui-security, ncui-state, ncui-webview-wry, ncui-webview, ncui-workers`.
- `G:\nc-ui\docs\known-gaps.md`, `G:\nc-ui\docs\remaining-work.md` —
  prior art for recording deliberate gates vs. gaps.
- `G:\nc-ui\crates\ncui-core\Cargo.toml` — `[lints] workspace = true` pattern.

rune-dev (reference):

- `G:\rune-dev\AGENTS.md` lines 18–53 — Tier 1 rules: 800/1500 size cap,
  exceptions with decreasing ceilings, capability-shaped folders (40 warn /
  60 error), `changelog/unreleased/<YYYY-MM-DD>-<slug>.md` + fold,
  hygiene regex `/Users/|/private/var/|\.ts\.net|ses_[A-Za-z0-9]{20}`.
- `G:\rune-dev\scripts\file-size.sh` (141 lines) — awk-based non-test line
  counter, `#[cfg(test)]` exclusion, SOFT=800 HARD=1500 TESTCAP=1000,
  `--warn-only/--update-exceptions/--allow-raise`, `--update-exceptions`
  rewrites exceptions with ceilings-only-down, delegates to folder-shape.
- `G:\rune-dev\scripts\folder-shape.sh` (112 lines) — Python-embedded bash,
  prefix-family rule (`x.rs`/`x/` + 2×`x_*.rs` or 3 alone → `x/`), breadth
  40/60, skips `examples/ benches/ bin/`, exceptions file same ceiling rule.
- `G:\rune-dev\scripts\file-size-exceptions.txt`,
  `G:\rune-dev\scripts\folder-shape-exceptions.txt` — empty, header-only.
- `G:\rune-dev\scripts\changelog-fold` (52 lines) — Python, folds fragments
  under `## dev` newest-first, `--check` dry-run, orders by first-commit time
  else filename date.
- `G:\rune-dev\changelog\unreleased\README.md` + sample fragment
  `2026-11-05-describe-gpu.md` — one top-level bullet per file.
- `G:\rune-dev\CHANGELOG.md` — `## dev` section; no released versions yet.

Conventions to preserve in nc-ui: `Makefile` `.PHONY` + `## target:` help
style with `cd tools && …` subshells; `tools/*.py` Python helpers with
`--check`; `scripts/` does not exist yet so new scripts use
`kebab-case` + `.sh`/`.ps1` shims over a single Python core (see §1.3);
CI amendments extend the existing `policy` job (ubuntu-latest, pinned
`1.98.1`) rather than adding a parallel runner.

---

## 1. File-size guard

### 1.1 Rule (from rune-dev, adapted to nc-ui scope)

- Files under `crates/**/*.rs`, `tools/ncui-tools/src/**/*.rs`,
  `examples/**/*.rs` (if present), `packages/**/*.rs` (none today) are measured.
- **Non-test lines** = total lines minus lines inside `#[cfg(test)]` items
  (attribute line + braced item) and minus standalone test files
  (`tests.rs`, `*_tests.rs`, `*_test.rs`, `*/tests/*`). Test-only files and
  inline test modules are measured separately against `TESTCAP = 1000`.
- Production thresholds: **warn > 800**, **error > 1500** unless listed in
  the exceptions file with a per-file ceiling.
- The point is god-file / edit-collision prevention, not chopping files that
  read fine as one piece. A file over 800 but under 1500 warns (does not
  block). A file over 1500 blocks unless its ceiling covers its count.
- Test files/modules: **warn > 1000** (never error in Tier 1; matches
  rune-dev behaviour of warning only).

### 1.2 Exceptions file with decreasing ceilings

New file: `scripts/file-size-exceptions.txt`

Format: `<path> <ceiling>` — path is repo-relative, ceiling is the non-test
line count at grandfathering time. Sorted by path, one per line, `#` comments
allowed (ignored). Example (illustrative, not grandfathered):

```text
# <path> <ceiling>: non-test line ceilings for files above 1500. Ceilings only go down.
# Refresh with scripts/file-size.sh --update-exceptions.
crates/ncui-runtime/src/lib.rs 1620
```

Semantics (verbatim from rune-dev, required for safe import):

- Ceilings **only go down**. `--update-exceptions` without `--allow-raise`
  keeps the old ceiling when the file grew; it never raises one. A brand-new
  oversized file is refused with `not listing new oversized file …; use
  --allow-raise` unless `--allow-raise` is explicit.
- `--update-exceptions` also prunes entries for files now ≤ 1500 and rewrites
  entries for shrunk files to their current count.
- `scripts/file-size-exceptions.txt` ships **empty (header only)** in Tier 1
  unless the first run shows genuine >1500 files that cannot be split in the
  same PR. Bootstrap: run once with `--warn-only`, split what can be split,
  then `--update-exceptions --allow-raise` once with reviewer sign-off.
  No blanket `--allow-raise` in CI or `make gate`.

### 1.3 Script: `scripts/file-size.sh` + `scripts/file-size.ps1` over Python core

rune-dev's `file-size.sh` is bash + awk. That breaks the nc-ui requirement:
`make gate`/CI must run identically on Windows (`windows-latest` in the
`rust`/`adapters` jobs, `G:\nc-ui` dev machines). Do **not** port the awk
parser verbatim.

Design (same CLI, same output, same exit codes as rune-dev):

```text
scripts/file-size.py      — Python core, typed, stdlib only
scripts/file-size.sh      — #!/usr/bin/env bash; exec python3 scripts/file-size.py "$@"
scripts/file-size.ps1     — pwsh; & python scripts/file-size.py @args
```

- Core language: **Python 3.10+** with `from __future__ import annotations`
  and typed signatures (matches existing helpers `tools/correct_test_counts.py`
  style; keep `# noqa` style minimal). No Rust binary for this guard in
  Tier 1 — nc-ui `tools/` Rust tools cover policy scans; a Python core keeps
  the guard runnable on CI's `ubuntu-latest` `policy` job without building
  `tools/` first, and matches rune-dev's Python-embedded `folder-shape.sh`.
- Python core re-implements the rune-dev awk logic exactly:
  1. `git ls-files -z 'crates/*.rs' 'tools/**/*.rs' 'examples/*.rs'` (fallback:
     `rglob` when run outside a git checkout, e.g. `--check` in a tarball).
  2. Per-file state machine: `testfile` regex `(^|/)tests?\.rs$|_tests?\.rs$|/tests/`;
     `#[cfg(test)]` head detection; brace-depth tracking with strings/comments
     stripped (naïve `braces()` port, documented as heuristic — identical to
     rune-dev's). This is a *size* proxy, not a compiler parse; false +
     1–2 lines are acceptable because warn is advisory and error has a
     ceiling.
  3. Aggregation, exception lookup, sorted `print(f"{n:6d}  {sev}  {path}")`,
     `::error/::warning` annotations when `GITHUB_ACTIONS == "true"`.
- CLI (identical to rune-dev): `--warn-only | --update-exceptions |
  --allow-raise`. Unknown option → exit 2.
- `file-size.sh`/`file-size.ps1` delegate to `folder-shape` and merge
  counts, printing one summary:
  `over 800: <n>, over 1500: <n>, errors: <n>, warnings: <n>`.
- Exit: 0 if `errors == 0` or `--warn-only`/`--update-exceptions`; 1 otherwise.

`.sh` shim (exact):

```bash
#!/usr/bin/env bash
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
exec python3 scripts/file-size.py "$@"
```

`.ps1` shim (exact):

```powershell
param([Parameter(ValueFromRemainingArguments=$true)][string[]]$Args)
$root = (git rev-parse --show-toplevel)
Push-Location $root
try { & python scripts/file-size.py @Args } finally { Pop-Location }
exit $LASTEXITCODE
```

### 1.4 Makefile + CI wiring

`Makefile`:

```makefile
.PHONY: gate fmt lint test doc tools policy client hygiene size bench clean help

gate: fmt lint test doc tools policy client hygiene  # hygiene includes size
```

New target (matches existing `## target:` style):

```makefile
## hygiene: file-size, folder-shape, secret/absolute-path scan (Tier 1)
hygiene:
	bash scripts/file-size.sh
	pwsh -File scripts/file-size.ps1
	bash scripts/hygiene-scan.sh
	# (ps1 equivalent: pwsh -File scripts/hygiene-scan.ps1)
```

On platforms without `make`, the comment beside the target lists the commands
verbatim (as `Makefile` already requires).

`ci.yml` — extend existing `policy` job (no new runner fleet):

```yaml
      - name: File size and folder shape (Tier 1)
        run: bash scripts/file-size.sh
```

`policy` already runs on `ubuntu-latest` with checkout — zero new infra.
The `rust`/`adapters` jobs are untouched (they need no new deps).

---

## 2. Folder-shape guard (capability-shaped folders)

### 2.1 Rule (from rune-dev, adapted)

Port `folder-shape.sh` logic, retargeted to the 19-crate layout. The rule
keeps folders shaped by capability, not by type or accident:

- **Prefix family:** a folder containing `x.rs` or `x/` plus 2+ `x_*.rs`
  siblings (or 3+ `x_*.rs` alone) → error, `move into x/`. Prefix is the
  stem up to the first `_` or `-`.
- **Folder breadth:** > 40 non-test source files in one folder → warn;
  > 60 → error, `group into capability subfolders`.
- Test files (`*_tests.rs`, `*_test.rs`, `tests.rs`, `*.test.ts`, `*/tests/`)
  are not counted. Files directly in `examples/`, `benches/`, `bin/` are
  skipped (each is its own program).
- Extensions: `.rs` only for Tier 1 (nc-ui is Rust-first; add `.ts/.tsx`
  when `packages/ncui-client` grows — note `ncui-placeholders` already scans
  `packages/` with those extensions in `scan.rs` `EXTENSIONS`).

nc-ui adaptation: each `crates/ncui-*/src/` is already a capability folder
(ADR-011 layering). The guard therefore fires *inside* crates: e.g.
`crates/ncui-runtime/src/` must not accumulate `scheduler_bridge.rs`,
`scheduler_*.rs` sprawl — it must become `scheduler/` with a `mod.rs`.
`crates/` top level is skipped (it holds crate directories, not `.rs`).

### 2.2 Script + exceptions

```text
scripts/folder-shape.py          — Python core (typed, stdlib)
scripts/folder-shape.sh          — exec python3 scripts/folder-shape.py "$@"
scripts/folder-shape.ps1         — & python scripts/folder-shape.py @Args
scripts/folder-shape-exceptions.txt
```

Exceptions format: `<key> [ceiling]` where key is `<dir>/<prefix>_*` or
`<dir>/`, ceiling optional (default unbounded, i.e. error always). Same
ceiling semantics as §1.2: `--update-exceptions` (no `--allow-raise`) never
raises a ceiling and never invents a new entry; `--allow-raise` must be
explicit. Ships empty in Tier 1; first violations are fixed by moving files,
not by exception.

Invocation: **not standalone in CI**. It runs *inside* `file-size.sh`
(rune-dev behaviour), so one command gives both. `--update-exceptions` on
`file-size.sh` forwards to both updaters. Direct `bash
scripts/folder-shape.sh --warn-only` is supported for debugging.

Exit/output: same summary line convention:
`folder shape: families: <n>, folders over 40: <n>, errors: <n>, warnings: <n>`,
merged into the `file-size.sh` summary. `::error file=…::` annotations for
GitHub Actions.

### 2.3 Makefile/CI

No separate target — folded into `make hygiene` / `bash scripts/file-size.sh`
(§1.4). Any split PR that moves files (`x_*.rs` → `x/`) must also update
the crate's `mod` declarations in declaration order (rune-dev convention:
`lib.rs` `mod`/`pub use` one per line, sorted — adopt as a Tier 1 doc note,
not a new lint).

---

## 3. Changelog / unreleased flow

### 3.1 Layout (new)

nc-ui has no `CHANGELOG.md` and no `changelog/` today. Tier 1 adds:

```text
CHANGELOG.md                            — # Changelog + ## [Unreleased] + ## [0.1.0]
changelog/unreleased/README.md          — contributor instructions (see §3.2)
changelog/unreleased/<YYYY-MM-DD>-<slug>.md  — one fragment per change
scripts/changelog-fold.py               — folder (Python core, typed)
scripts/changelog-fold.sh / .ps1        — shims (exec python3 / python)
```

Naming: `CHANGELOG.md` (upper, existing `Cargo.toml` style uses upper for
license/repo fields); folder `changelog/unreleased/` matches rune-dev
exactly (lowercase) to keep muscle memory.

`CHANGELOG.md` initial shape:

```markdown
# Changelog

All notable changes to NC-UI are documented here. This uses Keep a Changelog
and Semantic Versioning.

## [Unreleased]

- (entries folded from changelog/unreleased/)
```

rune-dev uses `## dev` because it has no releases. nc-ui already versions
(`Cargo.toml` `0.1.0`), so `## [Unreleased]` is the correct head; fold
targets that heading (single string constant `UNRELEASED_HEADING =
"## [Unreleased]"`).

### 3.2 Fragment contract + template

One file per change, `<YYYY-MM-DD>-<slug>.md`, holding **one top-level
Markdown bullet** in the style of the eventual `CHANGELOG.md` entries
(adapted from `G:\rune-dev\changelog\unreleased\README.md`):

> One file per change, `<YYYY-MM-DD>-<slug>.md`, holding one top-level
> Markdown bullet in the style of the `CHANGELOG.md` entries.
> `scripts/changelog-fold` prepends the fragments under `## [Unreleased]`
> (newest first) and deletes them; `--check` shows what it would do.

Template `changelog/unreleased/README.md` (plus `git` ignored `*.md` guard?
No — README stays tracked):

```md
# Unreleased fragments

One file per change that moves output, breaks a build, changes a gate, or
changes a public API. Name it `<YYYY-MM-DD>-<slug>.md` (lowercase kebab,
date = commit date). Hold exactly one top-level bullet, `- **<what changed>.**
<one-sentence why + migration if breaking>`. No secrets, no absolute paths,
no session IDs (see docs/tier1-plan.md §4).
```

Fragment example (style anchor — adapt to nc-ui crate names):

```markdown
- **`ncui-runtime`: `IslandHost::visible_rows` now clips rows out of view.** A page fetch naming a scrolled-out row no longer returns it, so a repaint that only moves the viewport does not resend rows the canvas will not draw.
```

### 3.3 Fold script design

`scripts/changelog-fold.py` — typed Python, stdlib only. Behaviour is a
line-faithful port of `G:\rune-dev\scripts\changelog-fold` with two nc-ui
changes: `UNRELEASED_HEADING = "## [Unreleased]"` (not `## dev`), and
`CHANGELOG.md` required (error if missing, no silent create).

1. `git rev-parse --show-toplevel` → `chdir(root)`.
2. `--check` → dry-run: print `would fold <file>` per fragment, exit 0,
   mutate nothing.
3. Else order fragments newest-first by first-commit time
   (`git log --diff-filter=A --format=%ct -1 -- <path>`), falling back to
   filename date `YYYY-MM-DD`, then basename.
4. Read `CHANGELOG.md`, find `UNRELEASED_HEADING`, insert the fragment block
   after leading prose (note line preserved), before existing entries or
   next heading. Split fragments with `\n\n`, join with `\n\n`.
5. Write `CHANGELOG.md`, delete fragment files, print `folded <file>` each.
6. Empty fragments → `no fragments`, exit 0.

Shims: `.sh` (`exec python3 scripts/changelog-fold.py "$@"`) and `.ps1`
(`& python scripts/changelog-fold.py @Args`), matching §1.3.

Usage (fold at release time, never in a feature PR):

```bash
python3 scripts/changelog-fold.py --check   # CI: would this fold cleanly?
python3 scripts/changelog-fold.py            # maintainer: fold + delete
```

### 3.4 Makefile/CI + process rule

- `Makefile`: no `gate` dependency (fold is a maintainer action, not a gate).
  Document in `## help` area as a comment block, plus `changelog-fold` target:

```makefile
## changelog-fold: fold changelog/unreleased/*.md into CHANGELOG.md (release-time only)
changelog-fold:
	python3 scripts/changelog-fold.py --check
```

- CI: `policy` job gains a `--check` step (fails on malformed fragment:
  missing bullet, absolute path, secret):

```yaml
      - name: Changelog fragments are well-formed
        run: python3 scripts/changelog-fold.py --check
```

- Process rule (add to `docs/remaining-work.md` or future `AGENTS.md`):
  any change that moves an app's output, breaks a build, or weakens a gate
  requires one fragment. Fragments are *not* folded by contributors; the
  maintainer folds at release. This mirrors rune-dev's `dev` workflow
  without importing rune-dev's `production`-branch deploy step (nc-ui has
  no `production` branch — do not import).

---

## 4. Hygiene scan (pre-commit + CI)

### 4.1 What is banned

Exact rune-dev regex, imported **without weakening**:

```text
/Users/|/private/var/|\.ts\.net|ses_[A-Za-z0-9]{20}
```

Meaning (rune-dev `AGENTS.md` lines 43–53, adapted to nc-ui):

- secrets, tokens, credential file contents (catch-all companion patterns
  below);
- home-folder / temp paths, machine names, tailnet hostnames — write
  repo-relative paths, use `host.example.ts.net` in tests;
- session IDs (`ses_<20 alnum>`), chat/Slack links, quoted private sessions;
- internal guide citations (say what the code does on its own terms);
- agent handoff notes (`REVIEW-TO-PARENT.md` etc.), raw logs/benchmark dumps;
- generated media nothing links to.

nc-ui extension (additive, not a substitute): Windows absolute paths
`[A-Za-z]:\\Users\\`, `C:\\Users` style drive-letter homes, UNC `\\`. The
four rune-dev patterns stay the minimum; the scan also flags
`[A-Za-z]:\\` + `Users`, `G:\\nc-*` dev-machine roots, and
`C:\Users\<name>` in staged text. This closes the obvious platform gap: a
rune-dev macOS-style pattern alone would miss every `G:\…` leak in nc-ui.

Companion secret patterns (cheap, low-FP, pre-commit only, CI warn→error):

```text
(?i)(api[_-]?key|secret|token|password)\s*[:=]\s*['"]?[A-Za-z0-9/_+\-]{16,}['"]?
ghp_[A-Za-z0-9]{36} | github_pat_[A-Za-z0-9_]{22,} | sk-[A-Za-z0-9]{20,}
```

These are heuristic; the *contract* pattern set is the primitive above.
Any suppression requires an inline `ncui-allow-hygiene: <reason>` (mirrors
`ncui-allow-placeholder:` in `scan.rs`, requires non-empty reason; empty
suppression is itself a finding). First-component exemption only:
`tests/fixtures/examples/docs` + `changelog/unreleased/README.md` are
*reported* as exempt, never silently skipped (matches `scan.rs`
`POLICY_EXEMPT` behaviour).

### 4.2 Script: `scripts/hygiene-scan.py` + shims + pre-commit hook

```text
scripts/hygiene-scan.py     — Python core, typed, stdlib
scripts/hygiene-scan.sh     — exec python3 scripts/hygiene-scan.py --staged "$@"
scripts/hygiene-scan.ps1    — & python scripts/hygiene-scan.py --staged @Args
.githooks/pre-commit        — POSIX sh hook; execs hygienic scan on staged diff
.githooks/pre-commit.ps1    — pwsh hook (core.hooksPath on Windows)
scripts/install-hooks.sh / .ps1 — git config core.hooksPath .githooks
```

Core modes:

- `--staged` (pre-commit): scans `git diff --cached --no-color -U0` added
  lines only (plus untracked files that are staged via `git add -N`? No —
  added lines only, matching rune-dev's `git diff --cached | rg`).
  Prints `file:line:pattern` per hit, exit 1 on any hit.
- `--tree [paths]` (CI): scans `git ls-files` tree (or given paths), same
  patterns, exit 1 on any hit. This catches committed leaks CI would have
  missed pre-commit.
- `--update-allowlist`? **Not in Tier 1.** Allowlist is inline-only
  (`ncui-allow-hygiene: reason`), per-file, auditable in diff. No silent
  global denylist (would hide leaks).

Hook file `.githooks/pre-commit` (exact):

```sh
#!/bin/sh
exec sh scripts/hygiene-scan.sh --staged
```

Install: `bash scripts/install-hooks.sh` runs
`git config core.hooksPath .githooks`. PowerShell equivalent sets the same
key. `.githooks/` is tracked (so CI and new clones see it); `core.hooksPath`
is per-machine, which is fine — CI does not depend on the hook.

### 4.3 CI job design

Extend `policy` (cheapest, already builds `tools/`):

```yaml
      - name: Hygiene scan (secrets, absolute paths)
        run: bash scripts/hygiene-scan.sh --tree
```

Do **not** create a separate `hygiene` job in Tier 1. The `policy` job
already runs `ubuntu-latest` + checkout; the scan is stdlib Python, no new
runner cost, and its failures sit beside `ncui-placeholders` failures where
a reviewer expects policy violations. Split into its own job only if
`policy` runtime exceeds ~5 min (measure, then decide).

`Makefile`:

```makefile
## hygiene: file-size + folder-shape + secret/absolute-path scan
hygiene:
	bash scripts/file-size.sh
	bash scripts/hygiene-scan.sh --tree
	pwsh -File scripts/hygiene-scan.ps1 --tree   # Windows parity
```

`make gate` appends `hygiene` last (after `client`), so a red size/shape or
hygiene check never masks a compile/test failure order that CI already
reports — gate order mirrors `ci.yml` job order.

### 4.4 What hygiene explicitly does NOT do in Tier 1

- No history rewrite / secret rotation. On a hit: remove the line, rotate
  the secret, `git filter-repo` only by an explicit incident decision.
- No dependency on `rg`, `grep -P`, or GitHub secret-scanning API (keeps
  Windows `pwsh` parity; Python stdlib only).
- No `gitleaks`/`trufflehog` vendoring — their entropy rules are heavier
  than Tier 1 needs and would false-positive on `Cargo.lock` digests, test
  vectors, and WASM fixtures. Revisit only with measured FP rate.

---

## 5. What NOT to import (do not weaken nc-ui gates)

The following appear in or are adjacent to rune-dev and must **not** be
copied into nc-ui. Each item cites the nc-ui line that enforces the opposite.

| # | Do NOT import | nc-ui enforcer |
|---|---|---|
| N1 | `unsafe_code = "allow"` or removing `unsafe_code = "forbid"` for any crate | `Cargo.toml:83`, `tools/Cargo.toml:12` — `forbid` is workspace-wide; ADR-012 exceptions (FFI behind features, e.g. `global-hotkey`, `tray-icon`, `getrandom`) are confined per-crate, never a workspace allow |
| N2 | Softening `unwrap_used/expect_used/panic = "deny"` to `warn` | `Cargo.toml:93-99`, `tools/Cargo.toml:17-21` — tests may `#[allow(clippy::unwrap_used, reason = "…")]` (see `scan.rs` tests) but production never warns-only |
| N3 | Shipping size/shape/changelog/hygiene with **no CI** (rune-dev has no CI job for them — `site.yml`/`www.yml` only) | `ci.yml` `policy` job amendment (§1.4/§3.4/§4.3) — a guard without a CI step is documentation, not a gate; `Makefile` comment `make gate runs exactly what CI runs` forbids local-only checks |
| N4 | `todo!/unimplemented!` as a starting point (rune-dev forbids them too, but nc-ui's `ncui-placeholders` is the enforcement) | `tools/ncui-tools/src/scan.rs` + `ci.yml` placeholders steps — Tier 1 scanners reuse its suppression-protocol shape, never replace it |
| N5 | `## dev` changelog heading + `production`-branch deploy flow | nc-ui versions (`Cargo.toml` `0.1.0`); use `## [Unreleased]` + fold at release (§3). No `production` branch to import |
| N6 | rune-dev Python-heavy bench/motion audits (`app-audit`, `motion-audit`, `rune-hot`) | nc-ui's evidence standard is Rust tests + `policy` binaries + `benchmark`/`soak`/`fuzz` jobs; Tier 1 imports *housekeeping* gates only, not the performance audit stack |
| N7 | Raising size/shape ceilings as routine PR practice | `--allow-raise` is explicit, review-visible, and ceilings only go down (§1.2/§2.2). A PR that raises a ceiling must name the reviewer + split plan |
| N8 | `scripts/*` as bash-only with no pwsh path (rune-dev `*.sh` assume bash) | §1.3/§2.2/§4.2 shims — every new script ships `.sh` + `.ps1` + Python core or it does not ship |
| N9 | `gen`/`target/`/lockfile exemption by glob-anywhere | `scan.rs` first-component exemption + `ALWAYS_SKIPPED` — Tier 1 scanners copy that, never `contains("docs")` anywhere in path |

Review checklist for any Tier 1 PR (zero-trust, mirrors `make gate`):

1. `unsafe_code` still `forbid`, unwrap/panic still `deny` — no manifest edit
   weakens them (diff `Cargo.toml`/`tools/Cargo.toml`).
2. New script has all three files (`*.py` + `*.sh` + `*.ps1`), typed Python,
   stdlib only, `--check`/`--help` works on Linux + Windows.
3. Exceptions files only gain entries via `--update-exceptions --allow-raise`
   with a reviewer named in the PR.
4. `Makefile` target + `ci.yml` `policy` step land in the same PR.
5. Changelog fragment added iff the change moves output / breaks builds.

---

## 6. Implementation order

1. **Hygiene first** (`scripts/hygiene-scan.py` + hook + `policy` step) —
   protects the subsequent `docs/changelog` work from leaking the very
   paths this plan cites. Bootstrap: run `--tree`, fix hits by editing,
   not by allowlist.
2. **File-size** (`scripts/file-size.py/.sh/.ps1` + empty exceptions file) —
   run `--warn-only`, split or shrink the worst offenders or record one
   explicit `--allow-raise` bootstrap. Then wire `policy`.
3. **Folder-shape** (folded into file-size) — same run; fix prefix families
   by `x_*.rs → x/` moves, not exceptions.
4. **Changelog** (`CHANGELOG.md` + `changelog/unreleased/README.md` +
   fold) — no gating impact; uncomment the CI `--check` step last so the
   empty-tree case (`no fragments`) passes from day one.
5. **Docs**: record the four guards + `## [Unreleased]` flow in
   `docs/remaining-work.md` (new section) and keep this plan as the design
   record under `docs/tier1-plan.md`.

## 7. Risks / unverified

- Non-test line counting via brace-tracking is approximate (comments/strings
  with braces). Mitigated by warn-not-error at 800 and ceiling at 1500.
- Windows absolute-path companion regex may FP on test fixtures that embed
  `C:\…`; mitigated by inline `ncui-allow-hygiene:` with reason.
- `policy` job runtime increase is one Python invocation (≈1–2 s) + fold
  `--check` (no-op when empty).
- Folder-shape breadth (40/60) is uncalibrated against the 19 crates'
  current `src/` sizes; first `--warn-only` run may need a threshold note
  in `docs/known-gaps.md` if a crate legitimately exceeds it.

---

### Audit artifacts

This plan is the only artifact in Tier 1 design. Implementation artifacts
land as §1–§4 `scripts/`, `changelog/`, `CHANGELOG.md`, `.githooks/`,
`Makefile` `hygiene` target, and `ci.yml` `policy` steps, each citable from
`protocols/feature-registry.json` under `process.verification_tooling`
(reference `docs/known-gaps.md` registry section) when they ship.
