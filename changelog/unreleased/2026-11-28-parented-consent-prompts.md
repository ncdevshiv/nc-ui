- **Destructive page-command approval is now modal to the asking window.** The confirmation
  used to be constructed without any parent window, so on Windows it could surface as an
  orphaned background dialog and leave the page waiting forever (the counter plugin flow
  stalled at "Loading the plugin…"). Backends can now expose a `WindowConfirmation` channel;
  in the wry host the dialog is built on the engine thread with the live `tao` window as its
  `rfd` parent, the runtime prefers it over the runtime-default
  unparented prompt, and a parented failure fails closed instead of falling back. A
  prompter the embedder installs itself is the consent boundary and is always asked —
  the parented surface supersedes only the default native prompt, so test harnesses
  that substitute approval never wedge behind a dialog no one can click. No raw window handle crosses threads and no
  first-party unsafe was added. Evidence: `page_bridge` regression proving exact-operation
  content and prompt routing, plus the live-host smoke proof of the confirmation surface.
