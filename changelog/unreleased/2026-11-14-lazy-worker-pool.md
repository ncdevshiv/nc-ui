- **The worker pool now spawns its threads on first use, not at startup.** `WorkerPool::new`
built its bookkeeping but also spawned `max_workers` operating-system threads — which defaults to
`cpu_count.clamp(1, 64)` — so every runtime paid for a stack per worker the moment it started,
before a single background task existed. On this 12-core host that was twelve threads into every
window-with-a-page, and nothing in `ncui-runtime` or `ncui-cli` ever calls `workers.submit()`: a
minimal application was holding twelve idle threads it never used. The pool is now built without
its threads and spawns them lazily, on the first submitted task (or an explicit `start()`), so an
application that never runs background work spawns none.

This is the worker-pool half of A2's "lazy-init the subsystems a minimal app does not touch." The
other two subsystems A2 names were already lazy: the GPU renderer holds `host: Option<IslandHost>`
that stays `None` until islands are installed, and the metrics and log rings already bound their
pre-allocation (`capacity.min(64)` and `.min(1024)`). The worker pool was the one eager subsystem
left, and it is now lazy too.

The change is race-free and preserves the pool's contracts. `worker_count()` still reports the
configured capacity whether or not the threads exist, so an operator reading `ncui.workers.inspect`
sees the pool's shape either way; `has_started()` is the new accessor that tells "built but never
used" from "running." Workers begin in `Created` and move to `Active` on spawn, which is the
lifecycle the state machine already declared. Thread-creation failure is still an error the caller
handles — it surfaces from the first `submit()` or `start()` rather than from `new()`, and
`Runtime::start`'s doc now says so rather than claiming construction catches it. Unit-tested: a
fresh pool reports `has_started() == false` and its workers `Created`, `start()` is idempotent, a
second `start()` is a no-op, and every existing lifecycle, cancellation, backpressure and shutdown
test still passes once they `start()` before suspending.

**A2's measured target is not met, and the measurement shows why lazy-init cannot meet it.** The
goal commits to an idle process-tree working set of ≤ 300 MiB, proven by measurement. Measured with
the comparison harness's own Win32 walk (`scripts/measure_a2_working_set.py`, the same
`process_tree_memory` the comparison table uses, five repeats, median) on a minimal scaffolded app —
a window and a counter, granted only `read.metrics` and `window`:

    tree total   317.4 MiB   (target ≤ 300)
    msedgewebview2.exe  301.6 MiB
    ncui.exe             23.1 MiB

`ncui.exe` at 23 MiB is the lean host the audit recorded (~26 MiB); the lazy worker pool did its
job. But **WebView2's own processes alone measure 301.6 MiB — over the 300 MiB target before the
host contributes anything** — and the minimal app's worker pool is spawning zero threads. So no
amount of lazy-initialising host subsystems can bring this row under target while the system
WebView2 is the engine: the remaining gap is the web engine's multi-process footprint, which is an
architectural property of the port, not an allocation the host controls. A2 is recorded in
`docs/goal-prompt.md` and `docs/known-gaps.md` as blocked on that, with this decomposition as the
evidence — not quietly marked done.
