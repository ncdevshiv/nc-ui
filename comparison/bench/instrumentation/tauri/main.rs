#![cfg_attr(
  all(not(debug_assertions), target_os = "windows"),
  windows_subsystem = "windows"
)]
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::Manager;

// Our Tauri Command
#[tauri::command]
fn return_string(word: String) -> String {
    // debugging message
    println!("The frontend sent us this argument {}", word);
    word
}

// The comparison harness sets NCUI_BENCH_OUTPUT. These two commands expose the path to the
// frontend and write the measurement back, so the page can run IPC round trips and report.
#[tauri::command]
fn bench_config() -> Option<String> {
    std::env::var("NCUI_BENCH_OUTPUT").ok()
}

#[tauri::command]
fn bench_rounds() -> Option<String> {
    std::env::var("NCUI_BENCH_ROUNDS").ok()
}

#[tauri::command]
fn write_bench(json: String) -> Result<(), String> {
    let path = std::env::var("NCUI_BENCH_OUTPUT").map_err(|error| error.to_string())?;
    std::fs::write(path, json).map_err(|error| error.to_string())
}

fn main() {
  tauri::Builder::default()
    .menu(|app| {
      // Tauri 2 menu API: items carry an id and are built against the app handle, and native
      // items are PredefinedMenuItem constructors rather than MenuItem variants.
      let hello = MenuItem::with_id(app, "hello", "Hello", true, None::<&str>)?;
      let goodbye = MenuItem::with_id(app, "goodbye", "Goodbye", true, None::<&str>)?;
      let submenu = Submenu::with_items(app, "Menu", true, &[&hello, &goodbye])?;
      let copy = PredefinedMenuItem::copy(app, None)?;
      Ok(Menu::with_items(app, &[&copy, &submenu])?)
    })
    .on_menu_event(|app, event| {
      // Tauri 2: the event carries an id resolved against the app, and the window is looked
      // up from the app handle rather than reached through the event.
      match event.id().as_ref() {
        "hello" => {
          if let Some(window) = app.get_webview_window("main") {
            let _ = window.maximize();
          }
        }
        "goodbye" => {
          if let Some(window) = app.get_webview_window("main") {
            let _ = window.minimize();
          }
        }
        _ => {}
      }
    })
    // Register Command with Tauri App
    .invoke_handler(tauri::generate_handler![return_string, bench_config, bench_rounds, write_bench])
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}
