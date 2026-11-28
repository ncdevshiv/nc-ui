- **Embedded child webviews are now proven end to end in pixels, not just in API answers.**
  `crates/ncui-runtime/tests/child_webview_live.rs` drives the real surface an agent uses:
  with a live wry parent window open, an agent session creates a child webview at a
  commanded rectangle, lists it, moves it and closes it — while screen captures of the
  parent's client area prove the child's page appears at exactly the commanded rectangle,
  the vacated rectangle reverts to the parent's page on move, and closing removes the
  child's pixels. The captures are written under `target/f5-evidence/` as inspectable
  artifacts. Getting the capture truthful required two harness facts worth knowing: a
  screen capture reads whatever pixels are on top, so the helper pins the window topmost
  for the capture's duration rather than fighting the foreground lock, and the capture is
  of the client area in physical pixels with the page's own `devicePixelRatio` scaling the
  logical rectangle. Evidence is recorded on `webview.child.embedded` in the registry.
