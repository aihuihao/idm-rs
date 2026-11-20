//! 下载编排：Probe → 选择模式（静态分块 / 单连接顺序）→ JoinSet 并发 → 完成/取消/暂停。
//!
//! 取消基于 [`CancelSignal`]（`watch` 状态语义），在下载各阶段与每个分块任务的网络读取/
//! 磁盘写入之前检查，绝不会因"取消发生在等待开始之前"而漏掉。
//!
//! 协作式取消：每次下载内部创建一个 [`CancelSignal`]。外部取消、任意分块出错或 panic 都会
//! 先触发这个内部信号；分块任务订阅内部信号，在各自的检查点自行退出（并走到 `pool.release`
//! 释放写器）。主流程在触发内部信号后**等待**所有分块任务退出（最多 5 秒），超时才 `abort_all`。
//! 这杜绝了写入器与文件句柄泄漏。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use reqwest::header::{CONTENT_ENCODING, CONTENT_RANGE, ETAG, IF_RANGE, RANGE, RETRY_AFTER};
use reqwest::{Client, StatusCode, Url};
use tokio::sync::{Mutex, watch};
use tokio::task::JoinSet;
use tracing::{info, trace, warn};

use crate::disk::win32;
use crate::disk::{AllocationStrategy, DiskWriterPool, PoolChunk, SpaceCheck, TargetFile};
use crate::error::{Error, Result};
use crate::net::client::DownloadConfig;
use crate::net::filename::{infer_filename, resolve_conflict, sanitize_windows_filename};
use crate::net::headers::parse_content_range;
use crate::net::probe::{Probe, ProbeResult};
use crate::net::retry::{
    MAX_TOTAL_ATTEMPTS, PROGRESS_RESET_BYTES, XorShift64, backoff_delay, is_retryable,
    parse_retry_after,
};
use crate::net::{BandwidthLimiter, HostConnectionLimiter, HostPermit, RateLimiter};
use crate::store::{ChunkRow, PauseReasonDb, ProbeInfo, Store, TaskStateDb};
use crate::task::cancel::{CancelSignal, wait_cancelled};
use crate::task::progress::{ChunkInit, ProgressPublisher, ProgressSnapshot};
use crate::task::split::{
    ChunkShared, ChunkSlot, PendingChunk, in_flight_count, launch_new_chunk, try_split,
};
use crate::task::state::{ChunkState, TaskState};

/// 各分块的 `(chunk_index, acked 绝对位置)` 列表。
///
/// Phase 2 起分块会在运行中动态增加（切分），因此用 `Arc<Mutex<Vec<_>>>` 共享给
/// 检查点任务与切分流程；锁内只做列表复制，锁外再做快照与 IO。
pub(super) type AckedList = Arc<StdMutex<Vec<(i64, Arc<AtomicU64>)>>>;

/// 检查点快照观察回调：参数是 (chunk_index, acked 绝对位置) 的快照 S。
pub type SnapshotObserver = Arc<dyn Fn(&[(i64, u64)]) + Send + Sync>;

/// 检查点测试钩子（F.4 / F.5 / 1c.1 退出路径测试）。
#[derive(Clone, Default)]
pub struct CheckpointHooks {
    /// 置位时 `sync` 视为失败：本次检查点放弃，不写数据库。
    pub fail_sync: Arc<AtomicBool>,
    /// 置位时 `sync` 返回磁盘满错误（Windows 错误码 112 `ERROR_DISK_FULL`）：
    /// 错误经真实的系统错误分类转换成 `DiskFullDuringWrite`，用于验证"最后一次
    /// 检查点磁盘满 → `Paused(DiskFull)`"这条退出路径。
    pub fail_sync_disk_full: Arc<AtomicBool>,
    /// 取得快照 S 之后、`sync` 之前调用，用于验证写入数据库的值不超过 S。
    pub observe_snapshot: Option<SnapshotObserver>,
    /// 目标文件创建之后、分块任务启动之前调用。测试可在此触发外部取消，
    /// 以精确命中"分块启动循环中取消"这条退出路径。
    pub before_chunks: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Probe 成功之后、第一处"建文件之前取消"检查之前调用。测试可在此触发
    /// 外部取消，以精确命中"Probe 之后、建文件之前取消"这条退出路径。
    pub after_probe: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl std::fmt::Debug for CheckpointHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CheckpointHooks")
            .field("fail_sync", &self.fail_sync.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// 续传计划：已完成 Probe 比较后由 `resume()` 构造，交给下载流程执行。
#[derive(Debug, Clone)]
pub struct ResumePlan {
    pub final_path: PathBuf,
    pub tmp_path: PathBuf,
    pub file_name: String,
    pub final_url: Url,
    pub total_size: u64,
    pub etag: Option<(String, bool)>,
    pub last_modified: Option<String>,
    /// 数据库中的全部分块记录（Phase 2：含编号与来源；续传时可继续切分）。
    pub chunks: Vec<ResumeChunk>,
}

/// 一条续传分块记录。
#[derive(Debug, Clone, Copy)]
pub struct ResumeChunk {
    /// 数据库中的 `chunk_index`。
    pub index: i64,
    pub start: u64,
    pub end: u64,
    pub committed: u64,
    /// 来源：`None` = 初始分块，`Some(i)` = 从第 i 号分块切出。
    pub parent: Option<i64>,
}

/// 持久化上下文。传 `None` 时 `download()` 的行为与 Phase 1b 完全一致。
#[derive(Clone)]
pub struct DownloadContext {
    pub store: Arc<Store>,
    pub task_id: i64,
    pub resume: Option<ResumePlan>,
    pub checkpoint_interval: Duration,
    pub hooks: CheckpointHooks,
}

impl DownloadContext {
    /// 新建下载的默认上下文（每 2 秒一个检查点）。
    pub fn new(store: Arc<Store>, task_id: i64) -> Self {
        DownloadContext {
            store,
            task_id,
            resume: None,
            checkpoint_interval: Duration::from_secs(2),
            hooks: CheckpointHooks::default(),
        }
    }
}

/// ARCHITECTURE.md 9.2 检查点协议，**顺序不得改变**：
/// 1. 读取每个分块当前的 acked 位置，记为快照 S
/// 2. 等待 `pool.sync(target)` 完成
/// 3. sync 成功后，在一个数据库事务中把 S 写入 committed（只增不减）
///
/// sync 失败时本次检查点放弃，不写数据库。
pub(crate) async fn run_checkpoint(
    ctx: &DownloadContext,
    pool: &DiskWriterPool,
    target: &TargetFile,
    acked: &[(i64, Arc<AtomicU64>)],
) -> Result<()> {
    // 步骤 1：快照 S。每个字节都在 sync 开始前收到了 pool.write 的成功回执。
    //
    // 内存顺序：分块任务用 `fetch_add(.., Release)` 推进 acked，这里用 `Acquire`
    // 读取，构成 release/acquire 同步——看到某个 acked 值时，对应的 pool.write
    // 成功回执及其之前的写入必然已完成，不会发生"值已推进但数据还没交回"
    // 的情况。Relaxed 只能保证原子性，不能保证这一可见性，因此不能用。
    let snapshot: Vec<(i64, u64)> = acked
        .iter()
        .map(|(db_index, a)| (*db_index, a.load(Ordering::Acquire)))
        .collect();
    if let Some(observe) = &ctx.hooks.observe_snapshot {
        observe(&snapshot);
    }

    // 测试钩子：让 sync 以系统错误码 112 失败（真实错误分类，而非直接构造）。
    if ctx.hooks.fail_sync_disk_full.load(Ordering::SeqCst) {
        return Err(crate::error::io_error(
            "fsync",
            target.path().to_path_buf(),
            0,
            std::io::Error::from_raw_os_error(112),
        ));
    }
    // 步骤 2：sync。失败 → 直接返回，绝不写数据库。
    if ctx.hooks.fail_sync.load(Ordering::SeqCst) {
        return Err(Error::Io {
            operation: "injected sync failure (checkpoint abandoned)",
            path: target.path().to_path_buf(),
            offset: 0,
            source: std::io::Error::other("injected sync failure"),
        });
    }
    pool.sync(target).await?;

    // 步骤 3：单个事务，committed = MAX(committed, S)。
    ctx.store.checkpoint(ctx.task_id, &snapshot).await?;
    Ok(())
}

/// 下载结果。`Cancelled` / `Paused` 与 `Completed` 同属"非失败"结果，但保留 `.idm-part`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadOutcome {
    Completed,
    Cancelled,
    Paused(PauseReason),
}

/// 暂停原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseReason {
    DiskFull,
}

/// 下载选项。
#[derive(Debug, Clone)]
pub struct DownloadOptions {
    pub config: DownloadConfig,
    pub strategy: AllocationStrategy,
    pub dir: PathBuf,
    pub out_name: Option<String>,
    /// 可选的外部进度通道发送端：调用方可在下载开始前订阅 `watch::Receiver`，
    /// 从而在下载过程中实时读取进度。传 `None` 时由库内部创建通道。
    pub progress_tx: Option<watch::Sender<ProgressSnapshot>>,
    /// 整次下载的总限速（字节/秒）。`None` 表示不限速，行为与本字段加入前一致。
    /// `Some(rate)` 时所有分块共用同一个 `RateLimiter`，限制的是这次下载的总速度。
    pub max_speed: Option<u64>,
    /// Phase 3a：可选的**全局**限速器，多个并发 `download` / `resume` 调用传入同一个
    /// `Arc<RateLimiter>` 即可共享总带宽。与 `max_speed` 同时存在时两层配额都要满足。
    pub global_limiter: Option<Arc<RateLimiter>>,
    /// Phase 3a：可选的共享按 Host 连接限制器。未传入时按
    /// [`DownloadConfig::max_connections_per_host`] 在任务内部创建一个。
    pub host_limiter: Option<Arc<HostConnectionLimiter>>,
}

