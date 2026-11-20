//! 多任务下载队列与调度器（Phase 3b）。
//!
//! 设计目标见 `ARCHITECTURE.md` 第 11 节。本模块只在 `cfg(windows)` 下编译，因为它
//! 复用 [`task`] 中的下载编排（磁盘 I/O 与网络层均为 Windows 专用）。
//!
//! ## 调度规则
//!
//! - 队列持久化在 [`Store`](crate::store::Store)：任务以 `Queued` 状态入队，并带 `queue_order` 与 `max_speed`。
//! - 后台调度循环（`run_scheduler`）按 `queue_order ASC, id ASC` 顺序，把 `Queued` 任务
//!   拉起为活动下载任务，任何时刻活动任务数（`active` 映射长度）严格不超过
//!   `max_concurrent_tasks`（默认 3）。
//! - 活动任务完成 / 失败 / 被暂停后从 `active` 移除并递减「未完成任务计数」
//!   （`outstanding` = 仍在队列或活动中的任务），随即自动唤醒调度器拉起下一个。
//! - 同一队列内的所有任务共用同一个 [`DiskWriterPool`](crate::disk::DiskWriterPool)、全局 [`RateLimiter`](crate::net::ratelimit::RateLimiter) 与按服务器
//!   的 [`HostConnectionLimiter`](crate::net::host_limit::HostConnectionLimiter)，因此全局限速与每主机连接上限对队列内所有任务同时生效。
//! - 下载期间数据库 `state` 仍为 `Queued`（终态由 `finalize_download` 写入），所以调度器
//!   以内存中的 `active` 映射判断并发，而不是数据库状态。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, Notify, broadcast, watch};

use crate::disk::{AllocationStrategy, DiskWriterPool};
use crate::error::{Error, Result};
use crate::net::client::DownloadConfig;
use crate::net::{HostConnectionLimiter, RateLimiter};
use crate::store::{PauseReasonDb, QueuedTask, Store, TaskRecord, TaskStateDb};
use crate::task::{CancelSignal, DownloadOptions, DownloadOutcome, ProgressSnapshot, resume};

/// 队列调度配置。
#[derive(Debug, Clone)]
pub struct QueueConfig {
    /// 活动任务并发上限（默认 3，合法 1..=16）。
    pub max_concurrent_tasks: usize,
    /// 全局限速（字节/秒），`None` 表示不限速。
    pub global_max_speed: Option<u64>,
    /// 新任务的默认下载配置（连接数、每主机连接上限、代理等）。
    pub default_download_config: DownloadConfig,
    /// 新任务的默认分配策略。
    pub default_strategy: AllocationStrategy,
}

impl Default for QueueConfig {
    fn default() -> Self {
        QueueConfig {
            max_concurrent_tasks: 3,
            global_max_speed: None,
            default_download_config: DownloadConfig::default(),
            default_strategy: AllocationStrategy::Auto,
        }
    }
}

/// 入队请求。
#[derive(Debug, Clone)]
pub struct EnqueueRequest {
    pub url: String,
    pub dir: PathBuf,
    pub connections: usize,
    pub out_name: Option<String>,
    pub max_speed: Option<u64>,
    pub strategy: AllocationStrategy,
}

/// 队列事件，供 CLI / 测试订阅。
#[derive(Debug, Clone)]
pub enum QueueEvent {
    TaskEnqueued { id: i64, queue_order: i64 },
    TaskStarted { id: i64 },
    TaskCompleted { id: i64 },
    TaskPaused { id: i64 },
    TaskFailed { id: i64 },
}

/// 队列运行汇总（供 CLI / 测试报告）。
#[derive(Debug, Clone, Default)]
pub struct QueueSummary {
    pub completed: u64,
    pub failed: u64,
    pub paused: u64,
    pub total_elapsed: Duration,
}

/// 重新导出，便于调用方与测试引用。
pub use crate::store::QueueMove;

