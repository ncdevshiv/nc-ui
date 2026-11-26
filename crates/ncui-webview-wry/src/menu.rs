//! The `muda` binding for manifest-declared native menus.

use std::collections::{HashMap, HashSet};

use ncui_webview::{MenuEntry, MenuRequest, MenuSubmenu};
use tao::window::Window;

use muda::{
    IsMenuItem, Menu as NativeMenu, MenuId, MenuItem as NativeMenuItem, PredefinedMenuItem,
    Submenu as NativeSubmenu,
};

#[derive(Debug, Default)]
struct CommandIndex {
    by_id: HashMap<String, String>,
    names: HashSet<String>,
}

impl CommandIndex {
    fn insert(&mut self, id: String, command: &str) -> Result<(), String> {
        if !self.names.insert(command.to_owned()) {
            return Err(format!("duplicate native menu command `{command}`"));
        }
        self.by_id.insert(id, command.to_owned());
        Ok(())
    }

    fn command(&self, id: &str) -> Option<&str> {
        self.by_id.get(id).map(String::as_str)
    }
}

pub(super) struct NativeMenuState {
    menu: NativeMenu,
    commands: CommandIndex,
}

impl NativeMenuState {
    pub(super) fn command(&self, id: &str) -> Option<&str> {
        self.commands.command(id)
    }

    pub(super) fn attach(
        self,
        window: &Window,
        previous: Option<&NativeMenu>,
    ) -> Result<Self, String> {
        attach(window, &self.menu, previous)?;
        Ok(self)
    }
}

/// Attaches or removes one window's native menu on the engine thread.
pub(super) fn set_window_menu(
    windows: &mut HashMap<u64, crate::host::EngineWindow>,
    window: u64,
    request: Option<MenuRequest>,
) -> Result<(), String> {
    let entry = windows
        .get_mut(&window)
        .ok_or_else(|| format!("window {window} is not open"))?;
    let replacement = if let Some(request) = request {
        Some(prepare(window, &request)?)
    } else {
        None
    };
    let window_handle = &entry.window;
    if let Some(menu) = replacement {
        let previous = entry.menu.as_ref().map(|state| &state.menu);
        entry.menu = Some(menu.attach(window_handle, previous)?);
    } else {
        entry.menu = None;
    }
    Ok(())
}

/// Routes a native selection only to the window whose live menu owns its item identity.
pub(super) fn menu_was_activated(windows: &HashMap<u64, crate::host::EngineWindow>, id: &str) {
    let Some((entry, command)) = windows.iter().find_map(|(_, entry)| {
        entry
            .menu
            .as_ref()?
            .command(id)
            .map(|command| (entry, command))
    }) else {
        return;
    };
    let _ = entry.menu_tx.send(ncui_webview::MenuEvent {
        command: command.to_owned(),
    });
}

pub(super) fn prepare(window_id: u64, request: &MenuRequest) -> Result<NativeMenuState, String> {
    let menu = NativeMenu::with_id(format!("ncui.menu.root.{window_id}"));
    let mut commands = CommandIndex::default();
    for (index, submenu) in request.menus.iter().enumerate() {
        append_submenu(
            &menu,
            window_id,
            &format!("root.{index}"),
            submenu,
            &mut commands,
        )?;
    }
    Ok(NativeMenuState { menu, commands })
}

trait MenuContainer {
    fn append_native(&self, item: &dyn IsMenuItem) -> Result<(), muda::Error>;
}

impl MenuContainer for NativeMenu {
    fn append_native(&self, item: &dyn IsMenuItem) -> Result<(), muda::Error> {
        self.append(item)
    }
}

impl MenuContainer for NativeSubmenu {
    fn append_native(&self, item: &dyn IsMenuItem) -> Result<(), muda::Error> {
        self.append(item)
    }
}

fn append_submenu(
    container: &impl MenuContainer,
    window_id: u64,
    path: &str,
    submenu: &MenuSubmenu,
    commands: &mut CommandIndex,
) -> Result<(), String> {
    let id = format!("ncui.menu.submenu.{window_id}.{path}");
    let native = NativeSubmenu::with_id(id, &submenu.label, submenu.enabled);
    append_entries(&native, window_id, path, &submenu.items, commands)?;
    container
        .append_native(&native)
        .map_err(|error| format!("the native submenu could not be added: {error}"))
}