/// 执行一次下载。返回结果，以及进度快照的 `watch` 接收端（可不读）。
///
/// `cancel` 为外部取消信号（基于 `watch`，不会漏掉任何时刻的取消）；传 `None`
/// 表示不可取消。取消、失败、磁盘满时均保留 `.idm-part` 文件（仅在目标文件已创建后）；
/// 目标文件尚未创建时（Probe 失败、建文件前取消、建文件时磁盘满）不留下任何文件。
pub async fn download(
    url: &str,
    pool: Arc<DiskWriterPool>,
    opts: &DownloadOptions,
    cancel: Option<&CancelSignal>,
) -> Result<(DownloadOutcome, watch::Receiver<ProgressSnapshot>)> {
    download_with_store(url, pool, opts, cancel, None).await
}

/// 运行期需要统一收尾的资源。由 `run_download` 填充，`finalize_download` 消费。
#[derive(Default)]
struct DownloadRuntime {
    /// 目标文件（已创建时 `Some`）。9.4 改名之前必须释放最后一个句柄。
    target: Option<Arc<TargetFile>>,
    /// 检查点任务及其停止信号；收尾时先发信号再 `await` 任务真正结束。
    checkpoint_task: Option<(
        tokio::task::JoinHandle<()>,
        tokio::sync::oneshot::Sender<()>,
    )>,
    /// 外部取消 → 内部取消的转发任务。
    forward_task: Option<tokio::task::JoinHandle<()>>,
    /// 各分块的 acked 计数器（最终检查点的快照来源）。
    ///
    /// Phase 2 起分块会在运行中增加（动态切分），因此这里用 `Arc<Mutex<Vec<_>>>`
    /// 共享给检查点任务：每次检查点只在锁内复制一份列表，锁外再做快照与 IO。
    acked: AckedList,
    /// 进度发布器（收尾时发布最终快照）。
    progress: Option<Arc<Mutex<ProgressPublisher>>>,
    /// 返回给调用者的进度接收端。
    progress_rx: Option<watch::Receiver<ProgressSnapshot>>,
    /// `.idm-part` 路径。
    tmp_path: Option<PathBuf>,
    /// 最终文件路径（完成阶段改名目标）。
    final_path: Option<PathBuf>,
    task_id: u64,
}

/// 所有退出路径的类别。`finalize_download` 是**唯一**收尾点，负责把这些类别
/// 映射到数据库状态、返回值以及磁盘上的文件。
enum ExitKind {
    /// 路径 1、2：目标文件创建之前取消 → Paused(User)，part_path 为空，无文件。
    CancelledBeforeCreate,
    /// 路径 3：目标文件创建之前磁盘满 → Paused(DiskFull)，part_path 为空，无文件。
    DiskFullBeforeCreate,
    /// 路径 4 及其他失败：→ Failed 并记录错误。
    Failed(Error),
    /// 路径 5 及下载中取消：目标文件已创建 → Paused(User)，保留 .idm-part。
    Cancelled,
    /// 目标文件已创建，磁盘满暂停 → Paused(DiskFull)，保留 .idm-part。
    PausedDiskFull,
    /// 全部分块成功，等待 9.4 完成阶段（必须先过最终检查点门禁）。
    ReadyToFinalize,
}

/// 带可选持久化的下载。`ctx` 为 `None` 时行为与 [`download`] 完全一致；
/// `Some` 时启用 acked 跟踪、9.2 检查点循环、状态持久化与 9.4 完成顺序。
///
/// 本函数**只有两步**：运行内部流程、调用唯一的收尾函数。内部流程可以自由使用
/// `return` / `?`，但无论它返回什么，结果都会经过 `finalize_download`；这里
/// 不存在绕过收尾的 `return` 或 `?`。
pub async fn download_with_store(
    url: &str,
    pool: Arc<DiskWriterPool>,
    opts: &DownloadOptions,
    cancel: Option<&CancelSignal>,
    ctx: Option<DownloadContext>,
) -> Result<(DownloadOutcome, watch::Receiver<ProgressSnapshot>)> {
    let mut rt = DownloadRuntime::default();
    let inner = run_download(url, &pool, opts, cancel, ctx.as_ref(), &mut rt).await;
    finalize_download(&pool, opts, ctx.as_ref(), &mut rt, inner).await
}