/// 一个正在运行的受管任务在调度器中的句柄。
struct ActiveTask {
    cancel: Arc<CancelSignal>,
    done: Arc<Notify>,
    finished: Arc<AtomicBool>,
    /// 该任务的进度快照接收端，供 `DownloadQueue::progress` 读取（CLI 逐任务进度）。
    progress_rx: watch::Receiver<ProgressSnapshot>,
}

/// 调度器内部状态（被 [`DownloadQueue`] 以 `Arc` 持有）。
struct Inner {
    store: Arc<Store>,
    pool: Arc<DiskWriterPool>,
    global_limiter: Arc<RateLimiter>,
    host_limiter: Arc<HostConnectionLimiter>,
    config: QueueConfig,
    max_concurrent_tasks: AtomicUsize,
    active: Mutex<HashMap<i64, ActiveTask>>,
    /// 仍未结束（在队列中或活动中）的任务计数；降为 0 表示队列完全空闲。
    /// 使用原子计数作为权威值（`watch::Sender::send` 在无接收者时会丢弃数值，不可靠）。
    outstanding: AtomicU64,
    events: broadcast::Sender<QueueEvent>,
    wake: Notify,
    idle: Notify,
    shutdown: AtomicBool,
    summary: Mutex<QueueSummary>,
    started_at: Mutex<Option<Instant>>,
}

/// 多任务下载队列句柄（可 `Clone`，内部状态共享）。
#[derive(Clone)]
pub struct DownloadQueue {
    inner: Arc<Inner>,
}

impl DownloadQueue {
    /// 创建队列句柄并启动后台调度循环。队列内的任务共享 `pool`、全局限速与每主机连接限制。
    pub fn new(store: Arc<Store>, pool: Arc<DiskWriterPool>, config: QueueConfig) -> Self {
        let max = config.max_concurrent_tasks.clamp(1, 16);
        let global_limiter = Arc::new(RateLimiter::new(config.global_max_speed));
        let host_max = config
            .default_download_config
            .max_connections_per_host
            .clamp(1, 64);
        let host_limiter = Arc::new(
            HostConnectionLimiter::new(host_max)
                .unwrap_or_else(|_| HostConnectionLimiter::default()),
        );
        let (ev_tx, _ev_rx) = broadcast::channel(64);
        let inner = Arc::new(Inner {
            store,
            pool,
            global_limiter,
            host_limiter,
            config,
            max_concurrent_tasks: AtomicUsize::new(max),
            active: Mutex::new(HashMap::new()),
            outstanding: AtomicU64::new(0),
            events: ev_tx,
            wake: Notify::new(),
            idle: Notify::new(),
            shutdown: AtomicBool::new(false),
            summary: Mutex::new(QueueSummary::default()),
            started_at: Mutex::new(Some(Instant::now())),
        });
        let sched = Arc::clone(&inner);
        tokio::spawn(run_scheduler(sched));
        DownloadQueue { inner }
    }

    /// 入队一条 `Queued` 任务，并立即唤醒调度器（若并发槽位空闲则后台启动）。
    pub async fn enqueue(&self, req: EnqueueRequest) -> Result<i64> {
        let queued = QueuedTask {
            url: req.url,
            dir: req.dir.to_string_lossy().into_owned(),
            strategy: format!("{:?}", req.strategy),
            connections: req.connections,
            max_speed: req.max_speed,
            file_name: None,
        };
        let id = self.inner.store.enqueue_task(queued, None).await?;
        let queue_order = self.inner.store.get_task(id).await?.queue_order;
        bump_outstanding(&self.inner, 1);
        let _ = self
            .inner
            .events
            .send(QueueEvent::TaskEnqueued { id, queue_order });
        self.inner.wake.notify_one();
        Ok(id)
    }

