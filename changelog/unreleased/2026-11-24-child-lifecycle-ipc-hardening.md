- **Timed-out child creation can no longer leave an invisible engine surface.** Child identities
  are reserved by the caller before the build event is sent. If the reply budget expires, an
  ordered close event follows the build and removes a child that completes late; a live host test
  verifies the next request sees it as no longer live on Windows and Linux.
- **Child IPC is bounded and wakes on either source.** Primary and fallback channels now carry at
  most the bridge's 1,024 pending-call budget, pending host queues are bounded, and waits alternate
  between primary IPC and host fallback in short slices. A primary message arriving during a wait
  returns promptly instead of losing the whole timeout, while host-fallback messages still work.
- **Native-menu replacement keeps the previous menu when attachment fails.** Replacement menus
  are fully prepared before attachment. Windows removes the old menu from muda's HWND tracking
  before dropping it, then shows and verifies the replacement, restoring the old menu on failure;
  Linux attaches the new bar before dropping the old one. Live tests prove removed item IDs stop
  routing, new IDs route to the new command, and duplicate-command refusal retains the live menu.
  The fixed `window.child_operations` metric contract is unchanged.
