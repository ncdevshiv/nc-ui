# 2026-11-28 — 4K presenter upload fix

- **The 4K30 sustained defect is closed: the presenter was copying every frame
  twice.** The upload path ran a full-frame `.to_vec()` — 33MB of allocation
  churn per frame at 4K, a gigabyte a second — even when the producer's stride
  already satisfied the upload alignment, plus a fresh padding buffer per
  unaligned frame and a recreated texture view and bind group per frame.
  `write_texture` already performs its own staging copy, so every one of those
  copies was pure waste. The fix: zero-copy upload for aligned frames, a reused
  padding buffer, and a persistent view and bind group. The ten-minute
  re-measurement at 4K30: 18,000 frames in exactly 600s, push-to-presented p50
  4.64ms / p99 11.39ms against the 33.3ms budget — down from the failing
  p99 35.36ms — zero drops, degradation flat. 1080p30 improved with the same
  fix (p50 1.58ms).