/// 下载主流程：所有可能提前退出的逻辑都在这里，退出一律返回 `ExitKind`
/// （或把错误通过 `?` 交给外层），由 `finalize_download` 统一收尾。
async fn run_download(
    raw_url: &str,
    pool: &Arc<DiskWriterPool>,
    opts: &DownloadOptions,
    cancel: Option<&CancelSignal>,
    ctx: Option<&DownloadContext>,
    rt: &mut DownloadRuntime,
) -> Result<ExitKind> {
    let task_id = ctx.map_or(0, |c| c.task_id as u64);
    rt.task_id = task_id;
    let url = Url::parse(raw_url).map_err(|e| Error::InvalidArgument {
        message: format!("invalid URL: {e}"),
    })?;
    DownloadConfig::check_scheme(&url)?;

    let cancelled_now = || cancel.is_some_and(|c| c.is_cancelled());

    // Phase 5b-4b：实际生效的 Host 连接限制器——优先用调用方注入的（队列路径会
    // 注入自己的 limiter），否则按任务 config 新建。必须在此处（初始分块之前）确定，
    // 使初始分块数、有效并发、拆分阈值、分块上限全部基于同一个上限值，避免
    // 「config 值与注入的 limiter 上限不同」时口径不一致导致分块失控。
    let host_limiter: Arc<HostConnectionLimiter> = opts.host_limiter.clone().unwrap_or_else(|| {
        let limit = opts.config.max_connections_per_host;
        Arc::new(
            HostConnectionLimiter::new(limit).unwrap_or_else(|_| HostConnectionLimiter::default()),
        )
    });
    let host_max = host_limiter.max_per_host();

    // Phase 2.1 第一部分：续传时新分块编号必须避开数据库里**所有**已有编号。
    // 编号最大的分块可能已经下载完（被 `committed < end` 过滤掉），只看 inits 会让
    // 切分事务插入重复主键。`plan.chunks` 由 resume() 从全部分块记录构造。
    let resume_max_chunk_index = ctx
        .and_then(|c| c.resume.as_ref())
        .and_then(|p| p.chunks.iter().map(|c| c.index).max());

    // 元组：(client, probe_result, final_path, tmp_path, target, inits,
    //       total_for_progress, streaming)
    // 续传时 Probe 比较已由 resume() 完成，直接复用既有 .idm-part；
    // 新建下载走原来的 Probe → 创建文件 → 静态分块流程。
    let (client, probe_result, final_path, tmp_path, target, inits, total_for_progress, streaming) =
        if let Some(plan) = ctx.and_then(|c| c.resume.clone()) {
            let client = opts.config.build_client()?;
            let pr = ProbeResult {
                final_url: plan.final_url,
                total_size: Some(plan.total_size),
                supports_range: true,
                etag: plan.etag,
                last_modified: plan.last_modified,
                filename: plan.file_name.clone(),
                content_type: None,
            };
            // 9.4 / D.4：打开已有 .idm-part 并校验长度。
            let tf = match TargetFile::open_existing(&plan.tmp_path, plan.total_size) {
                Ok(t) => t,
                // 9.5：另一个进程持有可写句柄 → TaskInUse（数据库状态不变）。
                Err(Error::Io { source, .. }) if crate::error::is_sharing_violation(&source) => {
                    return Err(Error::TaskInUse {
                        id: ctx.map_or(0, |c| c.task_id),
                    });
                }
                Err(e) => return Err(e),
            };
            // 续传读取**全部**分块记录：`committed < end` 的继续下载，并且仍可再切分。
            let init: Vec<ChunkInit> = plan
                .chunks
                .iter()
                .filter(|c| c.committed < c.end)
                .map(|c| ChunkInit {
                    db_index: c.index,
                    start: c.committed,
                    end: c.end,
                    parent: c.parent,
                    takeover: false,
                })
                .collect();
            (
                client,
                pr,
                plan.final_path,
                plan.tmp_path,
                tf,
                init,
                Some(plan.total_size),
                false,
            )
        } else {
            // 文件名在 Probe 之前先由 --out / URL 推断；Probe 之后若拿到 Content-Disposition 再重算。
            let pre_name = match &opts.out_name {
                Some(n) => sanitize_windows_filename(n),
                None => infer_filename(&url, None),
            };
            let mut fp = resolve_conflict(&opts.dir, &pre_name);
            let mut tp = part_path(&fp);

            // 路径 1：Probe 开始之前取消（此时还没有任何文件）。
            if cancelled_now() {
                return Ok(ExitKind::CancelledBeforeCreate);
            }

            let client = opts.config.build_client()?;
            let probe = Probe::new(&client, &opts.config);
            let pr = probe.probe(&url).await?;

            // Probe 得到 Content-Disposition 后重算最终文件名（未显式指定 --out 时）。
            if opts.out_name.is_none() {
                fp = resolve_conflict(&opts.dir, &pr.filename);
                tp = part_path(&fp);
            }

            // 测试钩子：Probe 成功之后、第一处取消检查之前触发外部取消（路径 2）。
            if let Some(hook) = ctx.and_then(|c| c.hooks.after_probe.as_ref()) {
                hook();
            }

            // 路径 2（两处）：Probe 完成、创建目标文件之前取消。
            if cancelled_now() {
                return Ok(ExitKind::CancelledBeforeCreate);
            }
            // 让出一次，使外部取消有机会在"建文件之前"被观察到（协作式，不创建空文件）。
            tokio::task::yield_now().await;
            if cancelled_now() {
                return Ok(ExitKind::CancelledBeforeCreate);
            }

            // 选择模式：总长已知且支持 Range → 静态分块；否则单连接顺序（不预分配）。
            let (tf, init, total, stream) = if let (Some(total), true) =
                (pr.total_size, pr.supports_range)
                && total > 0
            {
                let tf = match TargetFile::create(&tp, total, opts.strategy, SpaceCheck::Enforce) {
                    Ok(t) => t,
                    // 路径 3：创建时磁盘满，文件尚未落盘，不留下任何文件。
                    Err(e) if e.is_disk_full() => {
                        return Ok(ExitKind::DiskFullBeforeCreate);
                    }
                    // 路径 4：创建出错 → 收尾统一写 Failed 并记录错误。
                    Err(e) => return Err(e),
                };
                // Phase 5b-4b：初始分块数按「有效并发」= min(连接数, 实际生效的
                // 每主机上限 host_max) 生成，而不是连接数本身，也不能用可能与注入
                // limiter 不一致的 opts.config.max_connections_per_host。否则连接数
                // 大于主机上限时会一次性创建大量永远拿不到 Host 名额的排队分块。
                let effective = opts.config.connections.min(host_max).max(1);
                let init: Vec<ChunkInit> =
                    chunk_ranges(total, effective, opts.config.min_chunk_size)
                        .into_iter()
                        .enumerate()
                        .map(|(i, (start, end))| ChunkInit {
                            db_index: i as i64,
                            start,
                            end,
                            parent: None,
                            takeover: false,
                        })
                        .collect();
                (tf, init, Some(total), false)
            } else {
                // 顺序下载路径的 create 出错同样由收尾统一写 Failed。
                let tf = TargetFile::create(&tp, 0, opts.strategy, SpaceCheck::Skip)?;
                let init = vec![ChunkInit {
                    db_index: 0,
                    start: 0,
                    end: u64::MAX,
                    parent: None,
                    takeover: false,
                }];
                (tf, init, None, true)
            };
            (client, pr, fp, tp, tf, init, total, stream)
        };

    let target = Arc::new(target);
    rt.target = Some(Arc::clone(&target));
    rt.tmp_path = Some(tmp_path.clone());
    rt.final_path = Some(final_path.clone());

    // Probe 成功后回填元信息；新建任务再写入初始分块（续传保留已有 committed）。
    // 注意：路径 1~4 都在本语句之前退出，因此这些任务的 part_path 保持为空。
    if let Some(c) = ctx {
        let file_name = final_path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        let info = ProbeInfo {
            final_url: probe_result.final_url.to_string(),
            file_name,
            part_path: tmp_path.to_string_lossy().into_owned(),
            total_size: probe_result.total_size,
            supports_range: probe_result.supports_range,
            etag: probe_result.etag.as_ref().map(|(t, _)| t.clone()),
            etag_weak: probe_result.etag.as_ref().is_some_and(|(_, weak)| *weak),
            last_modified: probe_result.last_modified.clone(),
            resumable: !streaming,
        };
        c.store.update_probe(c.task_id, info).await?;
        if c.resume.is_none() {
            let rows: Vec<ChunkRow> = inits
                .iter()
                .map(|c| ChunkRow {
                    chunk_index: c.db_index,
                    start: c.start,
                    end: c.end,
                    committed: c.start,
                    parent: c.parent,
                })
                .collect();
            c.store.save_chunks(c.task_id, rows).await?;
        }
    }

    // If-Range / 弱 ETag 校验策略：
    // - 强 ETag → 直接作为 If-Range；
    // - 弱 ETag 但有 Last-Modified → 用 Last-Modified 作为 If-Range；
    // - 无 ETag → 用 Last-Modified（若有）作为 If-Range；
    // - 弱 ETag 且无 Last-Modified → 不发 If-Range。
    // 无论强弱，都保留 `verify_etag` 用于逐分块校验响应中的 ETag。
    let if_range: Option<String> = match &probe_result.etag {
        Some((tag, weak)) if !*weak => Some(tag.clone()),
        Some(_) if probe_result.last_modified.is_some() => probe_result.last_modified.clone(),
        None => probe_result.last_modified.clone(),
        Some(_) => None,
    };
    let verify_etag: Option<(String, bool)> = probe_result.etag.clone();

    // 单连接顺序下载没有分块边界，明确标记为"不可续传"供 1c 使用。
    let (progress_pub, progress_rx) = ProgressPublisher::new(
        total_for_progress,
        &inits,
        opts.progress_tx.clone(),
        !streaming,
    );
    rt.progress_rx = Some(progress_rx);
    let progress = Arc::new(Mutex::new(progress_pub));
    rt.progress = Some(Arc::clone(&progress));
    let download_url = probe_result.final_url.clone();

    // Phase 3a 双层限速：单任务 `max_speed` + 可跨任务共享的 `global_limiter`。
    // 两块数据都要依次通过两者配额；`set_rate` 可在运行中修改任一层。
    let task_limiter: Option<Arc<RateLimiter>> = opts
        .max_speed
        .map(|rate| Arc::new(RateLimiter::new(Some(rate))));
    let bandwidth = BandwidthLimiter::new(task_limiter, opts.global_limiter.clone());
    // host_limiter 已在函数开头（初始分块之前）确定，见 Phase 5b-4b 注释。

    // 内部取消信号：所有分块任务订阅它；外部取消/出错/panic 都会先触发它。
    let internal = CancelSignal::new();
    if let Some(ext) = cancel {
        let int = internal.clone();
        let ext = ext.clone();
        rt.forward_task = Some(tokio::spawn(async move {
            ext.cancelled().await;
            int.cancel();
        }));
    }

    // acked：每个分块的绝对写入位置，只在 pool.write 成功返回后推进。
    let acked: AckedList = Arc::new(StdMutex::new(Vec::with_capacity(inits.len())));
    rt.acked = Arc::clone(&acked);

    // 每个任务独立运行一个检查点循环（9.2：定时触发，默认每 2 秒）。
    // 用 oneshot 作为停止信号，收尾时先发信号再 await 任务真正结束。
    if let Some(c) = ctx {
        let c = c.clone();
        let p = Arc::clone(pool);
        let t = Arc::clone(&target);
        let a = acked.clone();
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
        let mut ticker = tokio::time::interval(c.checkpoint_interval);
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = &mut stop_rx => break,
                    _ = ticker.tick() => {
                        // 只在锁内复制列表（动态切分可能刚刚追加了分块），锁外做快照与 IO。
                        let list = a.lock().unwrap_or_else(|e| e.into_inner()).clone();
                        if let Err(e) = run_checkpoint(&c, &p, &t, &list).await {
                            warn!(error = %e, "checkpoint failed; database not updated");
                        }
                    }
                }
            }
        });
        rt.checkpoint_task = Some((handle, stop_tx));
    }

    // 测试钩子：在目标文件创建之后、分块任务启动之前触发外部取消（路径 5）。
    if let Some(hook) = ctx.and_then(|c| c.hooks.before_chunks.as_ref()) {
        hook();
    }

    let spawner = ChunkSpawner {
        client: client.clone(),
        url: download_url.clone(),
        pool: Arc::clone(pool),
        target: Arc::clone(&target),
        progress: Arc::clone(&progress),
        if_range: if_range.clone(),
        verify_etag: verify_etag.clone(),
        bandwidth: bandwidth.clone(),
        host_limiter: Arc::clone(&host_limiter),
        cancel: internal.clone(),
        cancel_rx: internal.subscribe(),
        task_id,
        streaming,
        max_retries: opts.config.max_retries,
        backoff_base: opts.config.retry_base_delay,
        backoff_cap: opts.config.retry_max_delay,
        min_split: opts.config.min_split_size,
        // Phase 5b-4c：保存任务连接数；有效并发与分块硬上限改为每次判断时
        // 动态计算（见 ChunkSpawner::effective_concurrency/max_chunks），
        // 使运行中调整每主机上限后拆分行为能立即跟随。
        connections: opts.config.connections,
    };

    let mut join_set: JoinSet<Result<ChunkResult>> = JoinSet::new();
    let mut slots: Vec<ChunkSlot> = Vec::with_capacity(inits.len());
    // 续传用数据库全部分块记录的最大编号；新建下载用初始分块的最大编号。
    let mut next_db_index: i64 = resume_max_chunk_index
        .or_else(|| inits.iter().map(|c| c.db_index).max())
        .map_or(0, |m| m + 1);
    let mut aborted_before_all_spawned = false;
    for (idx, init) in inits.iter().enumerate() {
        // 取消检查点 3：分块任务全部启动之前。
        if cancelled_now() {
            aborted_before_all_spawned = true;
            break;
        }
        let shared = Arc::new(ChunkShared::new(init.start, init.end, init.db_index));
        // 写器在主流程分配并注册，主流程借此保留句柄做切分（10.3 第 6 步）。
        // 分配失败时不能让 JoinSet 直接 drop 掉已经启动的分块任务：先触发内部
        // 取消信号，再走与主循环相同的协作式排空，最后才返回错误。
        let pc = match spawner.open(&shared).await {
            Ok(pc) => pc,
            Err(e) => {
                internal.cancel();
                cooperative_drain(&mut join_set).await;
                return Err(e);
            }
        };
        acked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((init.db_index, Arc::clone(&shared.acked)));
        slots.push(ChunkSlot::new(Arc::clone(&shared), pc.clone(), idx));
        spawner.spawn(shared, pc, &mut join_set, idx);
        // 给外部取消一个插入点，使"分块全部启动之前取消"可被真实触发。
        tokio::task::yield_now().await;
    }

    // 路径 5：分块启动循环中取消。收尾会先跑最终检查点，再写 Paused(User)。
    if aborted_before_all_spawned || internal.is_cancelled() {
        internal.cancel();
        drain(&mut join_set).await;
        return Ok(ExitKind::Cancelled);
    }

    // Phase 2.2：连接空闲时定期重试切分。10.5 的「卡住」要等满一个速度窗口
    // （4 秒）才成立，只在"某个分块完成"时尝试一次是不够的；有空闲连接却暂时
    // 没有合法候选时，必须继续重试，否则被卡住的分块永远等不到接管。
    let mut split_retry = tokio::time::interval(Duration::from_millis(100));
    split_retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Phase 5b-4b：拆分阈值用有效并发（min(连接数, 每主机上限)），
    // 而非 inits.len()（= 连接数）。连接数大于主机上限时，后者会让
    // 被扣除排队块后的 in_flight 永远小于阈值，触发无休止拆分。
    // Phase 5b-4c：有效并发每次判断时通过 spawner.effective_concurrency() 动态读取。

    // 10.5 零进度接管：等待被接管分块释放写器之后才启动的新分块。
    let mut pending: Vec<PendingChunk> = Vec::new();

    // Phase 3d：在途连接数 = 仍在跑的分块任务 + 延迟分配 pending − 正在排队
    // 等 Host 名额的分块。排队分块不占连接槽位，也不会触发新的切分
    // （它们同时被 select_victim 排除）；退避中的分块仍计入，保持 10.5 行为。
    let in_flight = |slots: &[ChunkSlot],
                     join_set: &JoinSet<Result<ChunkResult>>,
                     pending: &[PendingChunk]|
     -> usize {
        in_flight_count(
            join_set.len(),
            pending.len(),
            &slots
                .iter()
                .map(|s| Arc::clone(s.shared()))
                .collect::<Vec<_>>(),
        )
    };

    let mut outcome: Option<Result<DownloadOutcome>> = None;
    loop {
        // pending 也占着一个连接名额，不能因为 join_set 暂时为空就收尾。
        if join_set.is_empty() && pending.is_empty() {
            break;
        }
        tokio::select! {
            biased;
            _ = internal.cancelled() => {
                outcome = Some(Ok(DownloadOutcome::Cancelled));
                break;
            }
            Some(joined) = join_set.join_next() => {
                // 这个分块的写器在 run_chunk 返回之前已经 release。
                let completed_db = match &joined {
                    Ok(Ok(ChunkResult::Completed { db_index })) => Some(*db_index),
                    _ => None,
                };
                match joined {
                    Ok(Ok(ChunkResult::Completed { .. })) => {
                        // 先启动已就绪的延迟分块，再考虑新的切分；pending 也计入
                        // 在途连接数，避免超过配置的连接上限。
                        if let Err(e) = process_pending_chunks(
                            &spawner,
                            &mut slots,
                            &acked,
                            &mut join_set,
                            &mut pending,
                            completed_db,
                            &tmp_path,
                        )
                        .await
                        {
                            internal.cancel();
                            outcome = Some(Err(e));
                            break;
                        }
                        if opts.config.dynamic_split
                            && !streaming
                            && in_flight(&slots, &join_set, &pending) < spawner.effective_concurrency()
                            && let Err(e) = try_split(
                                ctx,
                                &spawner,
                                &mut slots,
                                &acked,
                                &mut next_db_index,
                                &mut join_set,
                                &mut pending,
                            )
                            .await
                        {
                            internal.cancel();
                            outcome = Some(Err(e));
                            break;
                        }
                    }
                    Ok(Ok(ChunkResult::Cancelled)) => {}
                    Ok(Err(e)) => {
                        internal.cancel();
                        outcome = Some(if e.is_disk_full() {
                            Ok(DownloadOutcome::Paused(PauseReason::DiskFull))
                        } else {
                            Err(e)
                        });
                        break;
                    }
                    // abort_all 产生的 JoinError::Cancelled 属正常取消，不能当成 panic。
                    Err(e) if e.is_cancelled() => {}
                    Err(e) => {
                        internal.cancel();
                        outcome = Some(Err(Error::Io {
                            operation: "download chunk task panicked",
                            path: tmp_path.clone(),
                            offset: 0,
                            source: std::io::Error::other(format!("{e}")),
                        }));
                        break;
                    }
                }
            }
            // 有空闲连接时定期重试切分（10.5：卡住/明显慢是随时间的判定），
            // 同时检查延迟分配的 5 秒超时；两者都不阻塞主循环。
            _ = split_retry.tick() => {
                // Phase 5b-4b：把「等待 Host 许可」状态同步到进度快照，供 UI
                // 区分「排队」与「正在下载」。纯展示，不参与任何下载/拆分决策。
                {
                    let progress = spawner.progress.lock().await;
                    for slot in &slots {
                        progress.set_chunk_waiting(slot.idx(), slot.shared().waiting_for_permit());
                    }
                }
                if let Err(e) = process_pending_chunks(
                    &spawner,
                    &mut slots,
                    &acked,
                    &mut join_set,
                    &mut pending,
                    None,
                    &tmp_path,
                )
                .await
                {
                    internal.cancel();
                    outcome = Some(Err(e));
                    break;
                }
                if opts.config.dynamic_split
                    && !streaming
                    && in_flight(&slots, &join_set, &pending) < spawner.effective_concurrency()
                    && let Err(e) = try_split(
                        ctx,
                        &spawner,
                        &mut slots,
                        &acked,
                        &mut next_db_index,
                        &mut join_set,
                        &mut pending,
                    )
                    .await
                {
                    internal.cancel();
                    outcome = Some(Err(e));
                    break;
                }
            }
        }
    }

    // 协作式排空：等待剩余分块自行退出（它们会走到 pool.release 释放写器），最多 5 秒；
    // 超时（例如某分块卡死）才 abort_all 强制结束。
    cooperative_drain(&mut join_set).await;

    Ok(match outcome {
        Some(Ok(DownloadOutcome::Cancelled)) => ExitKind::Cancelled,
        Some(Ok(DownloadOutcome::Paused(_))) => ExitKind::PausedDiskFull,
        Some(Ok(DownloadOutcome::Completed)) => ExitKind::ReadyToFinalize,
        Some(Err(e)) => ExitKind::Failed(e),
        None => ExitKind::ReadyToFinalize,
    })
}

