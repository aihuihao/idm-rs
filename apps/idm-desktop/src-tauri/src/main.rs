#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;

use std::io::{Error as IoError, ErrorKind as IoErrorKind};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager, RunEvent};

use idm_desktop::service::{DesktopService, TaskView, finished_since};
use idm_desktop::throttle::Throttle;
use tauri_plugin_notification::NotificationExt;

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

/// Phase 5c-2：把启动自动处理的结果追加到数据目录下的日志；失败时静默忽略。
/// 不新增依赖（桌面 crate 无 tracing），只写文件，绝不写标准输出/注册表。
fn startup_auto_log(db_path: &std::path::Path, msg: &str) {
    let dir = match db_path.parent() {
        Some(d) => d,
        None => return,
    };
    let path = dir.join("desktop-startup.log");
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{msg}");
    }
}

/// 关闭 service 并退出：由托盘「退出」与运行器停止任务共用。
async fn shutdown_and_exit(handle: tauri::AppHandle) {
    let service = handle.state::<AppState>().service.clone();
    let mut guard = service.lock().await;
    if let Some(svc) = guard.take() {
        let _ = svc.shutdown().await;
    }
    drop(guard);
    handle.exit(0);
}

fn run() -> Result<(), String> {
    let db_path = resolve_db_path()?;
    let log_db_path = db_path.clone();
    let service =
        tauri::async_runtime::block_on(DesktopService::open(db_path)).map_err(|e| e.to_string())?;
    let stopped_rx = service.stopped_receiver();
    let state = AppState {
        service: Arc::new(tokio::sync::Mutex::new(Some(service))),
    };

    let app = tauri::Builder::default()
        // single-instance 必须是第一个注册的插件。
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            // 第二次启动若带 --minimized，已运行实例不做任何事。
            if args.iter().any(|arg| arg == "--minimized") {
                return;
            }
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            commands::is_read_only,
            commands::list_tasks,
            commands::add_task,
            commands::pause_task,
            commands::resume_task,
            commands::remove_task,
            commands::reveal_task,
            commands::clear_completed,
            commands::get_settings,
            commands::set_settings,
            commands::pick_download_dir,
            commands::task_detail,
            commands::browser_integration_status,
            commands::enable_browser_integration,
            commands::disable_browser_integration,
        ])
        .setup(move |app| {
            // --- 托盘：菜单 + 图标 ---
            let show_item = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_item, &quit_item])?;
            let icon = app.default_window_icon().cloned().ok_or_else(|| {
                IoError::new(IoErrorKind::NotFound, "default window icon missing")
            })?;
            let tray = TrayIconBuilder::new()
                .icon(icon)
                .menu(&menu)
                .tooltip("idm-rs")
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    "quit" => {
                        let handle = app.clone();
                        tauri::async_runtime::spawn(async move {
                            shutdown_and_exit(handle).await;
                        });
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Down,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(window) = app.get_webview_window("main") {
                            match window.is_visible() {
                                Ok(true) => {
                                    let _ = window.hide();
                                }
                                Ok(false) => {
                                    let _ = window.show();
                                    let _ = window.set_focus();
                                }
                                Err(_) => {}
                            }
                        }
                    }
                })
                .build(app)?;
            app.manage(tray);

            // --minimized：开机自启时不显示主窗口，只保留托盘图标。
            if std::env::args().any(|arg| arg == "--minimized")
                && let Some(window) = app.get_webview_window("main")
            {
                let _ = window.hide();
            }

            // --- Phase 5c-2：启动时自动处理（失败只记日志，不阻止启动） ---
            {
                let handle = app.handle().clone();
                let lp = log_db_path.clone();
                tauri::async_runtime::spawn(async move {
                    let shared = handle.state::<AppState>();
                    let guard = shared.service.lock().await;
                    let Some(svc) = guard.as_ref() else { return };
                    match svc.auto_fix_browser_integration() {
                        Ok(Some(msg)) => startup_auto_log(&lp, &msg),
                        Ok(None) => {}
                        Err(err) => startup_auto_log(&lp, &format!("auto-fix check failed: {err}")),
                    }
                    match svc.refresh_autostart_path() {
                        Ok(Some(msg)) => startup_auto_log(&lp, &msg),
                        Ok(None) => {}
                        Err(err) => {
                            startup_auto_log(&lp, &format!("autostart path refresh failed: {err}"))
                        }
                    }
                });
            }

            // --- 后台每 250ms 推送任务列表 ---
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut throttle = Throttle::new(Duration::from_millis(250));
                // 首次快照只建立基线、不发通知，避免把历史任务全报一遍。
                let mut prev: Option<Vec<TaskView>> = None;
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
                    let read_only = service.is_read_only();
                    drop(guard);

                    // 通知：仅非首次、非观察模式、设置开启时发送。
                    if let Some(previous) = prev.as_ref() {
                        let finished = finished_since(previous, &tasks);
                        if !finished.is_empty() && !read_only {
                            let guard = shared.service.lock().await;
                            let notify = match guard.as_ref() {
                                Some(svc) => match svc.get_settings().await {
                                    Ok(settings) => settings.notify_on_complete,
                                    Err(_) => false,
                                },
                                None => false,
                            };
                            drop(guard);
                            if notify {
                                for (_, name, ok) in finished {
                                    let title = if ok { "下载完成" } else { "下载失败" };
                                    let _ = handle
                                        .notification()
                                        .builder()
                                        .title(title)
                                        .body(name)
                                        .show();
                                }
                            }
                        }
                    }
                    prev = Some(tasks.clone());

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

            // --- 运行器停止 → shutdown + 退出（仅运行器模式） ---
            if let Some(mut rx) = stopped_rx {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    while !*rx.borrow() {
                        if rx.changed().await.is_err() {
                            break;
                        }
                    }
                    shutdown_and_exit(handle).await;
                });
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // 关闭按钮只隐藏窗口，不退出程序（退出走托盘菜单）。
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
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
