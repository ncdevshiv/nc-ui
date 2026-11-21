- **The Windows cross-framework comparison was re-run end to end, and it reproduces.** F1 asks
  NC-UI to win or tie every comparison row on every platform it runs on, measured rather than
  claimed. The Windows legs — the one host reproducible now — were re-measured in a single pass
  through the comparison harness (`python comparison/bench/compare.py`, repeats 5, rounds 200), all
  four configs: the real Electron app, the real Tauri app, and the NC-UI port of each. The harness is
  the evidence; the numbers below are its fresh output, written to
  `comparison/bench/results/windows.json`.

**The result reproduces, on every row.** Cold start to a visible window: NC-UI **51.3–51.6 ms**
against Electron's **261.1 ms** — five times faster — and level with Tauri's **51.5 ms**. Idle CPU:
**0.00 %** for all three, level. Distributed size: NC-UI **11.4 MiB** against Electron's
**356.8 MiB** — two orders of magnitude smaller. The rows where NC-UI is *not* best reproduced too,
with the same causes `comparison/README.md` already documents rather than hides: IPC round trip
(NC-UI **0.310–0.324 ms** against Electron **0.210 ms** and Tauri **0.252 ms** — a NC-UI page call
crosses the webview boundary twice and the host's event loop makes a thread hop Electron's direct C++
IPC does not, and `wry` exposes no `PostWebMessageAsJson` seam to avoid it) and idle memory (NC-UI
**331–332 MiB** against Electron's **291.8 MiB** — the WebView2 runtime keeps a browser process of
its own). A comparison that only ever shows the flattering rows is marketing; this one re-measures the
unflattering ones and they land where the written causes say they should.

**Why re-running is the deliverable, not a formality.** The individual timings move a little between
runs — Electron's cold start was 308.8 ms in the run the tables were first written from and 261.1 ms
here; NC-UI's IPC was 0.375 ms and is 0.310 ms — which is the machine, not the comparison. The verdicts
do not move: NC-UI wins cold start and size against Electron, ties Tauri on startup and idle CPU, and
loses IPC to both and disk to Tauri, every time. Confirming that the causes still explain the numbers
is what separates a measurement from a table transcribed into a document. The fresh table is recorded
in `comparison/README.md` under "Reproducing these numbers".

**Still open on F1, and why.** The **Zero Native** fourth side has no verified upstream in this repo,
so its figures remain a claim, not a measurement. The **Tauri 2** upgrade from 1.2 is a port of the
comparison app, not a version bump — Tauri 2 overhauled the menu API, added the capabilities and
permissions model, and removed the `custom-protocol` feature — and needs a Tauri 2 build of the app to
measure against. The **Linux and macOS** legs wait on those hosts (B3).