/// **唯一收尾函数**（1c.1 第一部分）。所有退出路径都经过这里，负责：
/// 1. 停止检查点任务，并 `await` 它真正结束（不能只调用 abort）；
/// 2. 停止外部取消的转发任务；
/// 3. 目标文件已创建时执行最后一次检查点（9.2），完成路径必须成功；
/// 4. 写入最终的数据库状态（Cancelled → Paused(User)、DiskFull → Paused(DiskFull)、
///    错误 → Failed；TaskInUse 不改状态）；
/// 5. 非 Completed 时调用 purge；Completed 走 9.4 完成阶段。
async fn finalize_download(
    pool: &DiskWriterPool,
    opts: &DownloadOptions,
    ctx: Option<&DownloadContext>,
    rt: &mut DownloadRuntime,
    inner: Result<ExitKind>,
) -> Result<(DownloadOutcome, watch::Receiver<ProgressSnapshot>)> {
    // 1. 停止检查点任务：先发停止信号，再 await 任务真正结束。
    //    只调用 abort 无法保证任务已经退出，它可能仍在持有 TargetFile 并写数据库。
    if let Some((handle, stop_tx)) = rt.checkpoint_task.take() {
        let _ = stop_tx.send(());
        let _ = handle.await;
    }
    // 2. 停止外部取消转发任务。
    if let Some(handle) = rt.forward_task.take() {
        handle.abort();
        let _ = handle.await;
    }

    let mut kind = match inner {
        Ok(k) => k,
        Err(e) => ExitKind::Failed(e),
    };

    // 3. 目标文件已创建 → 最后一次检查点。
    //    第二部分：完成路径上必须成功，否则不得进入 Finalizing。
    if let Some(target) = rt.target.as_deref() {
        if let Some(c) = ctx {
            let list = rt.acked.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if let Err(e) = run_checkpoint(c, pool, target, &list).await {
                warn!(error = %e, "final checkpoint abandoned; committed not advanced");
                if matches!(kind, ExitKind::ReadyToFinalize) {
                    kind = if e.is_disk_full() {
                        ExitKind::PausedDiskFull
                    } else {
                        ExitKind::Failed(e)
                    };
                }
            }
        } else {
            // 无持久化：保持 Phase 1b 的额外刷盘行为。
            let _ = pool.sync(target).await;
        }
    }

    // 失败/取消同样发布最终快照，否则 CLI 的分块汇总表拿到的是旧数据。
    if let Some(p) = &rt.progress {
        p.lock().await.publish_final();
    }
    let rx = rt
        .progress_rx
        .take()
        .unwrap_or_else(|| watch::channel(ProgressSnapshot::default()).1);

    let mut state = TaskState::Pending;
    advance(&mut state, TaskState::Probing);
    advance(&mut state, TaskState::Downloading);

    // 4. 写入最终的数据库状态。
    if let Some(c) = ctx {
        match &kind {
            ExitKind::CancelledBeforeCreate | ExitKind::Cancelled => {
                let _ = c
                    .store
                    .update_state(
                        c.task_id,
                        TaskStateDb::Paused,
                        Some(PauseReasonDb::User),
                        None,
                    )
                    .await;
                advance(&mut state, TaskState::Cancelled);
            }
            ExitKind::DiskFullBeforeCreate | ExitKind::PausedDiskFull => {
                let _ = c
                    .store
                    .update_state(
                        c.task_id,
                        TaskStateDb::Paused,
                        Some(PauseReasonDb::DiskFull),
                        None,
                    )
                    .await;
                advance(&mut state, TaskState::Paused);
            }
            ExitKind::Failed(e) => {
                // 9.5：TaskInUse 说明另一个进程正在下载，数据库状态必须保持不变。
                if !matches!(e, Error::TaskInUse { .. }) {
                    let _ = c
                        .store
                        .update_state(c.task_id, TaskStateDb::Failed, None, Some(format!("{e}")))
                        .await;
                }
                advance(&mut state, TaskState::Failed);
            }
            ExitKind::ReadyToFinalize => {}
        }
    }

    // 5a. 完成阶段（9.4）：最终检查点（上面已做且必须成功）→ Finalizing
    //     → 释放所有分块和文件句柄 → 不覆盖地重命名 → 记录最终文件名并改为 Completed。
    if matches!(kind, ExitKind::ReadyToFinalize) {
        if let Some(c) = ctx {
            c.store
                .update_state(c.task_id, TaskStateDb::Finalizing, None, None)
                .await?;
        }
        advance(&mut state, TaskState::Finalizing);
        if let Some(target) = rt.target.as_deref() {
            pool.sync(target).await?;
        }
        // 检查点任务已 await 结束，这里持有最后一个 Arc<TargetFile>；drop 后
        // 句柄全部释放。共享模式不允许删除，只要还有句柄，改名就会失败。
        rt.target.take();

        let recorded = rt.final_path.as_deref().unwrap_or(opts.dir.as_path());
        let desired = recorded
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        let base = recorded.parent().unwrap_or(opts.dir.as_path());
        let final_candidate = base.join(&desired);
        let final_path = if final_candidate.exists() {
            resolve_conflict(base, &desired)
        } else {
            final_candidate
        };
        let tmp_path = rt.tmp_path.clone().unwrap_or_default();
        win32::move_file(&tmp_path, &final_path).map_err(|e| Error::Io {
            operation: "rename (move_file)",
            path: final_path.clone(),
            offset: 0,
            source: std::io::Error::other(format!("{e}")),
        })?;
        if let Some(c) = ctx {
            let name = final_path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            c.store.set_final_name(c.task_id, &name).await?;
            // Phase 4a：任务已完成，清除其请求头（Cookie 不得长期留在数据库）。
            let _ = c.store.clear_task_headers(c.task_id).await;
        }
        advance(&mut state, TaskState::Completed);
        info!(task_id = rt.task_id, state = ?state, "download completed");
        return Ok((DownloadOutcome::Completed, rx));
    }

    // 5b. 非 Completed：purge 兜底清理可能残留的写入器（panic/泄露时）。
    if let Some(target) = rt.target.as_deref() {
        purge_if_needed(pool, target, rt.task_id).await;
        let _ = pool.sync(target).await;
    }
    info!(task_id = rt.task_id, state = ?state, "download stopped");
    match kind {
        ExitKind::CancelledBeforeCreate | ExitKind::Cancelled => {
            Ok((DownloadOutcome::Cancelled, rx))
        }
        ExitKind::DiskFullBeforeCreate | ExitKind::PausedDiskFull => {
            Ok((DownloadOutcome::Paused(PauseReason::DiskFull), rx))
        }
        ExitKind::Failed(e) => {
            info!(task_id = rt.task_id, error = %e, "download failed");
            Err(e)
        }
        ExitKind::ReadyToFinalize => Ok((DownloadOutcome::Completed, rx)),
    }
}

