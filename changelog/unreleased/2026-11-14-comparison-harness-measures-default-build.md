- **The comparison harness measures NC-UI's renderer-carrying default again.** The A3 work
(`--profile minimal`) split NC-UI's build features: `ncui-cli/wry-host` became the stripped
`minimal` profile (window host, no GPU-islands renderer) and the renderer moved to
`ncui-cli/wry-host-gpu`. The comparison harness's `build_ncui()` still built
`ncui-cli/wry-host` — which after that split is the ~4.9 MiB minimal build, not the
~11.3 MiB renderer-carrying default the distributed-size row documents. Left alone, the next
harness run would have silently reported NC-UI's distributed size as ~4.9 MiB against Electron's
356.8 MiB and Tauri's 3.2 MiB — a 2.3× "win" over Tauri that the harness does not actually
measure, because the ported applications are compared against the incumbents' *full* runtimes.
The committed `bench/results/windows.json` was measured before the split and is consistent
(NC-UI at 11.269 / 11.273 MiB); the fix keeps future runs consistent with it.

`build_ncui()` now builds `ncui-cli/wry-host-gpu`, the default (`standard`) profile, with a
comment naming why: the comparison is against Electron's and Tauri's full runtimes, so the figure
must be the build an application ships by default, not the stripped one. `comparison/README.md`
now says `wry-host-gpu` where it said `wry-host`, and spells out that `wry-host` alone is the
`minimal` profile and is not the build this row compares. The renderer-cost decomposition in the
README's honest-status section now names the post-split features.

Measured through the harness's own code path on this Windows host (importing
`comparison/bench/compare.py` and calling its `build_ncui()` and `dir_size`, and its
`wait_for_window` / `process_tree_memory` for the runtime rows), the figures reproduce:

    distributed size (ncui.exe + port)   11.299 MiB   (README: 11.3; was drifting to ~4.9)
    window-open, cold start (p50)         51.3 ms     (committed row: 51.9)
    idle working set, process tree (p50)  317.4 MiB

The minimal build is real and measured — 4.905 MiB, reported in the A3 changelog and the goal
prompt's A3 section — but it is a deliberate stripped profile for applications that draw no
islands, not the default this table compares.

This is part of F1: the "NC-UI is smaller" claim now rests on a harness that measures the build it
says it measures, rather than one that would have drifted to a stripped build after the A3 feature
split. The remaining F1 work — the Zero Native fourth side and the Tauri 2 upgrade — is recorded
in the goal prompt; Zero Native has no verified upstream in this repo, so its figures remain
`[E-claimed]` rather than measured.
