# ADR-019: Windows job objects for process-tree containment

## Status

Accepted (2026-11-22)

## Context

`process.run` promises that stopping a program stops *everything it started*. On Unix that
promise is kept with a process group: the child is started as a group leader and one signal
reaches the whole group. On Windows the promise was kept with `taskkill /T /F`, which walks
the parent chain — and a parent chain is not a process tree. A grandchild whose parent has
already exited, or one that detached itself, is not found by the walk and outlives the
stop. `docs/known-gaps.md` recorded exactly this residual: closing the gap "needs a job
object (Windows) or a cgroup (Linux), which `std` does not expose and this workspace cannot
reach without FFI or a new dependency."

The same gap applies to a long-lived sidecar's `stop`, which makes the same promise.

## Decision

`win32job` is added as a dependency of `ncui-native`, for Windows targets only. The
one-shot runner and the streaming sidecar put their child into a job object created with
`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, and the job lives in the frame that owns the child:
when the run returns, when the run times out, or when a sidecar is stopped, dropping the
job closes its last handle and every process in it terminates. `taskkill /T /F` and the
direct `kill` remain as the fallback for the case the job is refused, and on Unix nothing
changes.

This is ADR-012's ninth exception. `win32job` performs the `CreateJobObjectW`,
`AssignProcessToJobObject` and `SetInformationJobObject` calls internally, so this
workspace's own code stays free of `unsafe`; the crate is MIT OR Apache-2.0, has one
transitive dependency (`windows`), and is confined to `ncui-native`'s two process-running
modules.

## Consequences

* A run that *returns normally* now also ends the tree it started: a child that spawns a
  detached grandchild and exits can no longer leave work running behind it. This is a
  behaviour change, and it is the contract `process.run` already documented.
* Job membership is inherited at process creation. A grandchild created in the window
  between the child's process creation and its assignment is not in the job; the window is
  the process start-up time of the child, and the fallback walk still reaches the common
  case.
* A child that requests breakaway (`CREATE_BREAKAWAY_FROM_JOB`) fails to start rather than
  escaping containment, because the job does not grant breakaway. A backend that genuinely
  needs to outlive its caller must not be run through this runner.
* Linux containment is unchanged: a process group is the platform's own job object, and a
  cgroup remains a possible future refinement, needing root or delegation.
