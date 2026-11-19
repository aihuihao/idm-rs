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
use idm_core::task::ChunkState;

const DEFAULT_MAX_TASKS: usize = 3;
const DEFAULT_CONNECTIONS: usize = 8;
/// 每主机连接上限默认 8（与 idm-core 的 DEFAULT_MAX_PER_HOST 一致）。重启生效：设置在 open 时读取。
const DEFAULT_MAX_PER_HOST: usize = 8;

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

#[derive(Clone, serde::Serialize)]
pub struct TaskView {
    pub id: i64,
    pub kind: String,
    pub file_name: Option<String>,
    pub url_display: String,
    pub total_size: Option<u64>,
    pub downloaded: u64,
    pub speed_bps: u64,
    pub state: String,
    pub pause_reason: Option<String>,
    pub last_error: Option<String>,
}

/// 单个下载分块的展示视图（不含请求头/Cookie/完整 URL）。
#[derive(serde::Serialize, Clone)]
pub struct ChunkView {
    pub index: usize,
    pub start: u64,
    /// 结束位置（包含）。
    pub end: u64,
    pub downloaded: u64,
    /// 拿不到实时速度时为 0。
    pub speed_bps: u64,
    /// "active" | "done" | "waiting" | "queued" | "retrying"
    pub state: String,
}

/// 任务详情：分块列表与整体信息。
#[derive(serde::Serialize, Clone)]
pub struct TaskDetail {
    pub id: i64,
    pub total_size: Option<u64>,
    pub connections: usize,
    pub chunks: Vec<ChunkView>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    pub download_dir: String,
    pub global_max_speed: u64,
    pub max_tasks: usize,
    pub default_connections: usize,
    pub max_connections_per_host: usize,
    pub autostart: bool,
    pub notify_on_complete: bool,
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
    let max_connections_per_host = parse_usize_or(
        store
            .get_setting("max-connections-per-host")
            .await
            .map_err(internal)?,
        DEFAULT_MAX_PER_HOST,
    )
    .clamp(1, 32);
    let notify_on_complete = match store
        .get_setting("notify-on-complete")
        .await
        .map_err(internal)?
    {
        Some(v) => v.trim() != "false",
        None => true,
    };
    Ok(Settings {
        download_dir: download,
        global_max_speed,
        max_tasks,
        default_connections,
        max_connections_per_host,
        autostart: autostart::query(desktop_name).is_some(),
        notify_on_complete,
    })
}

/// 对比前后两帧任务列表，找出期间新进入终态的任务。
///
/// 返回 `(任务 id, 显示名称, 是否完成)`：`true` 表示完成，`false` 表示失败。
/// 仅统计 prev 中存在、且当时不是 Completed/Failed，而在 now 中变为终态的任务；
/// prev 中不存在的新任务不计入。
pub fn finished_since(prev: &[TaskView], now: &[TaskView]) -> Vec<(i64, String, bool)> {
    let finished = |state: &str| match state {
        "Completed" => Some(true),
        "Failed" => Some(false),
        _ => None,
    };
    let mut out = Vec::new();
    for item in now {
        let Some(is_completed) = finished(&item.state) else {
            continue;
        };
        let Some(before) = prev.iter().find(|p| p.id == item.id) else {
            continue;
        };
        if finished(&before.state).is_some() {
            continue;
        }
        let name = item
            .file_name
            .clone()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| item.url_display.clone());
        out.push((item.id, name, is_completed));
    }
    out
}