/// 非 Completed 结束时，兜底清理可能残留的写入器；移除数量大于 0 时输出 warn 日志。
async fn purge_if_needed(pool: &DiskWriterPool, target: &TargetFile, task_id: u64) {
    match pool.purge(target).await {
        Ok(n) if n > 0 => warn!(
            task_id,
            removed = n,
            "purged residual writers after non-completed download"
        ),
        _ => {}
    }
}

/// 分块任务的可克隆启动器（Phase 2：分块可以在运行中动态增加）。
///
/// `task::split` 在切分新分块时需要使用 `pool` / `progress` / `task_id` 以及
/// [`ChunkSpawner::open`] / [`ChunkSpawner::spawn`]，因此这些成员对 `task` 可见。
#[derive(Clone)]
pub(super) struct ChunkSpawner {
    client: Client,
    url: Url,
    pub(super) pool: Arc<DiskWriterPool>,
    target: Arc<TargetFile>,
    pub(super) progress: Arc<Mutex<ProgressPublisher>>,
    if_range: Option<String>,
    verify_etag: Option<(String, bool)>,
    bandwidth: BandwidthLimiter,
    host_limiter: Arc<HostConnectionLimiter>,
    cancel: CancelSignal,
    cancel_rx: watch::Receiver<bool>,
    pub(super) task_id: u64,
    streaming: bool,
    max_retries: u32,
    backoff_base: Duration,
    backoff_cap: Duration,
    /// 普通对半切的最小分块大小（放进启动器，避免 try_split 参数过多）。
    pub(super) min_split: u64,
    /// 任务连接数。与 limiter 当前上限取小得到有效并发（5b-4c 动态计算）。
    connections: usize,
}

impl ChunkSpawner {
    /// Phase 5b-4c：有效并发 = min(任务连接数, limiter **当前**上限)，至少 1。
    /// 运行中上限可被 `set_max_per_host` 改变，故每次判断都重新读取，不缓存。
    pub(super) fn effective_concurrency(&self) -> usize {
        self.connections
            .max(1)
            .min(self.host_limiter.max_per_host())
            .max(1)
    }

    /// Phase 5b-4c：分块总数硬上限 = max(4 × 当前有效并发, 16)，动态计算。
    pub(super) fn max_chunks(&self) -> usize {
        (4 * self.effective_concurrency()).max(16)
    }

    /// 分配写器并注册到线程池。切分时必须**先** shrink 旧的、**再**分配新的，
    /// 否则两个写器的区间会重叠。
    pub(super) async fn open(&self, shared: &ChunkShared) -> Result<PoolChunk> {
        let chunk_id = shared.db_index as u64;
        let writer = if self.streaming {
            self.target
                .allocate_unbounded_chunk(chunk_id, shared.start)?
        } else {
            self.target
                .allocate_chunk(chunk_id, shared.start, shared.end())?
        };
        self.pool.register(writer).await
    }

    /// 把分块任务投入 JoinSet。
    pub(super) fn spawn(
        &self,
        shared: Arc<ChunkShared>,
        pc: PoolChunk,
        join_set: &mut JoinSet<Result<ChunkResult>>,
        idx: usize,
    ) {
        let job = ChunkJob {
            client: self.client.clone(),
            url: self.url.clone(),
            pool: Arc::clone(&self.pool),
            chunk_id: shared.db_index as u64,
            shared: Arc::clone(&shared),
            if_range: self.if_range.clone(),
            verify_etag: self.verify_etag.clone(),
            progress: Arc::clone(&self.progress),
            idx,
            streaming: self.streaming,
            cancel: self.cancel.clone(),
            cancel_rx: self.cancel_rx.clone(),
            bandwidth: self.bandwidth.clone(),
            host_limiter: Arc::clone(&self.host_limiter),
            task_id: self.task_id,
            received: Arc::new(AtomicU64::new(0)),
            acked: Arc::clone(&shared.acked),
            max_retries: self.max_retries,
            backoff_base: self.backoff_base,
            backoff_cap: self.backoff_cap,
        };
        join_set.spawn(run_chunk(job, pc));
    }
}

/// 一个分块任务的最终结果。
///
/// `Completed` 带数据库编号：10.5 零进度接管的延迟分配需要知道"被接管分块"
/// 的任务何时结束（它结束后才会 `release` 写器）。
pub(super) enum ChunkResult {
    Completed { db_index: i64 },
    Cancelled,
}

/// `run_chunk_inner` 的退出原因（写入位置由外层统一 release 后校验）。
enum ChunkExit {
    Completed,
    Cancelled,
}

/// 单个分块任务的全部输入。共享句柄均为 `Arc`/`Clone`，按值传入任务。
struct ChunkJob {
    client: Client,
    url: Url,
    pool: Arc<DiskWriterPool>,
    chunk_id: u64,
    /// 共享状态：起点不变，终点可能被切分缩小（Phase 2）。
    shared: Arc<ChunkShared>,
    if_range: Option<String>,
    verify_etag: Option<(String, bool)>,
    progress: Arc<Mutex<ProgressPublisher>>,
    idx: usize,
    streaming: bool,
    cancel: CancelSignal,
    cancel_rx: watch::Receiver<bool>,
    bandwidth: BandwidthLimiter,
    host_limiter: Arc<HostConnectionLimiter>,
    task_id: u64,
    received: Arc<AtomicU64>,
    /// 只在 pool.write 成功返回后推进的绝对写入位置（检查点快照来源）。
    acked: Arc<AtomicU64>,
    /// 重试参数（Phase 1d）：连续失败上限与退避基准/封顶。
    max_retries: u32,
    backoff_base: Duration,
    backoff_cap: Duration,
}

/// 单个分块的下载任务：请求 → 读取 → 经池写入 → 统一 release。
///
/// 写器由 [`ChunkSpawner::open`] 提前分配并注册（主流程需要保留句柄做切分），
/// 这里直接接收。
async fn run_chunk(job: ChunkJob, pc: PoolChunk) -> Result<ChunkResult> {
    let task_id = job.task_id;
    let chunk_id = job.chunk_id;
    let start = job.shared.start;
    let idx = job.idx;
    let streaming = job.streaming;
    let received_counter = job.received.clone();
    let pool = job.pool.clone();
    let progress = job.progress.clone();

    info!(
        task_id,
        chunk_id,
        start,
        end = job.shared.end(),
        received = 0u64,
        "chunk started"
    );

    let mut cs = next_chunk_state(ChunkState::Pending, ChunkState::Downloading);
    progress.lock().await.set_chunk_state(idx, cs);

    let chunk_started = Instant::now();
    let shared = Arc::clone(&job.shared);
    let inner = run_chunk_inner(job, &pc).await;
    // 被切分时 end 已经缩小，收尾校验必须用最新的 end。
    let end = shared.end();
    let elapsed_secs = chunk_started.elapsed().as_secs_f64();
    // 无论成功、失败还是取消，都统一 release，避免文件句柄在 worker 线程中残留。
    let released = pool.release(pc).await;
    let received = received_counter.load(Ordering::Relaxed);

    // 分块汇总表数据：已写入字节数取进度快照的 current - start（该位置只在
    // pool.write 成功返回后才推进）；耗时为本分块从开始到读取结束的墙钟时间。
    let written = {
        let p = progress.lock().await;
        let snap = p.snapshot();
        let guard = snap.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .chunks
            .get(idx)
            .map_or(0, |c| c.current.saturating_sub(c.start))
    };
    let avg_speed = if elapsed_secs > 0.0 {
        written as f64 / elapsed_secs
    } else {
        0.0
    };
    progress
        .lock()
        .await
        .set_chunk_timing(idx, elapsed_secs, avg_speed);

    match inner {
        Ok(ChunkExit::Completed) => {
            let final_pos = released?;
            if !streaming && final_pos != end {
                cs = next_chunk_state(cs, ChunkState::Failed);
                progress.lock().await.set_chunk_state(idx, cs);
                info!(task_id, chunk_id, start, end, received, "chunk failed");
                return Err(Error::UnexpectedEof {
                    chunk_id,
                    expected: end,
                    received: final_pos,
                });
            }
            cs = next_chunk_state(cs, ChunkState::Done);
            progress.lock().await.set_chunk_state(idx, cs);
            info!(task_id, chunk_id, start, end, received, "chunk finished");
            Ok(ChunkResult::Completed {
                db_index: chunk_id as i64,
            })
        }
        Ok(ChunkExit::Cancelled) => {
            cs = next_chunk_state(cs, ChunkState::Cancelled);
            progress.lock().await.set_chunk_state(idx, cs);
            info!(task_id, chunk_id, start, end, received, "chunk cancelled");
            Ok(ChunkResult::Cancelled)
        }
        Err(e) => {
            cs = next_chunk_state(cs, ChunkState::Failed);
            progress.lock().await.set_chunk_state(idx, cs);
            info!(task_id, chunk_id, start, end, received, error = %e, "chunk failed");
            Err(e)
        }
    }
}

