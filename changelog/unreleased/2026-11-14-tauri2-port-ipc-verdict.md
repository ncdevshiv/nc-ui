- **The comparison's Tauri app was ported from Tauri 1.2 to Tauri 2 and re-measured, and the
  upgrade flipped the IPC verdict.** F1 asks NC-UI to win or tie every comparison row against the
  incumbents *as they ship today*, measured rather than claimed. The comparison's Tauri
  application had been cloned at Tauri 1.2, which is no longer the version the project compares
  against: Tauri 2 is the current major. Measuring NC-UI against a museum piece would have been a
  comparison in name only, so the app was ported to Tauri 2 and both sides re-measured.

**It was a port, not a version bump, and the mechanical changes are the evidence.** Tauri 2
  rewrote the menu API (`CustomMenuItem`/`add_native_item` → `MenuItem::with_id`/`PredefinedMenuItem`,
  and the event now resolves its window through the app handle), replaced the allowlist with the
  capabilities and permissions model (`capabilities/default.json` granting `core:default`), moved
  `invoke` in the page from the `tauri` module to `core`, changed the config schema (`distDir` →
  `frontendDist`, `tauri.windows` → `app.windows`), and renamed the CLI's no-bundle flag
  (`-b none` → `--no-bundle`). The whole port lives in `comparison/bench/instrumentation/tauri/` —
  `main.rs`, `App.jsx`, `tauri.conf.json`, `package.json`, `Cargo.toml`, `capabilities/default.json` —
  so it is reviewable in one place and the harness reapplies it to a fresh clone on every run.
  `build_tauri` now reinstalls the frontend dependencies whenever the installed `@tauri-apps/api`
  predates the manifest, because a `node_modules` left by a 1.2 install satisfies an existence
  check while lacking the v2 `core` module the ported page imports.

**The Tauri 2 IPC regression is the finding, and it moved a row to NC-UI.** Measured on the
  identical workload (`invoke` → `return_string`, 200 calls, both sides unmodified from the
  upstream shape), Tauri 2's IPC round trip is **1.398 ms** p50 — about **6× slower** than the
  **0.252 ms** Tauri 1.2 measured before the upgrade, reproduced across two independent runs
  (1.398 and 1.548 ms). NC-UI measures **0.348 ms**, so the IPC row against Tauri goes from a
  Tauri win to an NC-UI win. The check that this is Tauri's change and not the measurement's:
  NC-UI's own IPC figure did not move — it is the same **0.31–0.37 ms** it measured against
  Electron and against Tauri 1.2. The regression is in Tauri's own bridge, on a path this harness
  does not touch. Startup (Tauri 2 **52.9 ms**, NC-UI **55.1 ms**), idle CPU (**0.00 %** both),
  idle memory (Tauri 2 **334.9 MiB**, NC-UI **330.4 MiB**) and size (Tauri 2 **8.5 MiB**, NC-UI
  **11.4 MiB**) are level or within a few percent. Against Electron nothing changed: NC-UI still
  starts ~5× faster and ships two orders of magnitude smaller, is level on idle CPU, and is slower
  on IPC (Electron **0.210 ms**, NC-UI **0.310 ms**) and larger at idle — the same architectural
  causes `comparison/README.md` documents rather than hides. The fresh tables are recorded in
  `comparison/README.md` under "Reproducing these numbers", and the F1 measured status in
  `docs/goal-prompt.md` now reads Tauri 2.

**Still open on F1, and why.** The **Zero Native** fourth side has no verified upstream in this
  repo, so its figures remain a claim, not a measurement. The **Linux and macOS** legs wait on
  those hosts (B3).
