- Frameless windows can now be resized from their edges: a new `ResizeEdge`
  port type, a `WebViewBackend::drag_resize` seam, `Windows::drag_resize`, and
  the `ncui.window.drag.edge` page method complete the shell-integration
  surface that `ncui.window.drag.region` started. The wiring surfaced a latent
  defect: `WindowHost::drag_window` existed but was never bound into the
  `WebViewBackend` impl, so runtime drag requests always answered false on the
  wry engine — the override is now in place for both drag calls.