/// 单次尝试的错误：携带 429/503 响应指定的 `Retry-After`（若有）。
struct AttemptError {
    error: Error,
    retry_after: Option<Duration>,
}

impl From<Error> for AttemptError {
    fn from(error: Error) -> Self {
        AttemptError {
            error,
            retry_after: None,
        }
    }
}

/// 10.5 唤醒：等待 end 被缩小的通知（状态语义，订阅之后发生的缩小不会漏掉）。
///
/// 被唤醒后加锁读取 end：`acked == end` 时返回 `Some(Completed)`，调用方立即
/// 丢弃响应（关闭连接）或停止退避等待；否则返回 `None`，继续原来的等待。
async fn wake_on_shrink(
    end_rx: &mut watch::Receiver<u64>,
    shared: &ChunkShared,
    acked: &AtomicU64,
) -> Option<ChunkExit> {
    if end_rx.changed().await.is_err() {
        // 发送端随 ChunkShared 一同存活，这里不会发生；防御性让出一次，避免自旋。
        tokio::task::yield_now().await;
        return None;
    }
    let end = shared.end();
    if acked.load(Ordering::Acquire) >= end {
        Some(ChunkExit::Completed)
    } else {
        None
    }
}

/// Phase 3d：申请 Host 名额，等待期间与「取消」和 10.5 的「end 缩小」并列监听。
///
/// `acquire_future` 只创建一次并跨 `select!` 轮次复用：被 end 缩小唤醒但
/// `acked < end` 时继续等**同一个**申请，不会把分块重新排到 Semaphore 公平队列的
/// 队尾。被接管到 `acked >= end` 时返回 `Ok(None)`——丢弃等待、不占名额，调用方
/// 立即以 `Completed` 结束（零进度接管等它 5 秒的场景由此不再超时失败）。
async fn acquire_with_shrink(
    limiter: &HostConnectionLimiter,
    url: &Url,
    cancel: &CancelSignal,
    shared: &ChunkShared,
    acked: &AtomicU64,
    end_rx: &mut watch::Receiver<u64>,
) -> Result<Option<HostPermit>> {
    // 排队等名额期间标记自己，供主循环把排队分块从在途连接数中扣除。
    shared.mark_waiting_for_permit();
    let _waiting = WaitingForPermitGuard(shared);
    let mut permit_fut = limiter.acquire_future(url)?;
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            exit = wake_on_shrink(end_rx, shared, acked) => {
                if exit.is_some() {
                    return Ok(None);
                }
            }
            p = &mut permit_fut => return p.map(Some),
        }
    }
}

/// 离开名额申请（拿到名额 / 被接管唤醒 / 取消 / 任务被 abort）时清除排队标记。
struct WaitingForPermitGuard<'a>(&'a ChunkShared);

impl Drop for WaitingForPermitGuard<'_> {
    fn drop(&mut self) {
        self.0.clear_waiting_for_permit();
    }
}

/// 请求结束时复位 `holds_permit` 的 RAII 守卫：无论完成、出错、被取消还是被接管
/// 提前关闭，`attempt_chunk_once` 返回时都会释放「持有名额」标记。
/// `eligible_for_split` 不复位——退避中的分块仍可被接管（10.5）。
struct HeldPermit<'a> {
    _permit: HostPermit,
    shared: &'a ChunkShared,
}

impl Drop for HeldPermit<'_> {
    fn drop(&mut self) {
        self.shared.release_permit();
    }
}

/// 分块主体（Phase 1d 重试循环）：每一步网络读取与磁盘写入之前都检查取消。
/// 分块请求**不**设总时长超时（依赖 Client 级别的 `connect_timeout` 与
/// `read_timeout`），避免大分块在慢速网络下失败。
///
/// 出错后按 [`crate::net::retry::is_retryable`] 分类：可重试的错误从当前写入位置
/// （acked，等于 ChunkWriter 的当前位置）重新发送 Range 请求直到区间终点；
/// ChunkWriter 保持注册，不注销也不重新申请，绝不从分块起点重新下载。
/// 退避为指数加 ±20% 抖动（默认 1s 起翻倍、封顶 30s）；429/503 优先采用
/// `Retry-After`（上限 60s）。连续失败达 `max_retries`（单次写入 ≥1 MiB 清零）
/// 或总尝试次数达 [`MAX_TOTAL_ATTEMPTS`] 后，以 [`Error::RetryExhausted`] 失败。
/// 退避等待期间持续监听取消信号。限速器在重试间保持不变。
/// 流式（总长未知）模式无法安全续传（服务器不支持 Range，重试只能从头下载），
/// 这类任务本就标记为不可续传，因此禁用重试。
async fn run_chunk_inner(job: ChunkJob, pc: &PoolChunk) -> Result<ChunkExit> {
    let mut cancel_rx = job.cancel_rx.clone();
    // 流式模式：见上，禁用重试。
    let max_retries = if job.streaming { 0 } else { job.max_retries };

    let mut rng = XorShift64::from_now();
    let mut consecutive_failures: u32 = 0;
    let mut retries: u64 = 0;
    let mut total_attempts: u32 = 0;

    loop {
        total_attempts += 1;
        // 本次尝试起点：上次成功写入的位置（首次调用时等于分块 start；
        // 续传/resume 场景由调用方保证 acked 初始值即起点）。
        let cur = job.acked.load(Ordering::Acquire);
        match attempt_chunk_once(&job, pc, cur, &mut cancel_rx).await {
            Ok(ChunkExit::Completed) => return Ok(ChunkExit::Completed),
            Ok(ChunkExit::Cancelled) => return Ok(ChunkExit::Cancelled),
            Err(attempt) => {
                // 错误发生时若外部已取消，按取消收尾（保留已写入的数据）。
                if rx_cancelled(&cancel_rx) {
                    return Ok(ChunkExit::Cancelled);
                }
                // Phase 3a 边界保护：最后一次在途写盘完成后 end 被接管缩到 acked，
                // 此时网络侧恰好断开也不得进入重试退避或报错路径。
                if !job.streaming && job.acked.load(Ordering::Acquire) >= job.shared.end() {
                    return Ok(ChunkExit::Completed);
                }
                // 不可重试的错误（磁盘错误、RemoteChanged 等）原样返回。
                if !is_retryable(&attempt.error) {
                    return Err(attempt.error);
                }
                // 禁用重试（max_retries = 0，含 streaming 模式强制置 0）时，
                // 首次错误原样返回，不包装成 RetryExhausted。
                if max_retries == 0 {
                    return Err(attempt.error);
                }
                // 单次尝试写入 ≥1 MiB → 清零连续失败计数（有实质进展）。
                let wrote = job.acked.load(Ordering::Acquire).saturating_sub(cur);
                if wrote >= PROGRESS_RESET_BYTES {
                    consecutive_failures = 0;
                }
                if consecutive_failures >= max_retries || total_attempts >= MAX_TOTAL_ATTEMPTS {
                    warn!(
                        task_id = job.task_id,
                        chunk_id = job.chunk_id,
                        attempts = total_attempts,
                        error = %attempt.error,
                        "chunk retries exhausted"
                    );
                    return Err(Error::RetryExhausted {
                        chunk_id: job.chunk_id,
                        attempts: total_attempts,
                        last_error: Box::new(attempt.error),
                    });
                }
                // 429/503 且带 Retry-After → 按该值等待（解析时已封顶 60s）；
                // 否则指数退避 + 抖动。
                let wait = match attempt.retry_after {
                    Some(d) => d,
                    None => backoff_delay(
                        consecutive_failures,
                        job.backoff_base,
                        job.backoff_cap,
                        &mut rng,
                    ),
                };
                let resume_from = job.acked.load(Ordering::Acquire);
                warn!(
                    task_id = job.task_id,
                    chunk_id = job.chunk_id,
                    attempt = total_attempts + 1,
                    error = %attempt.error,
                    wait_ms = wait.as_millis() as u64,
                    resume_from,
                    "chunk retry"
                );
                // 退避等待期间监听取消信号与 10.5 的 end 变更：被接管到 acked
                // 时立刻以 Completed 退出；否则继续等完原本的退避时长。
                let mut end_rx = job.shared.subscribe_end();
                let deadline = Instant::now() + wait;
                loop {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    tokio::select! {
                        biased;
                        _ = wait_cancelled(&mut cancel_rx) => return Ok(ChunkExit::Cancelled),
                        exit = wake_on_shrink(&mut end_rx, &job.shared, &job.acked) => {
                            if let Some(exit) = exit {
                                return Ok(exit);
                            }
                        }
                        _ = tokio::time::sleep(remaining) => break,
                    }
                }
                consecutive_failures += 1;
                retries += 1;
                job.progress
                    .lock()
                    .await
                    .set_chunk_retries(job.idx, retries);
            }
        }
    }
}

