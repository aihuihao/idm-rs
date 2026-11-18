//! 独立常驻队列运行器。
//!
//! 本模块承载 `idm-cli queue run --watch` 的运行器逻辑：抢 `runner.lock`、启动恢复、
//! 调度 `Queued` 任务、写 `runner.log`、每 5 秒重读 `global-max-speed`，以及通过
//! `runner.stop` 哨兵文件安全停止。
//!
//! 本模块**不处理 Ctrl+C**：信号监听留在 CLI，调用方用 `wait_stopped()` 或自己的
//! Ctrl+C 监听，二选一先到后调用 `stop()` 收尾。
//!
//! 行为不变量：`stop()` 返回后，`runner.lock` 已释放，且本模块启动的所有 tokio
//! 任务均已结束（不会因遗留的 `DownloadQueue` clone / 信号监听任务阻塞退出）。

use std::io::Write;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::watch;

use crate::disk::{AllocationStrategy, DiskWriterPool};
use crate::error::Error;
use crate::net::client::{DownloadConfig, ProxyConfig};
use crate::queue::{DownloadQueue, QueueConfig, QueueEvent};
use crate::store::Store;

/// 运行器配置。字段与对外接口约定一致。
pub struct RunnerConfig {
    pub db_path: PathBuf,
    pub max_tasks: usize,
    pub host_max_connections: usize,
    /// `None` 表示使用系统代理（对应 CLI 未显式指定 --proxy/--no-proxy 的默认值）。
    pub proxy: Option<ProxyConfig>,
}

/// 启动运行器的失败原因。
#[derive(Debug)]
pub enum RunnerStartError {
    /// `runner.lock` 被占用（另一个运行器正在运行）。
    AlreadyRunning,
    /// 其他错误（磁盘池初始化 / 启动恢复 / adopt 失败）。
    Other(Error),
}

impl std::fmt::Display for RunnerStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunnerStartError::AlreadyRunning => f.write_str("already running"),
            RunnerStartError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RunnerStartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RunnerStartError::AlreadyRunning => None,
            RunnerStartError::Other(e) => Some(e),
        }
    }
}

/// 常驻运行器句柄。持有 `runner.lock` 与后台任务；调用 `stop()` 安全收尾。
pub struct Runner {
    queue: DownloadQueue,
    lock: std::fs::File,
    stop_flag: Arc<AtomicBool>,
    stop_path: PathBuf,
    log_path: PathBuf,
    stopped_tx: watch::Sender<bool>,
    stopped_rx: watch::Receiver<bool>,
    watch_task: Option<tokio::task::JoinHandle<()>>,
    log_task: Option<tokio::task::JoinHandle<()>>,
}

/// 读取 `global-max-speed` 设置并解析为字节率；"0"/未设置/非法均视为不限速。
async fn configured_max_speed(store: &Store) -> Option<u64> {
    match store.get_setting("global-max-speed").await {
        Ok(Some(v)) if v.trim() == "0" => None,
        Ok(Some(v)) => crate::net::parse_max_speed(&v),
        _ => None,
    }
}

/// runner.log 容量上限：1 MiB，超过则覆盖重写。
const RUNNER_LOG_CAP: u64 = 1_048_576;

/// 追加一行运行日志到 `runner.log`（info 级别，单文件 1 MiB 覆盖重写）。
/// 只记录运行器生命周期事件与任务 id，不写 URL、查询参数或任何请求头值。
fn runner_log(path: &Path, msg: &str) {
    let mut file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        Ok(f) => f,
        Err(_) => return,
    };
    if file.metadata().map(|m| m.len()).unwrap_or(0) > RUNNER_LOG_CAP
        && let Ok(f) = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
    {
        file = f;
    }
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = writeln!(file, "[{secs}] INFO {msg}");
}

