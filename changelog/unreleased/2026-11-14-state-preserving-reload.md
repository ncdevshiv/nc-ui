- **`ncui run --watch` now preserves the page state the application asks to keep.** A
`--watch` reload used to be a blunt `window.location.reload()`, which destroyed every piece of
in-memory state the page held — a half-filled form, a scroll position, a counter mid-count —
so a dev loop that fires on every save made the person using it start over each time. The page
now designates what it wants kept by assigning it to `window.__ncui_state`; before reloading,
the runtime snapshots that one value into `sessionStorage`, and after the reload a bridge
script restores it and dispatches `ncui:state-restored`.

Not breaking: the bridge is **opt-in**. An application includes one `<script
src="/__ncui/reload.js">` tag on its own origin (the counter example now does), and one that
does not gets the old fresh-reload behaviour unchanged — the snapshot is guarded and restores
nothing when there was nothing to save. State the page does not put on `__ncui_state` is *not*
preserved: a reload tears down the JavaScript heap, which is what a reload is, so this is one
well-defined slot the application opts into, not a framework's component-tree resurrection.
That boundary is stated in `docs/known-gaps.md` rather than papered over.

The bridge is served from the runtime's own route table — the same mechanism that serves a GPU
island frame — so the asset server stays a file server that rewrites nothing, no script is
injected into the application's HTML, and the mechanism **adds no new dependency** (it is
`sessionStorage` plus a route, not a bundler or a file-watcher). `ncui-runtime` gained a
`reload` module owning the bridge script, the route and the reload signal; the `--watch` path
in `application` pushes the snapshot-then-reload signal in place of the bare reload.

Unit-tested: the route serves the bridge at its own path (and ignores a cache-busting query),
declines a path that is not the bridge with `None` rather than an empty body, the bridge reads
the same storage key the host writes and restores onto the same global the app assigns and
clears the key so a stale state is never resurrected, the reload signal snapshots *before* it
reloads, the signal is guarded so a missing or unserialisable state still reloads, and the
storage key is namespaced to the runtime. The counter example demonstrates it end to end.

**The sharp edge, found by tracing rather than by a passing test.** The bridge is a classic
script and the application's code is a deferred module, so the bridge restores
`window.__ncui_state` *before* the application runs. A page must therefore **seed its working
state from the restored global** on load. The first version of the example seeded `let count =
0` and wrote it back in `render()`, which mentioned `__ncui_state`, passed a check that only
looked for the name, and preserved nothing — the literal clobbered the restored value on the
first render. The counter example and the `ncui new` template now seed from the global, and a
test asserts the scaffolded script seeds from it rather than from a literal (and that the seed
precedes `render`), because a name-only assertion did not catch it.
