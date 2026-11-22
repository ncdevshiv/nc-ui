- **A GPU island row whose text is taller than the row is now elided, not sliced.** This closes
the clipped-row gap from the completion audit: wrapping already fitted a row's text to its width,
but a block of lines taller than the row was cut off at the bottom mid-glyph, with nothing to say
the text continued. Now the lines that fit are drawn whole and the last visible one is truncated to
make room for a shaped ellipsis (`U+2026`), so a reader sees the text continues rather than a line
that merely stops. A row too short for even one whole line still draws its first — clipped at the
row's edge, as a row too short for its text always was — rather than going blank, because a blank
row tells a reader less than a clipped one.

**The decision is pure layout, so it lives with the wrapping it sits on.** `fit_lines` (how many of
a wrapped block's lines fit a row of a given height, and whether any were left out) and
`elide_line` (cut a line short to fit the marker, keeping its advance true so the ellipsis lands
where the removed glyphs were) are in `ncui-gpu-renderer`'s `text` module next to `wrap`. They work
on `ShapedLine`/`ShapedText` and nothing else — no font, no graphics adapter — which is what makes
them unit-testable over synthetic lines on every machine and benchable without the GPU stack. The
renderer's `draw_rows` calls them: shape the row's text, fit it to the row, mark the last visible
line when it overflowed. The ellipsis is shaped with the row's own font at the row's own size, so
it reads as part of the text rather than as a box the renderer invented.

**Measured: the common case is nanoseconds, and the overflow path is bounded.** `fit_lines` is one
float divide and a floor, and it replaces the multiply the old block-height computation did, so a
block that fits its row costs **~3.4 ns** and one that overflows **~3.2 ns** — the same, because
the decision is O(1) whether or not lines were left out. `elide_line`, which runs only on the
overflowing rows, is **~99 ns**. Against the GPU render and the megabyte-per-frame readback an
island already costs, none of this moves the frame (`island/*` in `ncui-bench`).

**Tested unit and end to end.** `fit_lines` is tested for a block that fits (not elided), a block
taller than its row (elided to the lines that fit), a row shorter than one line (still draws its
first, marked elided), and an empty block (neither); `elide_line` is tested to make room inside the
row and to keep at least one glyph on a single-letter line. The whole path is exercised end to end:
a real overflowing row — a long unbroken word that wraps past a short row's height — is driven
through the island route on a real GPU, and the frame comes back well formed with ink in the row's
band and nothing drawn below it. Gate: `cargo fmt`, `cargo clippy --workspace --all-targets`
(pedantic, no warnings) and the full `cargo test --workspace` suite (1472 tests) all pass, plus the
feature-gated `ncui-runtime` (`gpu-islands`, 168 lib + 12 island integration) and
`ncui-gpu-renderer` (`text-shaping`, 61 lib) suites.

**What this does not do.** The glyphs are still shaped and placed left-to-right: RTL/bidi runs and
ligatures/joining are the renderer's per-run layout work and remain open. The locale and
direction metadata (shipped separately) tells a reader *which way* an island lays out; this change
only stops a too-tall block from being silently cut off.