impl Runner {
    /// 获取 `runner.lock`（独占，200ms 重试、共 3s）并启动下载引擎。
    pub async fn start(store: Arc<Store>, cfg: RunnerConfig) -> Result<Runner, RunnerStartError> {
        let lock_dir = cfg.db_path.parent().unwrap_or_else(|| Path::new("."));
        let lock_path = lock_dir.join("runner.lock");
        let mut lock_opt = std::fs::OpenOptions::new();
        lock_opt.read(true).write(true).create(true).truncate(false);
        OpenOptionsExt::share_mode(&mut lock_opt, 0);
        let deadline = Instant::now() + Duration::from_secs(3);
        let lock = loop {
            match lock_opt.open(&lock_path) {
                Ok(f) => break f,
                Err(_) => {
                    if Instant::now() >= deadline {
                        return Err(RunnerStartError::AlreadyRunning);
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        };

        // 启动恢复（9.4）：处理遗留的 Downloading / Finalizing 任务。
        store.recover().await.map_err(RunnerStartError::Other)?;

        let proxy = cfg.proxy.unwrap_or(ProxyConfig::System);
        let config = QueueConfig {
            max_concurrent_tasks: cfg.max_tasks,
            global_max_speed: None,
            default_download_config: DownloadConfig {
                max_connections_per_host: cfg.host_max_connections,
                proxy,
                ..DownloadConfig::default()
            },
            default_strategy: AllocationStrategy::Auto,
        };
        let pool = Arc::new(DiskWriterPool::new(8, 64).map_err(RunnerStartError::Other)?);
        let queue = DownloadQueue::new(store.clone(), pool, config);
        queue
            .adopt_queued()
            .await
            .map_err(RunnerStartError::Other)?;

        let log_path = lock_dir.join("runner.log");
        let stop_path = lock_dir.join("runner.stop");
        runner_log(&log_path, "runner started");

        // 订阅队列事件，记录任务开始/结束（只写 id，不写 URL/请求头）。
        let q_log = queue.clone();
        let log_p = log_path.clone();
        let log_task = tokio::spawn(async move {
            let mut events = q_log.subscribe_events();
            loop {
                match events.recv().await {
                    Ok(QueueEvent::TaskStarted { id }) => {
                        runner_log(&log_p, &format!("task {id} started"));
                    }
                    Ok(QueueEvent::TaskCompleted { id }) => {
                        runner_log(&log_p, &format!("task {id} completed"));
                    }
                    Ok(QueueEvent::TaskPaused { id }) => {
                        runner_log(&log_p, &format!("task {id} paused"));
                    }
                    Ok(QueueEvent::TaskFailed { id }) => {
                        runner_log(&log_p, &format!("task {id} failed"));
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });

        let stop_flag = Arc::new(AtomicBool::new(false));
        let (stopped_tx, stopped_rx) = watch::channel(false);

        // 常驻循环：每秒检查新的 Queued 任务与 runner.stop 哨兵；每 5 秒重读限速。
        let q = queue.clone();
        let st = store.clone();
        let flag = stop_flag.clone();
        let sp = stop_path.clone();
        let lp = log_path.clone();
        let tx = stopped_tx.clone();
        let watch_task = tokio::spawn(async move {
            let mut current_speed = configured_max_speed(&st).await;
            q.set_global_max_speed(current_speed);
            let mut last_speed_check = Instant::now();
            loop {
                // 仅探测停机信号并通知调用方；所有停机收尾（日志、shutdown、删哨兵）
                // 集中在 Runner::stop 中完成，避免与之重复，也保证 watch 任务能立即
                // 释放其持有的 store/queue clone。
                if flag.load(Ordering::Relaxed) || sp.exists() {
                    break;
                }
                if last_speed_check.elapsed() >= Duration::from_secs(5) {
                    last_speed_check = Instant::now();
                    let new_speed = configured_max_speed(&st).await;
                    if new_speed != current_speed {
                        current_speed = new_speed;
                        q.set_global_max_speed(current_speed);
                        runner_log(
                            &lp,
                            &format!(
                                "global-max-speed changed to {}",
                                current_speed.map_or("unlimited".to_string(), |v| v.to_string())
                            ),
                        );
                    }
                }
                if q.adopt_queued().await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            let _ = tx.send(true);
        });

        Ok(Runner {
            queue,
            lock,
            stop_flag,
            stop_path,
            log_path,
            stopped_tx,
            stopped_rx,
            watch_task: Some(watch_task),
            log_task: Some(log_task),
        })
    }

    /// 返回内部队列，供调用方增删任务和读取进度。
    pub fn queue(&self) -> &DownloadQueue {
        &self.queue
    }

    /// 返回停止信号的接收端副本，供调用方在不持有 Runner 引用的情况下等待停机。
    pub fn stopped_receiver(&self) -> watch::Receiver<bool> {
        self.stopped_rx.clone()
    }

    /// 等待直到运行器因 `runner.stop` 文件（或主动 stop）而停止。
    pub async fn wait_stopped(&self) {
        let mut rx = self.stopped_rx.clone();
        while !*rx.borrow() {
            if rx.changed().await.is_err() {
                break;
            }
        }
    }

    /// 主动安全停止：取消活动任务（变为 Paused(User)）、释放锁、结束所有后台任务。
    pub async fn stop(self) -> Result<(), Error> {
        let mut this = self;
        this.stop_flag.store(true, Ordering::Relaxed);
        // 通知 watch 任务停机后立即中止它：它持有的 store/queue clone 必须在本函数
        // 返回前释放，否则遗留的 tokio 任务会拖住调用方的 store.shutdown()，使
        // watcher 进程在 runner.lock 释放后仍迟迟不退出（Phase 4c 曾修过同类问题）。
        // 停机收尾（shutdown / 删除哨兵）由下面显式完成，不依赖 watch 任务执行。
        if let Some(task) = this.watch_task.take() {
            task.abort();
            let _ = task.await;
        }
        // 显式收尾：与重构前 CLI watch 分支退出时的日志序列保持一致。
        runner_log(&this.log_path, "runner stopping");
        // 显式调用一次 shutdown：取消活动任务为 Paused(User)，不依赖被中止的 watch 任务。
        this.queue.shutdown().await?;
        if let Some(task) = this.log_task.take() {
            task.abort();
            let _ = task.await;
        }
        let _ = std::fs::remove_file(&this.stop_path);
        runner_log(&this.log_path, "runner stopped");
        // 释放 runner.lock（必须在本函数返回前完成）。
        let _ = this.lock.sync_all();
        drop(this.lock);
        // 避免 stopped_tx 的存活副本阻塞任何等待者。
        let _ = this.stopped_tx.send(true);
        Ok(())
    }
}