    /// 暂停任务：活动中的任务触发取消并等待其收尾（数据库变为 `Paused(User)`）；
    /// 队列中的任务直接将数据库状态改为 `Paused(User)`。
    pub async fn pause(&self, task_id: i64) -> Result<()> {
        let (cancel, done, finished) = {
            let a = self.inner.active.lock().await;
            match a.get(&task_id) {
                Some(t) => (
                    Some(Arc::clone(&t.cancel)),
                    Some(Arc::clone(&t.done)),
                    Some(Arc::clone(&t.finished)),
                ),
                None => (None, None, None),
            }
        };
        if let (Some(cancel), Some(done), Some(finished)) = (cancel, done, finished) {
            cancel.cancel();
            // 先创建 notified()（构造时捕获 notify_waiters 调用计数），再检查条件，
            // 最后 await：这样即使任务恰好在「检查条件之后、注册等待之前」结束并
            // 调用 notify_waiters，唤醒也不会丢失（首次 poll 会看到计数变化而立即返回）。
            loop {
                let notified = done.notified();
                if finished.load(Ordering::SeqCst) {
                    break;
                }
                notified.await;
            }
            Ok(())
        } else {
            let rec = self.inner.store.get_task(task_id).await?;
            match rec.state {
                TaskStateDb::Queued => {
                    self.inner
                        .store
                        .update_state(
                            task_id,
                            TaskStateDb::Paused,
                            Some(PauseReasonDb::User),
                            None,
                        )
                        .await?;
                    bump_outstanding(&self.inner, -1);
                    let _ = self
                        .inner
                        .events
                        .send(QueueEvent::TaskPaused { id: task_id });
                    self.inner.wake.notify_one();
                    Ok(())
                }
                _ => Err(Error::InvalidArgument {
                    message: format!("task {task_id} is neither active nor queued"),
                }),
            }
        }
    }

    /// 将 `Paused` / `Failed` 任务重新入队（置为 `Queued`），并唤醒调度器。
    pub async fn resume_queued(&self, task_id: i64, to_front: bool) -> Result<()> {
        self.inner.store.requeue_task(task_id, to_front).await?;
        bump_outstanding(&self.inner, 1);
        self.inner.wake.notify_one();
        Ok(())
    }

    /// 调整队列中某任务的顺序。
    pub async fn move_queue(&self, task_id: i64, pos: QueueMove) -> Result<()> {
        self.inner.store.move_in_queue(task_id, pos).await?;
        self.inner.wake.notify_one();
        Ok(())
    }

    /// 动态调整并发任务数上限（1..=16）。调大时立即唤醒调度器补足槽位；
    /// 调小时不中断已在运行的任务。
    pub fn set_max_concurrent_tasks(&self, n: usize) -> Result<()> {
        if !(1..=16).contains(&n) {
            return Err(Error::InvalidArgument {
                message: format!("max_concurrent_tasks must be in 1..=16, got {n}"),
            });
        }
        self.inner.max_concurrent_tasks.store(n, Ordering::SeqCst);
        self.inner.wake.notify_one();
        Ok(())
    }

    /// 调整全局限速，对所有正在运行的任务立即生效。
    pub fn set_global_max_speed(&self, bytes_per_sec: Option<u64>) {
        self.inner.global_limiter.set_rate(bytes_per_sec);
    }

    /// Phase 5b-4c：调整每主机连接上限（1..=32），对正在运行的任务立即生效。
    /// 转调共享的 [`HostConnectionLimiter`]：调大唤醒排队分块，调小不中断
    /// 已持有许可的连接，欠额在归还时回收。
    pub fn set_max_connections_per_host(&self, n: usize) -> Result<()> {
        self.inner.host_limiter.set_max_per_host(n)
    }

    /// 订阅队列事件流。
    pub fn subscribe_events(&self) -> broadcast::Receiver<QueueEvent> {
        self.inner.events.subscribe()
    }

    /// 读取某个活动任务的最新进度快照；任务不在活动表中时返回 `None`。
    ///
    /// 供 CLI 在 `queue run` 期间逐个任务打印进度，无需为每个任务另外建立通道。
    pub async fn progress(&self, task_id: i64) -> Option<ProgressSnapshot> {
        let active = self.inner.active.lock().await;
        active.get(&task_id).map(|t| t.progress_rx.borrow().clone())
    }

