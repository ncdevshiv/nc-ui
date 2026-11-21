- **The ADR-018 quit arm no longer breaks non-macOS builds.** `HostEvent::Quit` and its
  `run_engine` match arm landed without the `wry-engine + macOS` gate the `QUIT_CODE` static and
  `run_on_main_thread` already carry, so any Windows or Linux build with the engine enabled failed
  with `cannot find value QUIT_CODE` at the arm. The variant and the arm now carry the same gate;
  `run_on_main_thread` stays the only sender of `Quit`.

Tested: `cargo check -p ncui-webview-wry --features wry-engine --all-targets` passes on this
Windows host; the macOS path is unchanged in shape and remains unrun here (see the seams fragment's
platform note).