/// 单次尝试：从 `cur` 到 `end` 发送一次 Range 请求并写盘。
/// 每次尝试都带上与首次相同的 If-Range 与 ETag 校验规则。
async fn attempt_chunk_once(
    job: &ChunkJob,
    pc: &PoolChunk,
    cur: u64,
    cancel_rx: &mut watch::Receiver<bool>,
) -> std::result::Result<ChunkExit, AttemptError> {
    let ChunkJob {
        client,
        url,
        pool,
        chunk_id,
        shared,
        if_range,
        verify_etag,
        progress,
        idx,
        streaming,
        bandwidth,
        host_limiter,
        cancel,
        task_id,
        received: received_counter,
        acked: acked_counter,
        ..
    } = job;
    // 解构 `&ChunkJob`（match ergonomics）得到的是各字段的引用；
    // Copy 字段在此解引用，供后续直接使用。
    let (chunk_id, idx, streaming, task_id) = (*chunk_id, *idx, *streaming, *task_id);
    // 订阅 end 变更必须在读取 end 之前：若中间发生切分，通知仍会被观察到。
    let mut end_rx = shared.subscribe_end();
    // 本次尝试的区间终点：可能已经被切分缩小（Phase 2 / 10.5 接管）。
    let end = shared.end();
    // 本函数沿用原实现的变量名：start 即本次尝试的起点 cur。
    let start = cur;
    let expected = end.saturating_sub(start);

    if rx_cancelled(cancel_rx) {
        return Ok(ChunkExit::Cancelled);
    }
    // Phase 2.2 剩余长度为 0 的保护：当前位置已经到达（或被切分缩小到）end，
    // 直接以完成结束。分块下载过程中绝不允许发出起点等于终点的 Range 请求。
    if start >= end {
        return Ok(ChunkExit::Completed);
    }

    // Phase 3d：每次尝试（含重试）都重新申请 Host 名额；等待期间与取消、10.5 的
    // end 缩小并列监听，且保持 Semaphore 公平队列中的位置。被接管到 acked 时
    // 立即以 Completed 结束：不发请求、不占名额、不再排队。
    let permit = match acquire_with_shrink(
        host_limiter,
        url,
        cancel,
        shared.as_ref(),
        acked_counter,
        &mut end_rx,
    )
    .await
    {
        Ok(Some(permit)) => permit,
        Ok(None) => return Ok(ChunkExit::Completed),
        Err(Error::Cancelled) => return Ok(ChunkExit::Cancelled),
        Err(e) => return Err(e.into()),
    };
    // 拿到名额、`req.send()` 即将开始：置位 holds_permit 与 eligible_for_split，
    // 并把速度采样 / is_fresh 的计时起点重置到此刻。守卫在函数任何返回路径上
    // 复位 holds_permit（退避期间不占在途连接数）。
    shared.mark_request_started();
    let _held = HeldPermit {
        _permit: permit,
        shared: shared.as_ref(),
    };

    // 流式（总长未知）模式不发送 Range；分块模式发送精确区间。
    let mut req = client.get(url.clone());
    if !streaming {
        let range_hdr = format!("bytes={}-{}", start, end - 1);
        req = req.header(RANGE, range_hdr);
        if let Some(ir) = &if_range {
            req = req.header(IF_RANGE, ir.clone());
        }
    }

    // 10.5（Phase 2.2.1）：等待服务器返回响应头时同时监听取消与 end 缩小。
    // 零进度接管的原分块正卡在这里；被唤醒且 acked == end 时丢弃本次请求
    // （drop 掉 send future 即关闭连接），以 Completed 结束。
    // send future 只创建一次，唤醒但尚未完成时可以继续等它，不会重发请求。
    let mut send_fut = std::pin::pin!(req.send());
    let mut resp = loop {
        tokio::select! {
            biased;
            _ = wait_cancelled(cancel_rx) => return Ok(ChunkExit::Cancelled),
            exit = wake_on_shrink(&mut end_rx, shared, acked_counter) => {
                if let Some(exit) = exit {
                    return Ok(exit);
                }
            }
            r = &mut send_fut => {
                break r.map_err(|e| Error::Http { url: url.to_string(), source: e })?;
            }
        }
    };
    // 429/503 的 Retry-After 必须在产生错误之前从响应头读取。
    let status = resp.status();
    let retry_after: Option<Duration> = if matches!(status.as_u16(), 429 | 503) {
        resp.headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after)
    } else {
        None
    };
    resp.error_for_status_ref().map_err(|e| AttemptError {
        error: Error::Http {
            url: url.to_string(),
            source: e,
        },
        retry_after,
    })?;

    // 非 identity 的 Content-Encoding → 绝不把压缩字节当文件内容。
    if let Some(ce) = resp.headers().get(CONTENT_ENCODING) {
        let ce = ce.to_str().unwrap_or("").trim().to_ascii_lowercase();
        if ce != "identity" {
            return Err(Error::UnsupportedContentEncoding { encoding: ce }.into());
        }
    }

    match status {
        StatusCode::PARTIAL_CONTENT if !streaming => {
            let cr_val = resp
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| Error::InvalidContentRange {
                    requested: (start, end),
                    actual: "(missing Content-Range)".to_string(),
                })?;
            let cr = parse_content_range(cr_val).map_err(|_| Error::InvalidContentRange {
                requested: (start, end),
                actual: cr_val.to_string(),
            })?;
            if cr.start != start || cr.end != end - 1 {
                return Err(Error::InvalidContentRange {
                    requested: (start, end),
                    actual: cr_val.to_string(),
                }
                .into());
            }
            // 弱 ETag 场景：未发 If-Range，需校验响应 ETag 与 Probe 时一致。
            if verify_etag.as_ref().map(|(_, w)| *w).unwrap_or(false)
                && let Some(p_etag) = &verify_etag
            {
                let r_etag = resp
                    .headers()
                    .get(ETAG)
                    .and_then(|v| v.to_str().ok())
                    .map(str::trim);
                match r_etag {
                    Some(r) if r == p_etag.0.trim() => {}
                    _ => return Err(Error::RemoteChanged.into()),
                }
            }
        }
        StatusCode::OK if streaming => {}
        StatusCode::OK if !streaming => {
            if if_range.is_some() {
                return Err(Error::RemoteChanged.into());
            }
            return Err(Error::RangeNotHonored.into());
        }
        _ => {
            return Err(Error::InvalidContentRange {
                requested: (start, end),
                actual: status.to_string(),
            }
            .into());
        }
    }

    // 读取响应体并写入；网络 Bytes 零拷贝直接交给线程池。
    let mut received: u64 = 0;
    loop {
        // 读网络之前检查取消。
        if rx_cancelled(cancel_rx) {
            return Ok(ChunkExit::Cancelled);
        }
        let bytes = tokio::select! {
            biased;
            _ = wait_cancelled(cancel_rx) => return Ok(ChunkExit::Cancelled),
            // 10.5：等待网络数据时也要监听 end 缩小。被接管到 acked 时立即
            // 丢弃响应（resp 随本函数返回而 drop，连接随之关闭），以完成结束。
            exit = wake_on_shrink(&mut end_rx, shared, acked_counter) => {
                if let Some(exit) = exit {
                    return Ok(exit);
                }
                continue;
            }
            r = resp.chunk() => match r {
                Ok(Some(b)) => b,
                Ok(None) => break,
                Err(e) => {
                    // Phase 3a 边界保护：接管已经把 end 缩到 acked 时，网络侧
                    // 刚好断开也不能误走重试 / 报错，必须直接以完成结束。
                    if !streaming
                        && acked_counter.load(Ordering::Acquire) >= shared.end()
                    {
                        return Ok(ChunkExit::Completed);
                    }
                    // 读取空闲超时（read_timeout）属于超时相关错误，必须如实返回，
                    // 不能误判为"响应体被截断"。
                    if e.is_timeout() {
                        return Err(Error::Http { url: url.to_string(), source: e }.into());
                    }
                    // 响应体被中途截断时按"提前结束"处理，而不是笼统的 HTTP 错误。
                    if !streaming && received < expected {
                        received_counter.store(start + received, Ordering::Relaxed);
                        return Err(Error::UnexpectedEof { chunk_id, expected, received }.into());
                    }
                    return Err(Error::Http { url: url.to_string(), source: e }.into());
                }
            },
        };
        let len = bytes.len() as u64;
        if !streaming && received + len > expected {
            return Err(Error::InvalidContentRange {
                requested: (start, end),
                actual: format!("received more than {expected} bytes"),
            }
            .into());
        }
        if len > 0 {
            // 限速：读到一块数据之后、交给写入线程池之前领取配额。等待被延后后，
            // TCP 接收窗口填满，服务器端随之放慢发送，从而真正限制网络流量。
            // acquire 与取消信号并列，等待令牌时也能立刻响应取消。
            // 重试后继续使用同一个限速器。
            if !bandwidth.is_unlimited() {
                tokio::select! {
                    biased;
                    _ = wait_cancelled(cancel_rx) => return Ok(ChunkExit::Cancelled),
                    _ = bandwidth.acquire(len) => {}
                }
            }
            // 写磁盘之前检查取消。
            if rx_cancelled(cancel_rx) {
                return Ok(ChunkExit::Cancelled);
            }
            // 10.2 第 1 步：锁内读取 end、预留可写长度，锁外写入。
            let writable = shared.reserve(len);
            if writable == 0 {
                // 10.2 第 2 步：区间已被缩小到当前位置 → 丢弃剩余数据，主动关闭连接，
                // 分块以"完成"结束（不再校验 received 是否等于 expected）。
                trace!(
                    task_id,
                    chunk_id, "chunk range shrunk to current position; closing connection"
                );
                return Ok(ChunkExit::Completed);
            }
            // 10.2 第 3 步：只写入可写长度部分，超出 end 的数据截掉丢弃。
            let data = if writable == len {
                bytes
            } else {
                bytes.slice(..writable as usize)
            };
            trace!(task_id, chunk_id, len = writable, "chunk data");
            if let Err(e) = pool.write(pc, data).await {
                // 10.2 第 4 步：写入失败时把 reserved 退回到 acked，再按 1d 规则重试。
                shared.rollback_reserved();
                return Err(e.into());
            }
            // 只有落盘线程池返回成功回执之后才推进 acked。
            // Release 与检查点快照的 Acquire 配对：保证检查点读到这个位置时，
            // pool.write 的成功回执（以及写入本身）已经完成，绝不把尚未落盘
            // 的数据记成 committed。
            acked_counter.fetch_add(writable, Ordering::Release);
            shared.record(start + received + writable);
            received += writable;
            received_counter.store(start + received, Ordering::Relaxed);
            let mut p = progress.lock().await;
            p.set_chunk_current(idx, start + received);
            p.publish_if_due();
            drop(p);
            // 10.5：接管时 end 可能恰好缩到本次写盘的末尾，而唤醒发生在
            // `pool.write` 尚未返回（acked 还落后于 reserved）的窗口里。
            // 回执到达后再确认一次，避免继续等一个永远不来的网络数据块。
            if !streaming && acked_counter.load(Ordering::Acquire) >= shared.end() {
                return Ok(ChunkExit::Completed);
            }
        }
    }
    received_counter.store(start + received, Ordering::Relaxed);

    if rx_cancelled(cancel_rx) {
        return Ok(ChunkExit::Cancelled);
    }
    // Phase 3a 边界保护：响应体提前结束时，若接管已把 end 缩到 acked，
    // 数据其实已经完整，必须直接以完成结束而不是 UnexpectedEof。
    if !streaming && acked_counter.load(Ordering::Acquire) >= shared.end() {
        return Ok(ChunkExit::Completed);
    }
    if !streaming && received != expected {
        return Err(Error::UnexpectedEof {
            chunk_id,
            expected,
            received,
        }
        .into());
    }
    Ok(ChunkExit::Completed)
}

