- **The cross-framework comparison now fails closed on incomplete evidence.** Each repeat must own the measured window, remain alive through idle sampling, expose a complete process tree, and return every requested IPC round; refused bridge calls no longer count as completed work. Eight Windows regression tests cover result validation, aggregate timing consistency, and process-tree sampling. The full Windows run completed five repeats and five 200-round IPC samples for all four configurations on 2026-11-24; aggregate elapsed time is now derived from the median per-round time so the published fields cannot disagree.
- **Idle sampling now waits for the framework's descendant tree.** A native window can become
  visible before WebView2 finishes starting its browser processes. The harness waits for a live
  descendant before beginning the five-second idle sample and still fails if none appears, so a
  transient startup race cannot be published as a complete memory measurement.
- **Repeat cleanup now waits for the captured descendant PIDs to exit.** The next framework is
  not launched while the previous run's browser processes are still shutting down, removing a
  sequence-dependent source of cold-start and IPC failures.
