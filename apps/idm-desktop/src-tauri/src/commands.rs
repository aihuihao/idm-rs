//! Tauri 命令薄包装：只转发到 DesktopService / 更新插件，不含业务逻辑。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{Emitter, State};
use tauri_plugin_updater::{Update, UpdaterExt};

use idm_desktop::service::{
    BrowserIntegration, DesktopService, FirefoxExtensionStatus, OBSERVER_RUNNER_MESSAGE,
    ServiceError, Settings, TaskDetail, TaskView,
};
use idm_desktop::update::{
    UpdateCheckView, UpdateProgress, check_error_text, evaluate_update, run_install_flow,
};

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) service: Arc<tokio::sync::Mutex<Option<DesktopService>>>,
    /// 更新安装期间为 true：运行器主动停止信号不应触发应用退出。
    pub(crate) update_in_progress: Arc<AtomicBool>,
}

fn closed() -> ServiceError {
    ServiceError::Internal("service is shutting down".to_string())
}

#[tauri::command]
pub(crate) async fn is_read_only(state: State<'_, AppState>) -> Result<bool, ServiceError> {
    let guard = state.service.lock().await;
    let service = guard.as_ref().ok_or_else(closed)?;
    Ok(service.is_read_only())
}

#[tauri::command]
pub(crate) async fn list_tasks(state: State<'_, AppState>) -> Result<Vec<TaskView>, ServiceError> {
    let guard = state.service.lock().await;
    let service = guard.as_ref().ok_or_else(closed)?;
    service.list_tasks().await
}

#[tauri::command]
pub(crate) async fn add_task(
    url: String,
    connections: Option<usize>,
    state: State<'_, AppState>,
) -> Result<i64, ServiceError> {
    let guard = state.service.lock().await;
    let service = guard.as_ref().ok_or_else(closed)?;
    service.add_task(url, connections).await
}

#[tauri::command]
pub(crate) async fn pause_task(id: i64, state: State<'_, AppState>) -> Result<(), ServiceError> {
    let guard = state.service.lock().await;
    let service = guard.as_ref().ok_or_else(closed)?;
    service.pause_task(id).await
}

#[tauri::command]
pub(crate) async fn resume_task(id: i64, state: State<'_, AppState>) -> Result<(), ServiceError> {
    let guard = state.service.lock().await;
    let service = guard.as_ref().ok_or_else(closed)?;
    service.resume_task(id).await
}

#[tauri::command]
pub(crate) async fn remove_task(
    id: i64,
    delete_file: bool,
    state: State<'_, AppState>,
) -> Result<(), ServiceError> {
    let guard = state.service.lock().await;
    let service = guard.as_ref().ok_or_else(closed)?;
    service.remove_task(id, delete_file).await
}

#[tauri::command]
pub(crate) async fn reveal_task(id: i64, state: State<'_, AppState>) -> Result<(), ServiceError> {
    let guard = state.service.lock().await;
    let service = guard.as_ref().ok_or_else(closed)?;
    service.reveal_task(id).await
}

#[tauri::command]
pub(crate) async fn clear_completed(state: State<'_, AppState>) -> Result<usize, ServiceError> {
    let guard = state.service.lock().await;
    let service = guard.as_ref().ok_or_else(closed)?;
    service.clear_completed().await
}

/// 读取任务分块详情（只读，观察模式允许）。
#[tauri::command]
pub(crate) async fn task_detail(
    id: i64,
    state: State<'_, AppState>,
) -> Result<TaskDetail, ServiceError> {
    let guard = state.service.lock().await;
    let service = guard.as_ref().ok_or_else(closed)?;
    service.task_detail(id).await
}

#[tauri::command]
pub(crate) async fn get_settings(state: State<'_, AppState>) -> Result<Settings, ServiceError> {
    let guard = state.service.lock().await;
    let service = guard.as_ref().ok_or_else(closed)?;
    service.get_settings().await
}

#[tauri::command]
pub(crate) async fn set_settings(
    settings: Settings,
    state: State<'_, AppState>,
) -> Result<(), ServiceError> {
    let guard = state.service.lock().await;
    let service = guard.as_ref().ok_or_else(closed)?;
    service.set_settings(settings).await
}

// --- Phase 5c-2：浏览器集成（观察模式允许） ---
#[tauri::command]
pub(crate) async fn browser_integration_status(
    state: State<'_, AppState>,
) -> Result<BrowserIntegration, ServiceError> {
    let guard = state.service.lock().await;
    let service = guard.as_ref().ok_or_else(closed)?;
    service.browser_integration_status()
}

#[tauri::command]
pub(crate) async fn enable_browser_integration(
    state: State<'_, AppState>,
) -> Result<(), ServiceError> {
    let guard = state.service.lock().await;
    let service = guard.as_ref().ok_or_else(closed)?;
    service.enable_browser_integration()
}

#[tauri::command]
pub(crate) async fn disable_browser_integration(
    state: State<'_, AppState>,
) -> Result<(), ServiceError> {
    let guard = state.service.lock().await;
    let service = guard.as_ref().ok_or_else(closed)?;
    service.disable_browser_integration()
}

/// 查询 Firefox 扩展状态（只读）。
#[tauri::command]
pub(crate) async fn firefox_extension_status(
    state: State<'_, AppState>,
) -> Result<FirefoxExtensionStatus, ServiceError> {
    let guard = state.service.lock().await;
    let service = guard.as_ref().ok_or_else(closed)?;
    Ok(service.firefox_extension_status())
}