    /// 让队列接纳数据库中所有已存在的 `Queued` 任务（CLI `queue run` 启动恢复后调用）。
    pub async fn adopt_queued(&self) -> Result<u64> {
        let queued = self.inner.store.list_queued_tasks().await?;
        let n = queued.len() as u64;
        // 原子累加而非覆盖写：store(n) 会与并发的 enqueue / 任务收尾互相覆盖，
        // 丢失这 n 个任务的未完成计数。
        self.inner.outstanding.fetch_add(n, Ordering::SeqCst);
        self.inner.wake.notify_one();
        Ok(n)
    }

    /// 等待队列完全空闲（所有 `Queued` 与活动任务均结束）。返回汇总。
    pub async fn wait_for_idle(&self) -> Result<QueueSummary> {
        // 先创建 notified future 再检查条件，避免丢失唤醒（Notify 推荐用法）。
        loop {
            let notified = self.inner.idle.notified();
            if self.inner.outstanding.load(Ordering::SeqCst) == 0 {
                break;
            }
            notified.await;
        }
        Ok(self.inner.summary.lock().await.clone())
    }

    /// 停止接纳新任务，取消所有当前活动任务并等待它们安全收尾退出。
    /// 未被启动的排队任务（因停机而不再运行）移出未完成计数，避免 `wait_for_idle` 悬挂。
    pub async fn shutdown(&self) -> Result<()> {
        // 1. 先立停机标志：调度循环不再拉起任何 `Queued` 任务。
        self.inner.shutdown.store(true, Ordering::SeqCst);

        // 2. 在取消生效前快照活动任务的取消句柄，并精确统计「未被启动的排队任务」。
        //    活动任务的 DB 状态可能已是 `Downloading`（不在 `list_queued_tasks` 里），
        //    因此不能简单用 `queued.len() - active.len()` 估算，必须按 id 排除活动项。
        let queued = self.inner.store.list_queued_tasks().await?;
        let (cancels, not_started) = {
            let active = self.inner.active.lock().await;
            let cancels = active
                .values()
                .map(|t| Arc::clone(&t.cancel))
                .collect::<Vec<_>>();
            let not_started = queued
                .iter()
                .filter(|r| !active.contains_key(&r.id))
                .count() as i64;
            (cancels, not_started)
        };
        bump_outstanding(&self.inner, -not_started);

        // 3. 先取消所有活动任务（Ctrl+C 安全停止：状态落为 `Paused(User)`）。
        for c in &cancels {
            c.cancel();
        }
        self.inner.wake.notify_one();

        // 4. 再等待它们收尾：先创建 notified()，再检查条件，最后 await，
        //    避免「最后一个任务恰好在检查之后结束」时丢失唤醒。
        loop {
            let notified = self.inner.idle.notified();
            if self.inner.active.lock().await.is_empty() {
                break;
            }
            notified.await;
        }
        Ok(())
    }
}

/// 后台调度循环：反复尝试拉起新任务，直到停机且活动任务清空。
async fn run_scheduler(inner: Arc<Inner>) {
    loop {
        try_schedule(&inner).await;
        let shutdown = inner.shutdown.load(Ordering::SeqCst);
        let active_empty = inner.active.lock().await.is_empty();
        if shutdown && active_empty {
            inner.idle.notify_waiters();
            break;
        }
        inner.wake.notified().await;
    }
}

/// 在并发槽位允许的范围内，按 `queue_order ASC, id ASC` 拉起尽可能多的 `Queued` 任务。
async fn try_schedule(inner: &Arc<Inner>) {
    loop {
        if inner.shutdown.load(Ordering::SeqCst) {
            break;
        }
        let max = inner.max_concurrent_tasks.load(Ordering::SeqCst);
        if inner.active.lock().await.len() >= max {
            break;
        }
        let queued = match inner.store.list_queued_tasks().await {
            Ok(q) => q,
            Err(_) => break,
        };
        // 下载期间 DB 状态仍为 `Queued`，以内存 `active` 排除已在运行的任务。
        let next = {
            let active = inner.active.lock().await;
            queued.into_iter().find(|r| !active.contains_key(&r.id))
        };
        let Some(rec) = next else {
            break;
        };
        spawn_task(inner, rec).await;
    }
}