fn append_entries(
    container: &impl MenuContainer,
    window_id: u64,
    path: &str,
    entries: &[MenuEntry],
    commands: &mut CommandIndex,
) -> Result<(), String> {
    for (index, entry) in entries.iter().enumerate() {
        let path = format!("{path}.{index}");
        match entry {
            MenuEntry::Item {
                label,
                command,
                enabled,
            } => {
                let id = format!("ncui.menu.item.{window_id}.{path}");
                commands.insert(id.clone(), command)?;
                let item = NativeMenuItem::with_id(MenuId::new(id), label, *enabled, None);
                container
                    .append_native(&item)
                    .map_err(|error| format!("the native menu item could not be added: {error}"))?;
            }
            MenuEntry::Submenu(submenu) => {
                append_submenu(container, window_id, &path, submenu, commands)?;
            }
            MenuEntry::Separator => {
                let separator = PredefinedMenuItem::separator();
                container.append_native(&separator).map_err(|error| {
                    format!("the native menu separator could not be added: {error}")
                })?;
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
#[allow(
    unsafe_code,
    reason = "muda's HWND attachment, replacement and visibility checks are the sole first-party unsafe exception"
)]
fn attach(window: &Window, menu: &NativeMenu, previous: Option<&NativeMenu>) -> Result<(), String> {
    use tao::platform::windows::WindowExtWindows as _;

    // SAFETY: the HWND comes from the live tao window and every menu is owned by the engine
    // thread. A previous menu is removed from muda's HWND tracking before it drops, so its drop
    // hook cannot detach the replacement. EngineWindow still drops its menu before its window.
    let hwnd = window.hwnd();
    unsafe {
        menu.init_for_hwnd(hwnd).map_err(|error| {
            format!("the native menu could not be attached to the window: {error}")
        })?;
        let mut previous_removed = false;
        if let Some(previous) = previous {
            if let Err(error) = previous.remove_for_hwnd(hwnd) {
                let _ = menu.remove_for_hwnd(hwnd);
                let _ = previous.show_for_hwnd(hwnd);
                return Err(format!(
                    "the previous native menu could not be replaced: {error}"
                ));
            }
            previous_removed = true;
            if let Err(error) = menu.show_for_hwnd(hwnd) {
                let _ = menu.remove_for_hwnd(hwnd);
                let _ = previous.init_for_hwnd(hwnd);
                return Err(format!(
                    "the replacement native menu could not be shown: {error}"
                ));
            }
        }
        if !menu.is_visible_on_hwnd(hwnd) {
            let _ = menu.remove_for_hwnd(hwnd);
            if let Some(previous) = previous {
                if previous_removed {
                    let _ = previous.init_for_hwnd(hwnd);
                } else {
                    let _ = previous.show_for_hwnd(hwnd);
                }
            }
            return Err("the window did not retain the native menu after attachment".to_owned());
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn attach(window: &Window, menu: &NativeMenu, previous: Option<&NativeMenu>) -> Result<(), String> {
    use tao::platform::unix::WindowExtUnix as _;

    let _ = previous;

    let gtk_window = window.gtk_window();
    let container = window
        .default_vbox()
        .ok_or_else(|| "the window has no visual box for a native menu".to_owned())?;
    menu.init_for_gtk_window(gtk_window, Some(container))
        .map_err(|error| format!("the native menu could not be attached to the window: {error}"))
}

#[cfg(target_os = "macos")]
fn attach(window: &Window, menu: &NativeMenu, previous: Option<&NativeMenu>) -> Result<(), String> {
    let _ = (window, previous);
    menu.init_for_nsapp();
    Ok(())
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
fn attach(
    _window: &Window,
    _menu: &NativeMenu,
    _previous: Option<&NativeMenu>,
) -> Result<(), String> {
    Err("this platform has no native menu attachment".to_owned())
}

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_ids_keep_identical_commands_scoped_to_their_window() {
        let mut first = CommandIndex::default();
        first
            .insert("ncui.menu.item.7.root.0.0".to_owned(), "open")
            .expect("the first window claims the command");
        let mut second = CommandIndex::default();
        second
            .insert("ncui.menu.item.8.root.0.0".to_owned(), "open")
            .expect("another window may expose the same command");

        assert_eq!(first.command("ncui.menu.item.7.root.0.0"), Some("open"));
        assert_eq!(first.command("ncui.menu.item.8.root.0.0"), None);
        assert_eq!(second.command("ncui.menu.item.8.root.0.0"), Some("open"));
        assert_eq!(second.command("ncui.menu.item.9.root.0.0"), None);
    }

    #[test]
    fn one_menu_cannot_give_two_native_items_the_same_command() {
        let mut index = CommandIndex::default();
        index
            .insert("ncui.menu.item.7.root.0.0".to_owned(), "open")
            .expect("the first item claims the command");
        let error = index
            .insert("ncui.menu.item.7.root.1.0".to_owned(), "open")
            .expect_err("a second item cannot claim the same command");
        assert!(
            error.contains("duplicate native menu command `open`"),
            "{error}"
        );
    }
}