/// 一键安装 Firefox 扩展（观察模式允许）。
#[tauri::command]
pub(crate) async fn install_firefox_extension(
    state: State<'_, AppState>,
) -> Result<(), ServiceError> {
    let guard = state.service.lock().await;
    let service = guard.as_ref().ok_or_else(closed)?;
    service.install_firefox_extension()
}

/// 在 Rust 端弹出文件夹选择框，返回选中路径；取消返回空字符串。
#[tauri::command]
pub(crate) async fn pick_download_dir(app: tauri::AppHandle) -> Result<String, ServiceError> {
    use tauri_plugin_dialog::DialogExt;
    // 对话框是阻塞式的，放到专用阻塞线程池，避免长期占用异步工作线程。
    let picked =
        tauri::async_runtime::spawn_blocking(move || app.dialog().file().blocking_pick_folder())
            .await
            .map_err(|err| ServiceError::Internal(err.to_string()))?;
    let path = match picked {
        Some(file_path) => match file_path.into_path() {
            Ok(p) => p.to_string_lossy().into_owned(),
            Err(err) => return Err(ServiceError::Internal(err.to_string())),
        },
        None => String::new(),
    };
    Ok(path)
}

// --- v0.3.0a：自动更新（插件只在 Rust 端调用） ---------------------------

/// 手动与自动检查共用的入口；结果整理成不依赖 Tauri 的纯数据结构。
pub(crate) async fn check_for_update_inner(
    app: &tauri::AppHandle,
) -> Result<UpdateCheckView, ServiceError> {
    let current = env!("CARGO_PKG_VERSION");
    let updater = app
        .updater()
        .map_err(|err| ServiceError::Internal(err.to_string()))?;
    let found = updater
        .check()
        .await
        .map_err(|err| ServiceError::InvalidInput(check_error_text(&err.to_string())))?;
    Ok(match found {
        Some(update) => evaluate_update(current, Some((&update.version, update.body.as_deref()))),
        None => evaluate_update(current, None),
    })
}

#[tauri::command]
pub(crate) async fn check_for_update(
    app: tauri::AppHandle,
) -> Result<UpdateCheckView, ServiceError> {
    check_for_update_inner(&app).await
}

#[tauri::command]
pub(crate) fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

async fn download_and_install(app: &tauri::AppHandle, update: &Update) -> Result<(), ServiceError> {
    let emitter = app.clone();
    update
        .download_and_install(
            move |chunk, total| {
                let _ = emitter.emit(
                    "update-progress",
                    UpdateProgress {
                        downloaded: chunk,
                        total,
                    },
                );
            },
            || {},
        )
        .await
        .map_err(|err| ServiceError::Internal(err.to_string()))
}

async fn install_update_inner(
    app: &tauri::AppHandle,
    shared: &Arc<tokio::sync::Mutex<Option<DesktopService>>>,
) -> Result<(), ServiceError> {
    let updater = app
        .updater()
        .map_err(|err| ServiceError::Internal(err.to_string()))?;
    let update = Arc::new(
        match updater
            .check()
            .await
            .map_err(|err| ServiceError::InvalidInput(check_error_text(&err.to_string())))?
        {
            Some(update) => update,
            None => return Err(ServiceError::InvalidInput("已是最新版本".to_string())),
        },
    );

    let taken = {
        let mut guard = shared.lock().await;
        guard.take().ok_or_else(closed)?
    };
    let session: Arc<tokio::sync::Mutex<Option<DesktopService>>> =
        Arc::new(tokio::sync::Mutex::new(Some(taken)));

    let stop_session = Arc::clone(&session);
    let restore_session = Arc::clone(&session);
    let download_app = app.clone();
    let download_update = Arc::clone(&update);

    let result = run_install_flow(
        move || {
            let session = Arc::clone(&stop_session);
            async move {
                let mut guard = session.lock().await;
                let service = guard.as_mut().ok_or_else(closed)?;
                service.prepare_for_update().await
            }
        },
        move || {
            let app = download_app.clone();
            let update = Arc::clone(&download_update);
            async move { download_and_install(&app, &update).await }
        },
        move || {
            let session = Arc::clone(&restore_session);
            async move {
                let mut guard = session.lock().await;
                let service = guard.as_mut().ok_or_else(closed)?;
                service.restore_after_update().await
            }
        },
    )
    .await;

    // 无论成功或失败，服务都不能永久留在临时 session 里。
    let restored = session.lock().await.take();
    if restored.is_some() {
        let mut guard = shared.lock().await;
        *guard = restored;
    }
    result
}

#[tauri::command]
pub(crate) async fn install_update(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<(), ServiceError> {
    {
        let guard = state.service.lock().await;
        let service = guard.as_ref().ok_or_else(closed)?;
        if !service.can_install_update() {
            return Err(ServiceError::InvalidInput(
                OBSERVER_RUNNER_MESSAGE.to_string(),
            ));
        }
    }
    if state.update_in_progress.swap(true, Ordering::SeqCst) {
        return Err(ServiceError::InvalidInput(
            "正在安装更新，请稍候".to_string(),
        ));
    }
    let result = install_update_inner(&app, &state.service).await;
    state.update_in_progress.store(false, Ordering::SeqCst);
    result
}
