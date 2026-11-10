#![cfg_attr(
  all(not(debug_assertions), target_os = "windows"),
  windows_subsystem = "windows"
)]
use tauri::{CustomMenuItem, Menu, MenuItem, Submenu};




// Our Tauri Command
#[tauri::command]
fn return_string(word: String) -> String{
    // debugging message
    println!("The frontend sent us this argument {}", word);
    return word
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

  // Define two sub items for the submenu
  let hello = CustomMenuItem::new("hello".to_string(), "Hello");
  let goodbye = CustomMenuItem::new("goodbye".to_string(), "Goodbye");
  // define the submenu
  let submenu = Submenu::new("Menu", Menu::new().add_item(hello).add_item(goodbye));
  // create main menu object
  let menu = Menu::new()
  // add native copy functionality
  .add_native_item(MenuItem::Copy)
  // add our submenu
  .add_submenu(submenu);


  
  tauri::Builder::default()
    .menu(menu)
    //register menu events
    .on_menu_event(|event| {
      match event.menu_item_id() {
        "hello" => {
          event.window().maximize();
        }
        "goodbye" => {
          event.window().minimize();
        }
        _ => {}
      }
    })
    // Register Command with Tauri App
    .invoke_handler(tauri::generate_handler![return_string, bench_config, bench_rounds, write_bench])
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}