/// 登记活动任务并启动其下载 future（detached）。
async fn spawn_task(inner: &Arc<Inner>, rec: TaskRecord) {
    let id = rec.id;
    let cancel = Arc::new(CancelSignal::new());
    let done = Arc::new(Notify::new());
    let finished = Arc::new(AtomicBool::new(false));
    let (ptx, prx) = watch::channel(ProgressSnapshot::default());
    inner.active.lock().await.insert(
        id,
        ActiveTask {
            cancel: Arc::clone(&cancel),
            done: Arc::clone(&done),
            finished: Arc::clone(&finished),
            progress_rx: prx,
        },
    );
    let inner2 = Arc::clone(inner);
    let cancel2 = Arc::clone(&cancel);
    tokio::spawn(async move {
        let outcome = run_one(&inner2, id, &cancel2, ptx).await;
        on_finished(&inner2, id, outcome, finished, done).await;
    });
    let _ = inner.events.send(QueueEvent::TaskStarted { id });
    inner.wake.notify_one();
}

/// 运行单个受管任务。全部走 `resume()`：有 `.idm-part` 则续传，否则内部退化为新建下载。
async fn run_one(
    inner: &Arc<Inner>,
    id: i64,
    cancel: &CancelSignal,
    ptx: watch::Sender<ProgressSnapshot>,
) -> Result<DownloadOutcome> {
    let rec = inner.store.get_task(id).await?;
    let opts = build_options(inner, &rec, ptx).await;
    let (outcome, _rx) = resume(
        id,
        Arc::clone(&inner.store),
        Arc::clone(&inner.pool),
        &opts,
        Some(cancel),
    )
    .await?;
    Ok(outcome)
}

/// 任务结束后清理：错误时落终态、移除活动项、递减计数、发事件、唤醒等待者。
async fn on_finished(
    inner: &Arc<Inner>,
    id: i64,
    result: Result<DownloadOutcome>,
    finished: Arc<AtomicBool>,
    done: Arc<Notify>,
) {
    // 1. 错误路径：若数据库里该任务仍是 `Queued`，说明下载在写入任何状态之前
    //    就失败了。必须立刻落终态，否则 `list_queued_tasks` 始终能查到它，
    //    调度器会把同一个任务反复拉起。
    //    该写入必须早于「移除活动项」和「唤醒调度器」：期间 `active` 仍持有这个
    //    id，`try_schedule` 会把它排除，杜绝并发重复拉起。
    if let Err(e) = &result
        && let Ok(rec) = inner.store.get_task(id).await
        && rec.state == TaskStateDb::Queued
    {
        let (state, pause, last_error) = if matches!(e, Error::TaskInUse { .. }) {
            // 9.5：另一个进程正占用该任务，放回暂停状态等待后续恢复，不改失败态。
            (TaskStateDb::Paused, Some(PauseReasonDb::Interrupted), None)
        } else {
            (TaskStateDb::Failed, None, Some(format!("{e}")))
        };
        let _ = inner.store.update_state(id, state, pause, last_error).await;
    }

    finished.store(true, Ordering::SeqCst);
    done.notify_waiters();
    inner.active.lock().await.remove(&id);
    bump_outstanding(inner, -1);
    let event = match &result {
        Ok(DownloadOutcome::Completed) => {
            inc_summary(inner, |s| s.completed += 1).await;
            Some(QueueEvent::TaskCompleted { id })
        }
        Ok(DownloadOutcome::Paused(_)) | Ok(DownloadOutcome::Cancelled) => {
            inc_summary(inner, |s| s.paused += 1).await;
            Some(QueueEvent::TaskPaused { id })
        }
        Err(_) => {
            inc_summary(inner, |s| s.failed += 1).await;
            Some(QueueEvent::TaskFailed { id })
        }
    };
    if let Some(ev) = event {
        let _ = inner.events.send(ev);
    }
    if inner.outstanding.load(Ordering::SeqCst) == 0
        && let Some(start) = inner.started_at.lock().await.take()
    {
        inner.summary.lock().await.total_elapsed = start.elapsed();
    }
    // 无条件唤醒：既服务 `wait_for_idle`（判断 outstanding），也服务 `shutdown`
    // （判断 active）；只在计数归零时通知会让 `shutdown` 因活动任务已清空但
    // 计数不为 0 而永远等不到唤醒。
    inner.idle.notify_waiters();
    inner.wake.notify_one();
}

