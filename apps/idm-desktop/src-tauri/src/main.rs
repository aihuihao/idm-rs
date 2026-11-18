#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;

use std::path::PathBuf;
use std::time::Duration;

use tauri::{Emitter, Manager, RunEvent};

use idm_desktop::service::DesktopService;
use idm_desktop::throttle::Throttle;

use commands::AppState;

fn main() {
    if let Err(err) = run() {
        eprintln!("idm-desktop failed to start: {err}");
        std::process::exit(1);
    }
}

fn resolve_db_path() -> Result<PathBuf, String> {
    match std::env::var_os("IDM_RS_DB") {
        Some(v) if !v.is_empty() => Ok(PathBuf::from(v)),
        _ => idm_core::store::default_db_path().map_err(|e| e.to_string()),
    }
}

fn run() -> Result<(), String> {
    let db_path = resolve_db_path()?;
    let service =
        tauri::async_runtime::block_on(DesktopService::open(db_path)).map_err(|e| e.to_string())?;
    let state = AppState {
        service: tokio::sync::Mutex::new(Some(service)),
    };

    let app = tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            commands::is_read_only,
            commands::list_tasks,
            commands::add_task,
            commands::pause_task,
            commands::resume_task,
            commands::remove_task,
            commands::get_settings,
            commands::set_settings,
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut throttle = Throttle::new(Duration::from_millis(250));
                loop {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    let shared = handle.state::<AppState>();
                    let guard = shared.service.lock().await;
                    let Some(service) = guard.as_ref() else {
                        break;
                    };
                    let tasks = match service.list_tasks().await {
                        Ok(t) => t,
                        Err(_) => continue,
                    };
                    drop(guard);
                    let snapshot = match serde_json::to_vec(&tasks) {
                        Ok(bytes) => bytes,
                        Err(_) => continue,
                    };
                    if throttle.should_emit(std::time::Instant::now(), &snapshot) {
                        let payload = String::from_utf8_lossy(&snapshot).into_owned();
                        let _ = handle.emit("tasks-updated", payload);
                    }
                }
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .map_err(|e| e.to_string())?;

    app.run(|app_handle, event| {
        if let RunEvent::Exit = event {
            let shared = app_handle.state::<AppState>();
            tauri::async_runtime::block_on(async {
                let mut guard = shared.service.lock().await;
                if let Some(service) = guard.take() {
                    let _ = service.shutdown().await;
                }
            });
        }
    });
    Ok(())
}
