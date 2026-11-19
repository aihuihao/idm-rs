//! 桌面下载服务层：封装 idm-core 的 Store / DownloadQueue / Runner。
//!
//! 本模块不引用 tauri，可脱离桌面框架单独测试。

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use idm_core::autostart;
use idm_core::disk::AllocationStrategy;
use idm_core::net::{ProxyConfig, Url, parse_max_speed, validate_url};
use idm_core::queue::{DownloadQueue, EnqueueRequest};
use idm_core::runner::{Runner, RunnerConfig, RunnerStartError};
use idm_core::store::{PauseReasonDb, Store, TaskStateDb, default_download_dir};

const DEFAULT_MAX_TASKS: usize = 3;
const DEFAULT_CONNECTIONS: usize = 8;

#[derive(Debug)]
pub enum ServiceError {
    ReadOnly,
    InvalidInput(String),
    Internal(String),
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServiceError::ReadOnly => f.write_str("read-only mode"),
            ServiceError::InvalidInput(msg) => f.write_str(msg),
            ServiceError::Internal(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for ServiceError {}

impl serde::Serialize for ServiceError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

#[derive(serde::Serialize)]
pub struct TaskView {
    pub id: i64,
    pub file_name: Option<String>,
    pub url_display: String,
    pub total_size: Option<u64>,
    pub downloaded: u64,
    pub speed_bps: u64,
    pub state: String,
    pub pause_reason: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    pub download_dir: String,
    pub global_max_speed: u64,
    pub max_tasks: usize,
    pub default_connections: usize,
    pub autostart: bool,
}

pub struct DesktopService {
    store: Arc<Store>,
    runner: Option<Runner>,
    read_only: bool,
    autostart_desktop_name: String,
    autostart_runner_name: String,
}

const DEFAULT_AUTOSTART_DESKTOP_NAME: &str = "idm-rs-desktop";
const DEFAULT_AUTOSTART_RUNNER_NAME: &str = "idm-rs-runner";

fn internal<E: std::fmt::Display>(err: E) -> ServiceError {
    ServiceError::Internal(err.to_string())
}

fn parse_usize_or(raw: Option<String>, default: usize) -> usize {
    raw.and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(default)
}

fn parse_speed_or(raw: Option<String>) -> u64 {
    match raw {
        Some(v) if v.trim() == "0" => 0,
        Some(v) => parse_max_speed(&v).unwrap_or(0),
        None => 0,
    }
}

async fn read_settings(store: &Store, desktop_name: &str) -> Result<Settings, ServiceError> {
    let download = match store.get_setting("download-dir").await.map_err(internal)? {
        Some(v) if !v.is_empty() => v,
        _ => default_download_dir(),
    };
    let global_max_speed = parse_speed_or(
        store
            .get_setting("global-max-speed")
            .await
            .map_err(internal)?,
    );
    let max_tasks = parse_usize_or(
        store.get_setting("max-tasks").await.map_err(internal)?,
        DEFAULT_MAX_TASKS,
    );
    let default_connections = parse_usize_or(
        store
            .get_setting("default-connections")
            .await
            .map_err(internal)?,
        DEFAULT_CONNECTIONS,
    );
    Ok(Settings {
        download_dir: download,
        global_max_speed,
        max_tasks,
        default_connections,
        autostart: autostart::query(desktop_name).is_some(),
    })
}

/// 只保留 scheme://host[:port]/path，去掉 userinfo、query 与 fragment。
fn url_display(raw: &str) -> String {
    let Ok(parsed) = Url::parse(raw) else {
        return String::new();
    };
    let Some(host) = parsed.host_str() else {
        return String::new();
    };
    let mut out = format!("{}://{}", parsed.scheme(), host);
    if let Some(port) = parsed.port() {
        out.push(':');
        out.push_str(&port.to_string());
    }
    out.push_str(parsed.path());
    out
}

fn ensure_dir(path: &Path) -> Result<(), ServiceError> {
    if !path.exists() {
        std::fs::create_dir_all(path).map_err(|e| {
            ServiceError::InvalidInput(format!("cannot create directory {}: {e}", path.display()))
        })?;
    }
    if !path.is_dir() {
        return Err(ServiceError::InvalidInput(format!(
            "not a directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn queue_of(service: &DesktopService) -> Result<&DownloadQueue, ServiceError> {
    service
        .runner
        .as_ref()
        .map(Runner::queue)
        .ok_or(ServiceError::ReadOnly)
}

/// 构造 explorer.exe 的 /select 参数。explorer 对引号解析特殊，参数形如
/// `/select,"<完整路径>"`；路径含双引号时拒绝（避免注入）。纯函数，便于测试。
pub fn reveal_args(path: &Path) -> Result<String, ServiceError> {
    let full = path.to_string_lossy();
    if full.contains('"') {
        return Err(ServiceError::InvalidInput(
            "path must not contain double quotes".to_string(),
        ));
    }
    Ok(format!("/select,\"{full}\""))
}

impl DesktopService {
    /// 打开数据库并尝试成为运行器；拿不到 runner.lock 时进入观察模式（不 recover）。
    /// 使用默认的 autostart 值名（`idm-rs-desktop` / `idm-rs-runner`）。
    pub async fn open(db_path: PathBuf) -> Result<Self, ServiceError> {
        Self::open_with_autostart_names(
            db_path,
            DEFAULT_AUTOSTART_DESKTOP_NAME.to_string(),
            DEFAULT_AUTOSTART_RUNNER_NAME.to_string(),
        )
        .await
    }

    /// 与 open 相同，但允许注入 autostart 值名（供测试使用随机测试名）。
    pub async fn open_with_autostart_names(
        db_path: PathBuf,
        autostart_desktop_name: String,
        autostart_runner_name: String,
    ) -> Result<Self, ServiceError> {
        let store = Store::open(&db_path).map_err(internal)?;
        let settings = read_settings(&store, &autostart_desktop_name).await?;
        let cfg = RunnerConfig {
            db_path,
            max_tasks: settings.max_tasks.clamp(1, 16),
            host_max_connections: settings.default_connections.clamp(1, 32),
            // 桌面服务层无代理配置入口；禁用系统代理以避免本地/受控环境差异。
            proxy: Some(ProxyConfig::Disabled),
        };
        match Runner::start(Arc::clone(&store), cfg).await {
            Ok(runner) => Ok(Self {
                store,
                runner: Some(runner),
                read_only: false,
                autostart_desktop_name,
                autostart_runner_name,
            }),
            Err(RunnerStartError::AlreadyRunning) => Ok(Self {
                store,
                runner: None,
                read_only: true,
                autostart_desktop_name,
                autostart_runner_name,
            }),
            Err(RunnerStartError::Other(err)) => Err(internal(err)),
        }
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub async fn list_tasks(&self) -> Result<Vec<TaskView>, ServiceError> {
        let summaries = self.store.list_tasks().await.map_err(internal)?;
        let mut views = Vec::with_capacity(summaries.len());
        for item in summaries {
            let record = self.store.get_task(item.id).await.map_err(internal)?;
            let speed_bps = match &self.runner {
                Some(runner) => runner
                    .queue()
                    .progress(item.id)
                    .await
                    .map(|p| p.speed.max(0.0) as u64)
                    .unwrap_or(0),
                None => 0,
            };
            views.push(TaskView {
                id: item.id,
                file_name: item.file_name,
                url_display: url_display(&record.url),
                total_size: item.total_size,
                downloaded: item.committed,
                speed_bps,
                state: item.state.as_str().to_string(),
                pause_reason: item
                    .pause_reason
                    .map(PauseReasonDb::as_str)
                    .map(str::to_string),
                last_error: item.last_error,
            });
        }
        // Store 按 id 升序返回；界面要求最新的在最上面，按 id 倒序。
        views.reverse();
        Ok(views)
    }

    pub async fn add_task(
        &self,
        url: String,
        connections: Option<usize>,
    ) -> Result<i64, ServiceError> {
        if self.read_only {
            return Err(ServiceError::ReadOnly);
        }
        if let Err(msg) = validate_url(&url) {
            return Err(ServiceError::InvalidInput(msg));
        }
        let settings = read_settings(&self.store, &self.autostart_desktop_name).await?;
        let connections = connections.unwrap_or(settings.default_connections);
        if !(1..=32).contains(&connections) {
            return Err(ServiceError::InvalidInput(
                "connections must be in 1..=32".to_string(),
            ));
        }
        let dir = PathBuf::from(&settings.download_dir);
        ensure_dir(&dir)?;
        let req = EnqueueRequest {
            url,
            dir,
            connections,
            out_name: None,
            max_speed: None,
            strategy: AllocationStrategy::Auto,
        };
        queue_of(self)?.enqueue(req).await.map_err(internal)
    }

    pub async fn pause_task(&self, id: i64) -> Result<(), ServiceError> {
        if self.read_only {
            return Err(ServiceError::ReadOnly);
        }
        queue_of(self)?.pause(id).await.map_err(internal)
    }

    pub async fn resume_task(&self, id: i64) -> Result<(), ServiceError> {
        if self.read_only {
            return Err(ServiceError::ReadOnly);
        }
        queue_of(self)?
            .resume_queued(id, false)
            .await
            .map_err(internal)
    }

    pub async fn remove_task(&self, id: i64, delete_file: bool) -> Result<(), ServiceError> {
        if self.read_only {
            return Err(ServiceError::ReadOnly);
        }
        let record = self.store.get_task(id).await.map_err(internal)?;
        // 活动/排队中的任务先经队列停下：pause 对活动任务会取消并等待收尾，
        // 对排队任务直接置 Paused(User)。已终态任务 pause 会返回 Err，忽略即可。
        if let Some(queue) = self.runner.as_ref().map(Runner::queue) {
            if matches!(
                record.state,
                TaskStateDb::Queued
                    | TaskStateDb::Downloading
                    | TaskStateDb::Probing
                    | TaskStateDb::Finalizing
            ) {
                let _ = queue.pause(id).await;
            }
            // pause 返回后活动任务已收尾；再轮询确认它确实退出活动表（带超时）。
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while queue.progress(id).await.is_some() && std::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        let part_path = self.store.delete_task(id).await.map_err(internal)?;
        if delete_file {
            if let Some(part) = part_path {
                let path = PathBuf::from(part);
                if path.exists() {
                    std::fs::remove_file(&path).map_err(internal)?;
                }
            }
            if let Some(name) = record.file_name.as_deref().filter(|n| !n.is_empty()) {
                let final_path = Path::new(&record.dir).join(name);
                if final_path.exists() {
                    std::fs::remove_file(&final_path).map_err(internal)?;
                }
            }
        }
        Ok(())
    }

    /// 在资源管理器中选中已完成任务的文件。观察模式（只读）也允许。
    pub async fn reveal_task(&self, id: i64) -> Result<(), ServiceError> {
        let record = self.store.get_task(id).await.map_err(internal)?;
        if record.state != TaskStateDb::Completed {
            return Err(ServiceError::InvalidInput(
                "只有已完成的任务可以打开文件夹".to_string(),
            ));
        }
        let name = record
            .file_name
            .as_deref()
            .filter(|n| !n.is_empty())
            .ok_or_else(|| ServiceError::InvalidInput("文件不存在".to_string()))?;
        let path = Path::new(&record.dir).join(name);
        if !path.exists() {
            return Err(ServiceError::InvalidInput("文件不存在".to_string()));
        }
        let args = reveal_args(&path)?;
        std::process::Command::new("explorer.exe")
            .raw_arg(&args)
            .spawn()
            .map_err(internal)?;
        Ok(())
    }

    /// 删除所有已完成任务的记录（不删除文件），返回条数。观察模式返回 ReadOnly。
    pub async fn clear_completed(&self) -> Result<usize, ServiceError> {
        if self.read_only {
            return Err(ServiceError::ReadOnly);
        }
        let summaries = self.store.list_tasks().await.map_err(internal)?;
        let mut count = 0usize;
        for item in summaries {
            if item.state == TaskStateDb::Completed {
                self.store.delete_task(item.id).await.map_err(internal)?;
                count += 1;
            }
        }
        Ok(count)
    }

    pub async fn get_settings(&self) -> Result<Settings, ServiceError> {
        read_settings(&self.store, &self.autostart_desktop_name).await
    }

    pub async fn set_settings(&self, s: Settings) -> Result<(), ServiceError> {
        if self.read_only {
            return Err(ServiceError::ReadOnly);
        }
        if !(1..=16).contains(&s.max_tasks) {
            return Err(ServiceError::InvalidInput(
                "max_tasks must be in 1..=16".to_string(),
            ));
        }
        if !(1..=32).contains(&s.default_connections) {
            return Err(ServiceError::InvalidInput(
                "default_connections must be in 1..=32".to_string(),
            ));
        }
        ensure_dir(Path::new(&s.download_dir))?;

        self.store
            .set_setting("download-dir", &s.download_dir)
            .await
            .map_err(internal)?;
        self.store
            .set_setting("global-max-speed", &s.global_max_speed.to_string())
            .await
            .map_err(internal)?;
        self.store
            .set_setting("max-tasks", &s.max_tasks.to_string())
            .await
            .map_err(internal)?;
        self.store
            .set_setting("default-connections", &s.default_connections.to_string())
            .await
            .map_err(internal)?;

        let queue = queue_of(self)?;
        let speed = if s.global_max_speed == 0 {
            None
        } else {
            Some(s.global_max_speed)
        };
        queue.set_global_max_speed(speed);
        queue
            .set_max_concurrent_tasks(s.max_tasks)
            .map_err(internal)?;

        // 开机启动：开启时写桌面项并清除旧的 CLI 运行器项，避免两个运行器抢锁；
        // 关闭时只删桌面项，不影响用户可能单独安装的 CLI 运行器。
        if s.autostart {
            let exe = std::env::current_exe().map_err(internal)?;
            let command_line = format!("\"{}\" --minimized", exe.display());
            autostart::install(&self.autostart_desktop_name, &command_line)
                .map_err(ServiceError::Internal)?;
            autostart::uninstall(&self.autostart_runner_name).map_err(ServiceError::Internal)?;
        } else {
            autostart::uninstall(&self.autostart_desktop_name).map_err(ServiceError::Internal)?;
        }
        Ok(())
    }

    /// 等待运行器停止：运行器模式等待 Runner 的停止信号；
    /// 观察模式（无 Runner）永不返回。
    pub async fn wait_stopped(&self) {
        match &self.runner {
            Some(runner) => runner.wait_stopped().await,
            None => std::future::pending().await,
        }
    }

    /// 返回运行器停止信号的接收端副本（运行器模式），供后台任务独立等待。
    /// 观察模式返回 None。
    pub fn stopped_receiver(&self) -> Option<watch::Receiver<bool>> {
        self.runner.as_ref().map(Runner::stopped_receiver)
    }

    /// 安全停止：运行器模式下由 Runner::stop 取消活动任务为 Paused(User) 并释放锁。
    pub async fn shutdown(self) -> Result<(), ServiceError> {
        if let Some(runner) = self.runner {
            runner.stop().await.map_err(internal)?;
        }
        self.store.shutdown().await.map_err(internal)
    }
}