/// 构造单个任务的 [`DownloadOptions`]，复用队列的全局限速与每主机连接限制。
async fn build_options(
    inner: &Inner,
    rec: &TaskRecord,
    ptx: watch::Sender<ProgressSnapshot>,
) -> DownloadOptions {
    let mut config = inner.config.default_download_config.clone();
    if let Ok(cs) = u8::try_from(rec.connections) {
        config.connections = (cs as usize).clamp(1, 32);
    }
    // Phase 4a：从数据库读取该任务的请求头（Referer / Cookie / User-Agent）并注入配置，
    // 随后经 `build_client` 的默认头对所有请求生效。
    config.extra_headers = inner
        .store
        .get_task_headers(rec.id)
        .await
        .unwrap_or_default();
    let strategy = match rec.strategy.as_str() {
        "SetLen" => AllocationStrategy::SetLen,
        "Reserve" => AllocationStrategy::Reserve,
        "Sparse" => AllocationStrategy::Sparse,
        _ => AllocationStrategy::Auto,
    };
    DownloadOptions {
        config,
        strategy,
        dir: PathBuf::from(&rec.dir),
        out_name: rec.file_name.clone(),
        progress_tx: Some(ptx),
        max_speed: rec.max_speed,
        global_limiter: Some(Arc::clone(&inner.global_limiter)),
        host_limiter: Some(Arc::clone(&inner.host_limiter)),
    }
}

/// 原子地增减未完成计数，下限为 0（饱和）。
///
/// 增加走 `fetch_add`，减少走原子读改写 `try_update`（即旧名 `fetch_update`，
/// rustc 1.99.1 起重命名）；绝不使用 `load` + `store` 的非原子读改写，否则并发的
/// `on_finished` 会互相覆盖，计数永远无法归零，`wait_for_idle` 随之永久挂起。
fn bump_outstanding(inner: &Arc<Inner>, delta: i64) {
    if delta >= 0 {
        let _ = inner.outstanding.fetch_add(delta as u64, Ordering::SeqCst);
    } else {
        let n = delta.unsigned_abs();
        let _ = inner
            .outstanding
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |cur| {
                Some(cur.saturating_sub(n))
            });
    }
}

