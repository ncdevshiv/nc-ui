# The gate answers only questions the machine can ask it

Three CI repairs, each one a case where a check demanded an answer that the machine it ran
on could not give, and a fourth where the check compared the wrong thing.

- **The Arabic-join test asserts where the font can answer and skips where it cannot.** The
  test shapes `ب` alone and inside `ببب` and requires the joined word to leave the isolated
  glyph behind. On macOS the system font carries the joining forms in an AAT `morx` table,
  which `rustybuzz` does not implement, so the shaper answers with the nominal glyph for
  both samples — and the previous revision turned that machine fact into a failure
  (`a joined beh must not keep the isolated form 911`), asserting precisely the case its own
  comment called inapplicable. The assertion now runs where the system font expresses
  joining in GSUB (Windows's Segoe UI, Linux's DejaVu Sans) and the shaper is required to
  apply it; a font that does not cover Arabic still skips through the `.notdef` guard.

- **The page-bridge e2e skips a displayless Linux.** The test drives a real `wry` window,
  whose Linux backend needs GTK, and the headless CI runner has no display: the engine's
  event loop panicked at startup. Like the runtime's real-window tests, the test now returns
  early on Linux when neither `DISPLAY` nor `WAYLAND_DISPLAY` is set, and runs for real
  everywhere a display exists.

- **The reproducibility check no longer compares build histories.** The check rebuilds after
  a `cargo clean` and compares digests, but left incremental compilation enabled — and on CI
  the first build starts from a cache the workflow restores, so the two builds differed in
  their cached incremental state rather than in source, and the Windows digest mismatched
  (`NOT reproducible: identical source produced different binaries`). Both builds now run
  with `CARGO_INCREMENTAL=0`, so the comparison is source against source.

Tested: `arabic_letters_take_their_joined_forms` passes with the strict assertion live on
Windows (`cargo test -p ncui-gpu-renderer --features text-shaping`); the page-bridge e2e
passes against a real WebView2 window on Windows; `python tools/test_build_release.py`
passes; both touched crates are clippy-clean with `-D warnings` and rustfmt-clean.
