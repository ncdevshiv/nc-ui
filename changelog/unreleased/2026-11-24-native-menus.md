- **Manifest-declared native menus now attach on Windows and Linux.** Applications add a bounded `menus` tree to `ncui.app.json`, receive selections as `menu` events, and get per-window command routing, observability, live-host attachment/routing tests with synthetic
activation, repeatable live Win32 `WM_COMMAND` probes for the counter and Tauri comparison port,
and an ADR-012 exception for muda's single Windows attachment call; the `native-menu` adapter remains feature-gated and a build without it refuses menu declarations before allocating runtime resources.
