//! Tauri 命令薄包装：只转发到 DesktopService，不含业务逻辑。

use tauri::State;

use idm_desktop::service::{DesktopService, ServiceError, Settings, TaskView};

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

/// 在 Rust 端弹出文件夹选择框，返回选中路径；取消返回空字符串。
#[tauri::command]
pub(crate) async fn pick_download_dir(app: tauri::AppHandle) -> Result<String, ServiceError> {
    use tauri_plugin_dialog::DialogExt;
    let picked = app.dialog().file().blocking_pick_folder();
    let path = match picked {
        Some(file_path) => match file_path.into_path() {
            Ok(p) => p.to_string_lossy().into_owned(),
            Err(err) => return Err(ServiceError::Internal(err.to_string())),
        },
        None => String::new(),
    };
    Ok(path)
}
