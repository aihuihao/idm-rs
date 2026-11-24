//! Tauri 命令薄包装：只转发到 DesktopService，不含业务逻辑。

use tauri::State;

use idm_desktop::service::{
    BrowserIntegration, DesktopService, FirefoxExtensionStatus, ServiceError, Settings, TaskDetail,
    TaskView,
};

pub(crate) struct AppState {
    pub(crate) service: std::sync::Arc<tokio::sync::Mutex<Option<DesktopService>>>,
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
