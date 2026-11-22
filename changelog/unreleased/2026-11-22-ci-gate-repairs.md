# The gate answers only questions the machine can ask it

Five CI repairs, each one a case where a check demanded an answer the machine it ran on
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

- **The reproducibility check builds with incremental compilation off, and the workflow no
  longer overrides the linker flags.** The check rebuilds after a `cargo clean` and
  compares digests; with incremental compilation enabled, a build that starts from a warm
  cache can digest differently from a clean rebuild of the same source. Both builds now run
  with `CARGO_INCREMENTAL=0`. That was still not enough, because the workflow set
  `RUSTFLAGS: -D warnings` for every job — and an environment variable *replaces* the
  rustflags from `.cargo/config.toml` rather than merging with them, so the `/Brepro`
  linker argument never reached the linker on CI. The preserved pair of builds proved it:
  twenty differing bytes, and a PE header timestamp in each that read as the wall clock of
  its own build. The environment variable is gone; the clippy and documentation steps that
  want warnings denied pass `-D warnings` themselves, and the linker flags live only in the
  config file.

- **A reproducibility mismatch keeps its evidence.** When the two builds differ, the check
  sets the first build aside instead of letting the rebuild overwrite it, and writes both
  binaries with a PE-level comparison — sizes, header timestamp, CodeView PDB GUID,
  differing byte offsets — under `artifacts/repro/`, which the release job now uploads even
  on failure. It was this pair that named the wall-clock timestamp, and with it the
  `RUSTFLAGS` override above.

- **A toast raise is serialized, and a refused tag is retried with a fresh one.** The
  platform keeps one notification store per identity, and the store has been observed to
  answer a provably-unique tag with the uniqueness constraint (`0x87AF0813`) when
  notifications follow one another quickly — a tag from this process carries the process's
  own sequence, so two tags from one process cannot be equal, and the refusal is the
  platform's transient, not a reuse. Raises now take a process-wide lock, and a uniqueness
  refusal is retried with a regenerated tag; any other failure is returned as the failure
  it is.

Tested: `arabic_letters_take_their_joined_forms` passes with the strict assertion live on
Windows (`cargo test -p ncui-gpu-renderer --features text-shaping`); the page-bridge e2e
passes against a real WebView2 window on Windows; `python tools/test_build_release.py`
passes; both touched crates are clippy-clean with `-D warnings` and rustfmt-clean.