async fn inc_summary(inner: &Arc<Inner>, f: impl FnOnce(&mut QueueSummary)) {
    let mut s = inner.summary.lock().await;
    f(&mut s);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use tempfile::tempdir;

    type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

    /// 构造一个带独立 SQLite 库的测试队列。
    fn test_queue(dir: &std::path::Path) -> TestResult<DownloadQueue> {
        let store = Store::open(dir.join("q.db"))?;
        let pool = Arc::new(DiskWriterPool::new(2, 16)?);
        Ok(DownloadQueue::new(store, pool, QueueConfig::default()))
    }

    /// 在活动表中登记一个「正在运行」的任务句柄（不启动真实下载），
    /// 返回完成信号与结束标志，供测试精确模拟任务在任意时刻自然结束。
    async fn install_active(q: &DownloadQueue, id: i64) -> (Arc<Notify>, Arc<AtomicBool>) {
        let done = Arc::new(Notify::new());
        let finished = Arc::new(AtomicBool::new(false));
        let (_tx, rx) = watch::channel(ProgressSnapshot::default());
        q.inner.active.lock().await.insert(
            id,
            ActiveTask {
                cancel: Arc::new(CancelSignal::new()),
                done: Arc::clone(&done),
                finished: Arc::clone(&finished),
                progress_rx: rx,
            },
        );
        (done, finished)
    }

    #[tokio::test]
    async fn enqueue_preserves_order_and_state() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("q.db");
        let store = Store::open(&db).unwrap();
        let pool = Arc::new(DiskWriterPool::new(2, 16).unwrap());
        let q = DownloadQueue::new(store.clone(), pool, QueueConfig::default());
        let mut ids = Vec::new();
        for i in 0..4 {
            let id = q
                .enqueue(EnqueueRequest {
                    url: format!("http://h/file{i}.bin"),
                    dir: dir.path().to_path_buf(),
                    connections: 4,
                    out_name: None,
                    max_speed: None,
                    strategy: AllocationStrategy::Auto,
                })
                .await
                .unwrap();
            ids.push(id);
        }
        let queued = store.list_queued_tasks().await.unwrap();
        assert_eq!(queued.len(), 4);
        // 严格按入队顺序（queue_order 递增、id 递增）。
        for (i, r) in queued.iter().enumerate() {
            assert_eq!(r.id, ids[i]);
            assert_eq!(r.queue_order, (i + 1) as i64);
            assert_eq!(r.state, TaskStateDb::Queued);
        }
        q.shutdown().await.unwrap();
    }

    /// 任务在 `pause()` 调用的同一时刻自然结束：重复 200 次，每次 `pause`
    /// 都必须在 1 秒内返回。多线程 + 不同 yield 次数扫描「检查条件之后、
    /// 注册等待之前」这一会丢失 `notify_waiters` 唤醒的竞态窗口。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pause_does_not_hang_when_task_finishes_concurrently() -> TestResult {
        let dir = tempdir()?;
        let q = test_queue(dir.path())?;
        for iteration in 0..200 {
            let id = (iteration + 1) as i64;
            let (done, finished) = install_active(&q, id).await;
            let fin = Arc::clone(&finished);
            let yields = iteration % 16;
            // 模拟任务恰好在 pause 调用期间自然结束：先置位结束标志，再
            // notify_waiters（与 on_finished 的顺序一致）。故意不立即移出活动表，
            // 保证 pause 每次都进入「等待活动任务收尾」的循环。
            let finisher = tokio::spawn(async move {
                for _ in 0..yields {
                    tokio::task::yield_now().await;
                }
                fin.store(true, Ordering::SeqCst);
                done.notify_waiters();
            });
            match tokio::time::timeout(Duration::from_secs(1), q.pause(id)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => panic!("iteration {iteration}: pause 挂起超过 1 秒"),
            }
            finisher.await?;
            q.inner.active.lock().await.remove(&id);
        }
        q.shutdown().await?;
        Ok(())
    }

    /// 20 个任务同时结束：未完成计数必须精确归零，`wait_for_idle` 正常返回；
    /// 重复 50 次。旧实现 `load` + `store` 的非原子读改写会丢失并发递减。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn outstanding_stays_consistent_under_concurrent_finish() -> TestResult {
        for iteration in 0..50 {
            let dir = tempdir()?;
            let q = test_queue(dir.path())?;
            let mut joins = Vec::new();
            for i in 0..20 {
                let id = (i + 1) as i64;
                let (done, finished) = install_active(&q, id).await;
                bump_outstanding(&q.inner, 1);
                let inner = Arc::clone(&q.inner);
                joins.push(tokio::spawn(async move {
                    on_finished(&inner, id, Ok(DownloadOutcome::Completed), finished, done).await;
                }));
            }
            for j in joins {
                j.await?;
            }
            assert_eq!(
                q.inner.outstanding.load(Ordering::SeqCst),
                0,
                "iteration {iteration}: 并发收尾后未完成计数未归零"
            );
            match tokio::time::timeout(Duration::from_secs(2), q.wait_for_idle()).await {
                Ok(Ok(s)) => assert_eq!(s.completed, 20, "iteration {iteration}: 汇总计数错误"),
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => panic!("iteration {iteration}: wait_for_idle 挂起"),
            }
            q.shutdown().await?;
        }
        Ok(())
    }
}
