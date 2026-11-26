### Changed

- The Linux gate now runs the real-window application suite and the real wry page-bridge suite under an explicit X11 environment, rather than reporting a fake runtime bridge as browser evidence.
- Linux GPU integration runs with `NCUI_REQUIRE_GPU=1`; adapter absence now fails the evidence run instead of returning early and being counted as a passing test.
- `tools/record_linux_verification.py` is now a read-only gap audit. It no longer marks every completed feature Linux verified or writes the unsupported `verified_on` field into the registry.
- Linux deep-link unregister now restores the prior `xdg-mime` handler, or removes NC-UI's association when there was none, instead of leaving a default that points at a deleted desktop entry.
- Linux clipboard reads and writes now fall through to the next backend when a tool starts but cannot reach its display, so a Wayland failure no longer hides a working X11 clipboard.
- Linux clipboard writers now detach their capture pipes so daemonized X11/Wayland selection owners survive the call; Wayland tools are skipped when no Wayland display exists.
- The Linux gate requires real clipboard and notification success through the provisioned user-local tools instead of accepting unavailable-platform results. Notification tests run in a temporary D-Bus session with the real dunst service activated on demand.
- The child-environment scrub now preserves `DBUS_SESSION_BUS_ADDRESS`, a local IPC endpoint required by notification tools, while still dropping `SESSION_ID` and other credential-shaped variables.
