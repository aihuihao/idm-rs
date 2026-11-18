#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Err(err) = run() {
        eprintln!("idm-desktop failed to start: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), tauri::Error> {
    tauri::Builder::default().run(tauri::generate_context!())
}