/// 按文件名（或回退的 URL）扩展名判断任务类型，供左侧分类栏使用。
///
/// 不区分大小写；复合扩展名 `.tar.gz` 等归为 archive。
pub fn classify(name: &str) -> &'static str {
    const GROUPS: &[(&str, &[&str])] = &[
        (
            "archive",
            &["zip", "rar", "7z", "tar", "gz", "xz", "bz2", "zst", "iso"],
        ),
        ("video", &["mp4", "mkv", "avi", "mov", "wmv", "flv", "webm"]),
        ("audio", &["mp3", "flac", "wav", "aac", "ogg", "m4a"]),
        (
            "document",
            &[
                "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "txt", "epub",
            ],
        ),
        ("program", &["exe", "msi", "apk", "dmg", "deb", "rpm"]),
    ];
    let lower = name.to_ascii_lowercase();
    for ext in [".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst"] {
        if lower.ends_with(ext) {
            return "archive";
        }
    }
    let ext = match lower.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() => ext,
        _ => return "other",
    };
    for (kind, exts) in GROUPS {
        if exts.contains(&ext) {
            return kind;
        }
    }
    "other"
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
            // 重启生效：每主机连接上限由独立的 max-connections-per-host 决定，
            // 不再误用「每任务默认连接数」default_connections。
            host_max_connections: settings.max_connections_per_host.clamp(1, 32),
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
            let kind_name = item
                .file_name
                .clone()
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| url_display(&record.url));
            let kind = classify(&kind_name).to_string();
            views.push(TaskView {
                id: item.id,
                kind,
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

    /// 读取任务的分块详情（只读，观察模式也允许）。
    ///
    /// 活动任务优先取队列的实时分块快照；非活动/观察模式回退到数据库分块记录。
    /// 分块进度区间在内部为半开 `[start, end)`，对外 `end` 转为包含式。
    pub async fn task_detail(&self, id: i64) -> Result<TaskDetail, ServiceError> {
        let record = self.store.get_task(id).await.map_err(internal)?;
        let connections = record.connections.max(0) as usize;

        let live = match &self.runner {
            Some(runner) => runner.queue().progress(id).await,
            None => None,
        };

        let chunks: Vec<ChunkView> = if let Some(snapshot) = live {
            snapshot
                .chunks
                .iter()
                .map(|c| {
                    // Phase 5b-4b：排队等待 Host 许可的分块（尚未发出请求）单列
                    // "queued"，与真正"正在下载"区分；它不改变底层 ChunkState。
                    let state = if c.waiting_for_permit {
                        "queued"
                    } else {
                        match c.state {
                            ChunkState::Downloading => "active",
                            ChunkState::Done => "done",
                            ChunkState::Failed | ChunkState::Cancelled => "retrying",
                            ChunkState::Pending => "waiting",
                        }
                    };
                    ChunkView {
                        index: c.db_index.max(0) as usize,
                        start: c.start,
                        end: c.end.saturating_sub(1),
                        downloaded: c.current.saturating_sub(c.start),
                        // 分块实时速度未发布到公开快照，按约定返回 0。
                        speed_bps: 0,
                        state: state.to_string(),
                    }
                })
                .collect()
        } else {
            let all_done = record.state == TaskStateDb::Completed;
            record
                .chunks
                .iter()
                .map(|c| {
                    let state = if all_done || c.committed >= c.end {
                        "done"
                    } else if c.committed > c.start {
                        "active"
                    } else {
                        "waiting"
                    };
                    ChunkView {
                        index: c.chunk_index.max(0) as usize,
                        start: c.start,
                        end: c.end.saturating_sub(1),
                        downloaded: c.committed.saturating_sub(c.start),
                        speed_bps: 0,
                        state: state.to_string(),
                    }
                })
                .collect()
        };

        Ok(TaskDetail {
            id,
            total_size: record.total_size,
            connections,
            chunks,
        })
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
        if !(1..=32).contains(&s.max_connections_per_host) {
            return Err(ServiceError::InvalidInput(
                "max_connections_per_host must be in 1..=32".to_string(),
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
        // 每主机连接上限「重启生效」：只保存，不尝试在运行中修改 HostConnectionLimiter。
        self.store
            .set_setting(
                "max-connections-per-host",
                &s.max_connections_per_host.to_string(),
            )
            .await
            .map_err(internal)?;
        self.store
            .set_setting("notify-on-complete", &s.notify_on_complete.to_string())
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