/// 把 `total` 均匀切成至多 `connections` 个分块（各分块尽量均匀）。
fn chunk_ranges(total: u64, connections: usize, min_chunk: u64) -> Vec<(u64, u64)> {
    let min_chunk = min_chunk.max(1);
    if total == 0 {
        return Vec::new();
    }
    let connections = (connections.max(1) as u64).min(total);
    let num = connections.min(total.div_ceil(min_chunk)).max(1);
    let base = total / num;
    let rem = total % num;
    let mut ranges = Vec::with_capacity(num as usize);
    let mut start = 0u64;
    for i in 0..num {
        let extra = if i < rem { 1 } else { 0 };
        let size = base + extra;
        if size == 0 {
            continue;
        }
        ranges.push((start, start + size));
        start += size;
    }
    if ranges.is_empty() {
        ranges.push((0, total));
    }
    ranges
}

/// 等待 JoinSet 中剩余任务结束（用于 abort 后排空）。
async fn drain(set: &mut JoinSet<Result<ChunkResult>>) {
    while set.join_next().await.is_some() {}
}

/// 协作式排空：调用方先触发内部取消信号，本函数再等待剩余分块任务自行退出
/// （它们会走到 `pool.release` 释放写器），最多 5 秒；超时才 `abort_all`。
///
/// 主循环结束与"初始分块启动循环中 `spawner.open` 失败"这两条错误路径共用它，
/// 后者不能让已经启动的任务随着 `JoinSet` 被 drop 而强行中止。
async fn cooperative_drain(set: &mut JoinSet<Result<ChunkResult>>) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !set.is_empty() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, set.join_next()).await {
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(_) => {
                set.abort_all();
                drain(set).await;
                break;
            }
        }
    }
}

/// 10.5 零进度接管的延迟第 7 步：被接管分块以 `Completed` 结束（写器已
/// `release`）之后，立刻为新分块分配写器并启动连接。超过 5 秒仍未结束属于
/// 程序错误。主循环在每次事件后调用本函数，**不等待、不阻塞**其它事件。
async fn process_pending_chunks(
    spawner: &ChunkSpawner,
    slots: &mut Vec<ChunkSlot>,
    acked: &AckedList,
    join_set: &mut JoinSet<Result<ChunkResult>>,
    pending: &mut Vec<PendingChunk>,
    completed: Option<i64>,
    tmp_path: &Path,
) -> Result<()> {
    let now = Instant::now();
    let mut i = 0usize;
    while i < pending.len() {
        if completed == Some(pending[i].victim_db_index) {
            let p = pending.remove(i);
            launch_new_chunk(spawner, slots, acked, join_set, p.shared, p.init).await?;
            continue;
        }
        if now >= pending[i].deadline {
            return Err(Error::Io {
                operation: "zero-progress takeover victim did not finish within 5s",
                path: tmp_path.to_path_buf(),
                offset: 0,
                source: std::io::Error::other("takeover victim did not release its writer"),
            });
        }
        i += 1;
    }
    Ok(())
}

/// 同步读取某个分块接收端的取消状态（`watch` 保存状态，不会漏掉）。
fn rx_cancelled(rx: &watch::Receiver<bool>) -> bool {
    *rx.borrow()
}

/// 经状态机受控转换推进任务状态；内部转换均合法，非法时保持原状态。
fn advance(state: &mut TaskState, to: TaskState) {
    if let Ok(next) = state.transition(to) {
        *state = next;
    }
}

/// 经状态机受控转换推进分块状态。
fn next_chunk_state(cur: ChunkState, to: ChunkState) -> ChunkState {
    cur.transition(to).unwrap_or(cur)
}

/// `<最终文件名>.idm-part`。
fn part_path(final_path: &Path) -> PathBuf {
    let name = final_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    final_path.with_file_name(format!("{name}.idm-part"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Phase 2.1 第四部分：协作式排空必须等待已启动的分块任务**自行退出**，
    /// 而不是让它们随着 `JoinSet` 被 drop 而强行中止。
    #[tokio::test]
    async fn cooperative_drain_waits_for_started_tasks() {
        let mut set: JoinSet<Result<ChunkResult>> = JoinSet::new();
        let exited = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&exited);
        set.spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            flag.store(true, Ordering::SeqCst);
            Ok(ChunkResult::Completed { db_index: 0 })
        });
        cooperative_drain(&mut set).await;
        assert!(exited.load(Ordering::SeqCst), "任务必须有机会正常退出");
        assert!(set.is_empty(), "排空之后 JoinSet 必须为空");
    }

    /// Phase 2.2 第三部分第 2 项：分块正在等待网络数据时，end 被接管缩小到
    /// acked，唤醒必须在 100 毫秒内让任务以 Completed 结束（丢弃连接）。
    #[tokio::test]
    async fn wake_on_shrink_returns_completed_within_100ms() {
        let shared = Arc::new(ChunkShared::new(0, 4 * 1024 * 1024, 0));
        let mut end_rx = shared.subscribe_end();
        let acked = Arc::clone(&shared.acked);
        let shrunk = Arc::clone(&shared);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            // 接管：reserved == split == acked == 0，确认条件放宽为 <=。
            assert!(shrunk.commit_split(0, 4 * 1024 * 1024, true));
        });

        let started = Instant::now();
        // 与 attempt_chunk_once 中"等待 resp.chunk()"同样的 select! 结构，
        // 只是把网络 future 换成永远 pending，模拟连接完全卡住。
        let exit = tokio::time::timeout(Duration::from_millis(100), async {
            loop {
                tokio::select! {
                    biased;
                    woken = wake_on_shrink(&mut end_rx, &shared, &acked) => {
                        if let Some(exit) = woken {
                            return exit;
                        }
                    }
                    _ = std::future::pending::<()>() => {}
                }
            }
        })
        .await;

        assert!(
            matches!(exit, Ok(ChunkExit::Completed)),
            "唤醒后必须返回 Completed"
        );
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "唤醒必须在 100ms 内完成，实际 {:?}",
            started.elapsed()
        );
    }

    /// Phase 3d：排队等 Host 名额的分块被接管到 acked 时，必须在 100 毫秒内
    /// 以 Completed 退出（丢弃名额等待），且不占用任何名额。
    #[tokio::test]
    async fn queued_chunk_released_by_takeover_wakes_immediately() {
        let url = Url::parse("http://example.com/f").unwrap();
        let limiter = Arc::new(HostConnectionLimiter::new(1).unwrap());
        let cancel = CancelSignal::new();
        // 唯一名额被别的请求持有：本分块只能排队。
        let held = limiter.acquire(&url, &cancel).await.unwrap();

        let shared = Arc::new(ChunkShared::new(0, 4 * 1024 * 1024, 0));
        let mut end_rx = shared.subscribe_end();
        let acked = Arc::clone(&shared.acked);
        let lim = Arc::clone(&limiter);
        let sh = Arc::clone(&shared);
        let url2 = url.clone();
        let cancel2 = cancel.clone();
        let handle = tokio::spawn(async move {
            acquire_with_shrink(&lim, &url2, &cancel2, sh.as_ref(), &acked, &mut end_rx).await
        });

        // 等它真正进入 Semaphore 等待队列。
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(limiter.available_permits(&url).unwrap(), 0);

        // 测试钩子：直接把 end 缩到 acked（零进度接管），唤醒排队中的分块。
        assert!(shared.commit_split(0, 4 * 1024 * 1024, true));

        let started = Instant::now();
        let res = tokio::time::timeout(Duration::from_millis(100), handle).await;
        assert!(
            matches!(&res, Ok(Ok(Ok(None)))),
            "被接管到 acked 的排队分块必须丢弃等待、以 Completed 退出: {res:?}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "唤醒必须在 100ms 内完成，实际 {:?}",
            started.elapsed()
        );
        // 不占名额：held 仍是唯一持有者。
        assert_eq!(limiter.available_permits(&url).unwrap(), 0);
        drop(held);
        assert_eq!(
            limiter.available_permits(&url).unwrap(),
            1,
            "排队者不得偷拿 / 残留占用名额"
        );
    }
}
