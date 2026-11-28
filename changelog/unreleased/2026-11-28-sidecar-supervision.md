# 2026-11-28 — supervised sidecar lifecycle

- **The runtime grows supervised child processes: spawn, restart, terminate,
  never orphan.** The new `ncui-sidecar` crate owns the semantics —
  restart-on-crash with exponential backoff and a counted ceiling (a process
  that cannot stay up is a fault surfaced, not a loop run), clean exits stay
  down because finished work is not restarted, and every transition is
  inspectable truth. The never-orphan guarantee is structural on Windows:
  every incarnation is assigned to a job object created with
  `KILL_ON_JOB_CLOSE`, so the operating system itself kills every sidecar
  when the host dies, however it dies. The byte transport survives restart —
  a restarted child is a new process with new pipes, so the owner holds
  stable pipe ends instead: writes route to the current incarnation (the
  death window fails honestly with `BrokenPipe`), and a forwarder thread
  follows incarnations so reads never re-open anything. The runtime owns the
  supervisor (`runtime.sidecars`); dropping the runtime is the app-exit path
  the spec means. Evidence: seven crate-level supervision tests plus three
  runtime-integration proofs — crash-restart through the runtime with the
  transport intact, termination visible in the runtime's own inspect, and a
  sidecar measurably dying with its runtime
  (`crates/ncui-sidecar/tests/supervision.rs`,
  `crates/ncui-runtime/tests/sidecar_wiring.rs`).
