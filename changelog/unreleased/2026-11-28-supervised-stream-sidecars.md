- Manifest `stream` sidecars are now supervised: `runtime.sidecars` owns the
  process, the job object and an `OnFailure` restart policy, and hands the
  stable transport pipes to `StreamingSidecar` — a crashed backend restarts
  and answers the next call through the same wire instead of refusing every
  call forever. New composition primitives: `SidecarHandle::into_transport`,
  `StreamingSidecar::from_pipes`, `SidecarSpec.env_clear`, and a public
  `ProcessRunner::resolve_program` so the containment guarantee survives the
  supervised spawn lane. `Supervisor` is now `Clone`; teardown moved to the
  inner Arc's drop so a cloned handle cannot kill siblings' children. Streams
  are also bounded: one window may own at most 64 open streams, refused
  honestly rather than silently.
