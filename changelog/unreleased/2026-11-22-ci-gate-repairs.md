# The gate answers only questions the machine can ask it

Four CI repairs, each one a case where a check demanded an answer the machine it ran on
could not give, or compared the wrong thing.

- **The Arabic-join test asserts where the font can answer and skips where it cannot.** The
  test shapes `ب` alone and inside `ببب` and requires the joined word to leave the isolated
  glyph behind. On macOS the system font carries the joining forms in an AAT `morx` table,
  which `rustybuzz` does not implement, so the shaper answers with the nominal glyph for
  both samples — and the previous revision turned that machine fact into a failure
  (`a joined beh must not keep the isolated form 911`), asserting precisely the case its own
  comment called inapplicable. The assertion now runs where the system font expresses
  joining in GSUB (Windows's Segoe UI, Linux's DejaVu Sans) and the shaper is required to
  apply it; a font that does not cover Arabic still skips through the `.notdef` guard.

- **The page-bridge e2e runs only where the engine can own its loop.** The test drives a
  real `wry` window. On Linux that needs GTK, and the headless CI runners have no display —
  the engine's event loop panicked at startup. On macOS the engine's loop owns the main
  thread (ADR-018) and a test harness cannot hand its own main thread over, so the host
  factory refused to spawn. Both are facts about the machine, not about the bridge: the
  test now returns early on a displayless Linux and on macOS, the same terms the runtime's
  real-window tests already used, and drives the page for real everywhere else.

- **The reproducibility check builds with incremental compilation off.** The check rebuilds
  after a `cargo clean` and compares digests; with incremental compilation enabled, a build
  that starts from a warm cache can digest differently from a clean rebuild of the same
  source, which would compare build histories rather than source. Both builds now run with
  `CARGO_INCREMENTAL=0`.

- **A reproducibility mismatch keeps its evidence.** When the two builds still differ, the
  check now sets the first build aside instead of letting the rebuild overwrite it, and
  writes both binaries with a PE-level comparison — sizes, header timestamp, CodeView PDB
  GUID, differing byte offsets — under `artifacts/repro/`, which the release job now
  uploads even on failure. A mismatch on one machine while another passes is a fact about
  the build environment; the pair turns the next investigation from a guess into a diff.

Tested: `arabic_letters_take_their_joined_forms` passes with the strict assertion live on
Windows (`cargo test -p ncui-gpu-renderer --features text-shaping`); the page-bridge e2e
passes against a real WebView2 window on Windows; `python tools/test_build_release.py`
passes; both touched crates are clippy-clean with `-D warnings` and rustfmt-clean.
