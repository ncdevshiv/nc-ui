# Pointer capture and timeline input integration

- Pointer capture is now input-model semantics: a button down inside an island
  captures it until release (`IslandHost::route_pointer_captured`), moves outside
  the island keep routing with honestly out-of-bounds content coordinates, and
  `click()` establishes capture through the same path so capture cannot stick.
  Shift/ctrl/alt modifier state travels on every pointer, wheel and scroll event —
  ctrl-wheel zoom's signal reaches its consumer. The web shell's timeline is now
  an island in the hit-test model, not only in the painted frame: the real-browser
  gate caught it missing (a painted but unhittable timeline) and now proves
  capture end-to-end — a mid-drag move far outside the island reads
  `island:timeline`, the post-release move reads `dom`. Unit coverage pins capture
  engagement, release, negative-coordinate honesty and region resolution during
  capture; the test suite itself corrected one unrealistic stream (clicks without
  releases) rather than weakening the semantics.
