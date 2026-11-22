- **A GPU island row's text is now split into direction runs before it is shaped, so mixed-direction
  text draws the right way round.** The shaping core is `rustybuzz`, a HarfBuzz port, and HarfBuzz
  shapes one direction at a time: it reorders a run that is all right-to-left, but a line handed to it
  whole is shaped in its first strong direction with the other runs left in logical order. A Hebrew
  word inside an English sentence was therefore shaped left-to-right and drew **backwards** — its
  letters in the wrong order, reading as gibberish. This closes C4's GPU shaping half: every line is
  first split into maximal single-direction runs (`bidi_runs`), each run is shaped with its own
  direction, and the runs are concatenated in the order they are read, with each glyph's cluster
  rebased onto the whole line so a caller can still map a glyph back to the character it came from.

**What the split decides.** Strong characters — letters — keep the direction their script reads.
A neutral (a space, punctuation, a digit) takes the direction of the strong text around it: the
surrounding run when both sides agree, the paragraph's base direction — the first strong character's —
when they disagree or it sits at an end. That is what makes two right-to-left words read as one
right-to-left phrase, the space between them pulled into the run so the word order reverses with the
letters, rather than each word reversing on its own while the words stay in the wrong order.

**Verified, not assumed.** The reordering was checked against a real font before and after. Pure
Hebrew and Arabic runs come out in visual order (a right-to-left run's clusters descend), which they
always did; the defect was the mixed case, where the embedded run kept logical order. Now: an embedded
Hebrew word's clusters descend within a sentence whose Latin stays ascending, two right-to-left words
land in one run, a left-to-right line stays a single run (so the common case goes through exactly one
shaper call, as before), and Arabic letters inside a word take a joined form rather than their isolated
one. On top of the unit tests, a real Hebrew row and a real Arabic row are driven through the island
route on a real GPU — the whole path from a page's request to composited pixels runs on right-to-left
text and comes back with the letters on it.

**Measured.** The split runs on every shaped line, so the plain left-to-right case — the overwhelmingly
common one — had better be cheap: **~672 ns** for a left-to-right line and **~818 ns** for a mixed one
(`bidi/*` in `ncui-bench`). Sub-microsecond, and small against the `rustybuzz` shaping and the GPU
render and megabyte readback the frame already pays for.

**Bounded, and the bounds are written down.** This is a bounded bidirectional algorithm, not the whole
of UAX #9: no directional marks (`LRM`/`RLM`/`RLE`/…), no paired-bracket resolution, and a number
follows its run's direction rather than being laid out by the algorithm's own number rules. The common
cases are handled and tested; `docs/known-gaps.md` records the limits and what a dedicated
bidirectional crate would add. Joining is verified for Arabic; Latin ligature substitution is the font's
own default feature, applied by the shaper when the font has it, not something this code adds.

**Gate.** `cargo fmt`, `cargo clippy --workspace --all-targets` (pedantic, no warnings), and the full
`cargo test --workspace` all pass, plus the feature-gated suites that own this code:
`ncui-gpu-renderer` (`text-shaping`, 68 lib tests — seven of them the new right-to-left/bidi/joining
ones) and `ncui-runtime` (`gpu-islands`, 258 across lib and integration — one of them the new
right-to-left end-to-end row).
