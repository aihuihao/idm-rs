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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use reqwest::header::{CONTENT_ENCODING, CONTENT_RANGE, ETAG, IF_RANGE, RANGE};
use reqwest::{Client, StatusCode, Url};
use tokio::sync::{Mutex, watch};
use tokio::task::JoinSet;
use tracing::{info, trace, warn};

use crate::disk::win32;
use crate::disk::{AllocationStrategy, DiskWriterPool, PoolChunk, SpaceCheck, TargetFile};
use crate::error::{Error, Result};
use crate::net::RateLimiter;
use crate::net::client::DownloadConfig;
use crate::net::filename::{infer_filename, resolve_conflict, sanitize_windows_filename};
use crate::net::headers::parse_content_range;
use crate::net::probe::{Probe, ProbeResult};
use crate::store::{ChunkRow, PauseReasonDb, ProbeInfo, Store, TaskStateDb};
use crate::task::cancel::{CancelSignal, wait_cancelled};
use crate::task::progress::{ProgressPublisher, ProgressSnapshot};
use crate::task::state::{ChunkState, TaskState};

/// 检查点快照观察回调：参数是 (chunk_index, acked 绝对位置) 的快照 S。
pub type SnapshotObserver = Arc<dyn Fn(&[(i64, u64)]) + Send + Sync>;

/// 检查点测试钩子（F.4 / F.5）。
#[derive(Clone, Default)]
pub struct CheckpointHooks {
    /// 置位时 `sync` 视为失败：本次检查点放弃，不写数据库。
    pub fail_sync: Arc<AtomicBool>,
    /// 取得快照 S 之后、`sync` 之前调用，用于验证写入数据库的值不超过 S。
    pub observe_snapshot: Option<SnapshotObserver>,
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
    /// 每个数据库分块的 (原始 start, end, committed)。
    pub chunks: Vec<(u64, u64, u64)>,
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
    let snapshot: Vec<(i64, u64)> = acked
        .iter()
        .map(|(db_index, a)| (*db_index, a.load(Ordering::Relaxed)))
        .collect();
    if let Some(observe) = &ctx.hooks.observe_snapshot {
        observe(&snapshot);
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

/// 带可选持久化的下载。`ctx` 为 `None` 时行为与 [`download`] 完全一致；
/// `Some` 时启用 acked 跟踪、9.2 检查点循环、状态持久化与 9.4 完成顺序。
pub async fn download_with_store(
    url: &str,
    pool: Arc<DiskWriterPool>,
    opts: &DownloadOptions,
    cancel: Option<&CancelSignal>,
    ctx: Option<DownloadContext>,
) -> Result<(DownloadOutcome, watch::Receiver<ProgressSnapshot>)> {
    // 任务 ID 由数据库自增主键提供；无持久化时仅用于日志（0）。
    let task_id = ctx.as_ref().map_or(0, |c| c.task_id as u64);
    let url = Url::parse(url).map_err(|e| Error::InvalidArgument {
        message: format!("invalid URL: {e}"),
    })?;
    DownloadConfig::check_scheme(&url)?;

    let mut state = TaskState::Pending;
    let cancelled_now = || cancel.is_some_and(|c| c.is_cancelled());
    advance(&mut state, TaskState::Probing);

    // 元组：(client, probe_result, final_path, tmp_path, target, ranges,
    //       total_for_progress, streaming, db_indices)
    // 续传时 Probe 比较已由 resume() 完成，直接复用既有 .idm-part；
    // 新建下载走原来的 Probe → 创建文件 → 静态分块流程。
    let (
        client,
        probe_result,
        mut final_path,
        tmp_path,
        target,
        ranges,
        total_for_progress,
        streaming,
        db_indices,
    ) = if let Some(plan) = ctx.as_ref().and_then(|c| c.resume.clone()) {
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
        let tf = TargetFile::open_existing(&plan.tmp_path, plan.total_size)?;
        let mut rs = Vec::new();
        let mut ids = Vec::new();
        for (db_index, &(_, end, committed)) in plan.chunks.iter().enumerate() {
            if committed < end {
                // 本次起点就是 committed：Range 与 ChunkWriter 都从这里开始。
                rs.push((committed, end));
                ids.push(db_index as i64);
            }
        }
        (
            client,
            pr,
            plan.final_path,
            plan.tmp_path,
            tf,
            rs,
            Some(plan.total_size),
            false,
            ids,
        )
    } else {
        // 文件名在 Probe 之前先由 --out / URL 推断；Probe 之后若拿到 Content-Disposition 再重算。
        let pre_name = match &opts.out_name {
            Some(n) => sanitize_windows_filename(n),
            None => infer_filename(&url, None),
        };
        let mut fp = resolve_conflict(&opts.dir, &pre_name);
        let mut tp = part_path(&fp);

        // 取消检查点 1：Probe 开始之前（此时还没有任何文件）。
        if cancelled_now() {
            advance(&mut state, TaskState::Cancelled);
            return early_cancelled();
        }

        let client = opts.config.build_client()?;
        let probe = Probe::new(&client, &opts.config);
        let pr = match probe.probe(&url).await {
            Ok(r) => r,
            // Probe 阶段的异常（如 gzip）不创建任何文件；任务改为 Failed 并记录错误。
            Err(e) => {
                if let Some(c) = &ctx {
                    let _ = c
                        .store
                        .update_state(c.task_id, TaskStateDb::Failed, None, Some(format!("{e}")))
                        .await;
                }
                return Err(e);
            }
        };

        // Probe 得到 Content-Disposition 后重算最终文件名（未显式指定 --out 时）。
        if opts.out_name.is_none() {
            fp = resolve_conflict(&opts.dir, &pr.filename);
            tp = part_path(&fp);
        }

        // 取消检查点 2：Probe 完成、创建目标文件之前（同样不创建任何文件）。
        if cancelled_now() {
            advance(&mut state, TaskState::Cancelled);
            return early_cancelled();
        }
        // 让出一次，使外部取消有机会在"建文件之前"被观察到（协作式，不创建空文件）。
        tokio::task::yield_now().await;
        if cancelled_now() {
            advance(&mut state, TaskState::Cancelled);
            return early_cancelled();
        }

        // 选择模式：总长已知且支持 Range → 静态分块；否则单连接顺序（不预分配）。
        let (tf, rs, total, stream) = if let (Some(total), true) =
            (pr.total_size, pr.supports_range)
            && total > 0
        {
            let tf = match TargetFile::create(&tp, total, opts.strategy, SpaceCheck::Enforce) {
                Ok(t) => t,
                // 创建时磁盘满：文件尚未落盘，不留下任何文件。
                Err(e) if e.is_disk_full() => {
                    advance(&mut state, TaskState::Paused);
                    let (_tx, rx) = watch::channel(ProgressSnapshot::default());
                    return Ok((DownloadOutcome::Paused(PauseReason::DiskFull), rx));
                }
                Err(e) => return Err(e),
            };
            let rs = chunk_ranges(total, opts.config.connections, opts.config.min_chunk_size);
            (tf, rs, Some(total), false)
        } else {
            let tf = TargetFile::create(&tp, 0, opts.strategy, SpaceCheck::Skip)?;
            (tf, vec![(0, u64::MAX)], None, true)
        };
        let ids: Vec<i64> = (0..rs.len() as i64).collect();
        (client, pr, fp, tp, tf, rs, total, stream, ids)
    };
    let target = Arc::new(target);
    advance(&mut state, TaskState::Downloading);

    // Probe 成功后回填元信息；新建任务再写入初始分块（续传保留已有 committed）。
    if let Some(c) = &ctx {
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
            let rows: Vec<ChunkRow> = ranges
                .iter()
                .enumerate()
                .map(|(i, &(s, e))| ChunkRow {
                    chunk_index: i as i64,
                    start: s,
                    end: e,
                    committed: s,
                })
                .collect();
            c.store.save_chunks(c.task_id, rows).await?;
        }
    }

    // If-Range / 弱 ETag 校验策略：
    // - 强 ETag → 直接作为 If-Range；
    // - 弱 ETag 但有 Last-Modified → 用 Last-Modified 作为 If-Range（弱 ETag 本身不能做
    //   强一致性校验，但 Last-Modified 可以）；
    // - 无 ETag → 用 Last-Modified（若有）作为 If-Range；
    // - 弱 ETag 且无 Last-Modified → 不发 If-Range。
    // 无论强弱，都保留 `verify_etag` 用于逐分块校验响应中的 ETag（弱 ETag 一致性必查）。
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
        &ranges,
        opts.progress_tx.clone(),
        !streaming,
    );
    let progress = Arc::new(Mutex::new(progress_pub));
    let download_url = probe_result.final_url.clone();

    // 整次下载唯一的限速器：所有分块通过 Arc 共享，限制的是总速度。
    // 不限速时保持 None，读取循环里不插入任何等待，行为与之前完全一致。
    let limiter: Option<Arc<RateLimiter>> =
        opts.max_speed.map(|rate| Arc::new(RateLimiter::new(rate)));

    // 内部取消信号：所有分块任务订阅它；外部取消/出错/panic 都会先触发它（协作式）。
    let internal = CancelSignal::new();
    if let Some(ext) = cancel {
        let int = internal.clone();
        let ext = ext.clone();
        tokio::spawn(async move {
            ext.cancelled().await;
            int.cancel();
        });
    }

    // acked：每个分块的绝对写入位置，只在 pool.write 成功返回后推进。
    // 元素为 (数据库 chunk_index, 计数器)，续传时只覆盖未完成的分块。
    let acked: Vec<(i64, Arc<AtomicU64>)> = ranges
        .iter()
        .zip(db_indices.iter())
        .map(|(&(start, _), &db_index)| (db_index, Arc::new(AtomicU64::new(start))))
        .collect();

    // 每个任务独立运行一个检查点循环（9.2：定时触发，默认每 2 秒）。
    let checkpoint_task = ctx.as_ref().map(|c| {
        let c = c.clone();
        let p = pool.clone();
        let t = target.clone();
        let a = acked.clone();
        let mut ticker = tokio::time::interval(c.checkpoint_interval);
        tokio::spawn(async move {
            loop {
                ticker.tick().await;
                if let Err(e) = run_checkpoint(&c, &p, &t, &a).await {
                    warn!(error = %e, "checkpoint failed; database not updated");
                }
            }
        })
    });

    let mut join_set: JoinSet<Result<ChunkResult>> = JoinSet::new();
    let mut aborted_before_all_spawned = false;
    for (idx, &(start, end)) in ranges.iter().enumerate() {
        // 取消检查点 3：分块任务全部启动之前。
        if cancelled_now() {
            aborted_before_all_spawned = true;
            break;
        }
        let job = ChunkJob {
            client: client.clone(),
            url: download_url.clone(),
            pool: pool.clone(),
            target: target.clone(),
            chunk_id: idx as u64,
            start,
            end,
            if_range: if_range.clone(),
            verify_etag: verify_etag.clone(),
            progress: progress.clone(),
            idx,
            streaming,
            cancel_rx: internal.subscribe(),
            limiter: limiter.clone(),
            task_id,
            received: Arc::new(AtomicU64::new(0)),
            acked: acked[idx].1.clone(),
        };
        join_set.spawn(run_chunk(job));
        // 给外部取消一个插入点，使"分块全部启动之前取消"可被真实触发。
        tokio::task::yield_now().await;
    }

    if aborted_before_all_spawned || internal.is_cancelled() {
        internal.cancel();
        drain(&mut join_set).await;
        let _ = pool.sync(&target).await;
        advance(&mut state, TaskState::Cancelled);
        return Ok((DownloadOutcome::Cancelled, progress_rx));
    }

    let mut outcome: Option<Result<DownloadOutcome>> = None;
    loop {
        if join_set.is_empty() {
            break;
        }
        tokio::select! {
            biased;
            // 内部信号（外部取消 / 某分块出错 / panic 触发）已置位：停止 select，进入协作式排空。
            _ = internal.cancelled() => {
                outcome = Some(Ok(DownloadOutcome::Cancelled));
                break;
            }
            Some(joined) = join_set.join_next() => {
                match joined {
                    Ok(Ok(ChunkResult::Completed)) => {}
                    Ok(Ok(ChunkResult::Cancelled)) => {}
                    Ok(Err(e)) => {
                        // 任意分块出错：触发内部信号，让其余分块协作式退出。
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
                        // 分块 panic：触发内部信号后，作为错误上报。
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
        }
    }

    // 协作式排空：等待剩余分块自行退出（它们会走到 pool.release 释放写器），最多 5 秒；
    // 超时（例如某分块卡死）才 abort_all 强制结束。
    let deadline = Instant::now() + Duration::from_secs(5);
    while !join_set.is_empty() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, join_set.join_next()).await {
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(_) => {
                join_set.abort_all();
                drain(&mut join_set).await;
                break;
            }
        }
    }

    if let Some(h) = &checkpoint_task {
        h.abort();
    }
    // 9.2：任务暂停、取消、失败、完成之前，各触发一次最终检查点。
    if let Some(c) = &ctx
        && let Err(e) = run_checkpoint(c, &pool, &target, &acked).await
    {
        warn!(error = %e, "final checkpoint abandoned; committed not advanced");
        // sync 失败且原因是磁盘已满 → 任务暂停，保留 .idm-part。
        if e.is_disk_full() {
            outcome = Some(Ok(DownloadOutcome::Paused(PauseReason::DiskFull)));
        }
    }

    match outcome {
        Some(r) => {
            // 失败/取消/暂停同样要发布最终快照，否则 CLI 的分块汇总表拿到的是
            // 上一次 200ms 节流的旧数据（分块终态、耗时、平均速度都会缺失）。
            progress.lock().await.publish_final();
            // 持久化层映射：Cancelled → Paused(User)；Paused(DiskFull) → Paused(DiskFull)；
            // 错误 → Failed。core 的返回值保持 Cancelled，原有测试不受影响。
            if let Some(c) = &ctx {
                match &r {
                    Ok(DownloadOutcome::Cancelled) => {
                        let _ = c
                            .store
                            .update_state(
                                c.task_id,
                                TaskStateDb::Paused,
                                Some(PauseReasonDb::User),
                                None,
                            )
                            .await;
                    }
                    Ok(DownloadOutcome::Paused(_)) => {
                        let _ = c
                            .store
                            .update_state(
                                c.task_id,
                                TaskStateDb::Paused,
                                Some(PauseReasonDb::DiskFull),
                                None,
                            )
                            .await;
                    }
                    Ok(DownloadOutcome::Completed) => {}
                    Err(e) => {
                        let _ = c
                            .store
                            .update_state(
                                c.task_id,
                                TaskStateDb::Failed,
                                None,
                                Some(format!("{e}")),
                            )
                            .await;
                    }
                }
            }
            let _ = pool.sync(&target).await;
            match r {
                Ok(o) => {
                    match o {
                        DownloadOutcome::Cancelled => advance(&mut state, TaskState::Cancelled),
                        DownloadOutcome::Paused(_) => advance(&mut state, TaskState::Paused),
                        DownloadOutcome::Completed => {}
                    }
                    // 非 Completed 结束：调用 purge 兜底清理可能残留的写入器（panic/泄露时）。
                    purge_if_needed(&pool, &target, task_id).await;
                    info!(task_id, state = ?state, "download stopped");
                    Ok((o, progress_rx))
                }
                Err(e) => {
                    advance(&mut state, TaskState::Failed);
                    purge_if_needed(&pool, &target, task_id).await;
                    info!(task_id, state = ?state, error = %e, "download failed");
                    Err(e)
                }
            }
        }
        None => {
            // 9.4：最终检查点（已在上面统一执行）→ 数据库状态改为 Finalizing
            // → 释放所有分块和文件句柄 → 不覆盖地重命名 → 记录最终文件名并改为 Completed。
            if let Some(c) = &ctx {
                c.store
                    .update_state(c.task_id, TaskStateDb::Finalizing, None, None)
                    .await?;
            }
            advance(&mut state, TaskState::Finalizing);
            progress.lock().await.publish_final();
            pool.sync(&target).await?;
            // 各分块已在 run_chunk 内 release；这里释放最后一个 TargetFile 句柄。
            drop(target);
            // 最终改名前再确认不覆盖：仅当最终文件名（不含 .idm-part）已被占用时，
            // 才用 resolve_conflict 另选名字。注意：不能让自身的 .idm-part 触发避让，
            // 否则会把已下载的文件误改名成 `name (1)`。
            let desired = final_path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            // 续传时 final_path 来自数据库记录，其父目录才是正确基准。
            let base = final_path.parent().unwrap_or(opts.dir.as_path());
            let final_candidate = base.join(&desired);
            final_path = if final_candidate.exists() {
                resolve_conflict(&opts.dir, &desired)
            } else {
                final_candidate
            };
            win32::move_file(&tmp_path, &final_path).map_err(|e| Error::Io {
                operation: "rename (move_file)",
                path: final_path.clone(),
                offset: 0,
                source: std::io::Error::other(format!("{e}")),
            })?;
            // 9.4 最后一步：记录最终文件名，状态改为 Completed。
            if let Some(c) = &ctx {
                let name = final_path
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_default();
                c.store.set_final_name(c.task_id, &name).await?;
            }
            advance(&mut state, TaskState::Completed);
            info!(task_id, state = ?state, "download completed");
            Ok((DownloadOutcome::Completed, progress_rx))
        }
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

/// 一个分块任务的最终结果。
enum ChunkResult {
    Completed,
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
    target: Arc<TargetFile>,
    chunk_id: u64,
    start: u64,
    end: u64,
    if_range: Option<String>,
    verify_etag: Option<(String, bool)>,
    progress: Arc<Mutex<ProgressPublisher>>,
    idx: usize,
    streaming: bool,
    cancel_rx: watch::Receiver<bool>,
    limiter: Option<Arc<RateLimiter>>,
    task_id: u64,
    received: Arc<AtomicU64>,
    /// 只在 pool.write 成功返回后推进的绝对写入位置（检查点快照来源）。
    acked: Arc<AtomicU64>,
}

/// 单个分块的下载任务：注册写器 → 请求 → 读取 → 经池写入 → 统一 release。
async fn run_chunk(job: ChunkJob) -> Result<ChunkResult> {
    let task_id = job.task_id;
    let chunk_id = job.chunk_id;
    let start = job.start;
    let end = job.end;
    let idx = job.idx;
    let streaming = job.streaming;
    let received_counter = job.received.clone();
    let pool = job.pool.clone();
    let progress = job.progress.clone();

    info!(
        task_id,
        chunk_id,
        start,
        end,
        received = 0u64,
        "chunk started"
    );

    // 总长未知的顺序下载使用无界区间；静态分块使用有界区间。
    let writer = if streaming {
        job.target.allocate_unbounded_chunk(chunk_id, start)?
    } else {
        job.target.allocate_chunk(chunk_id, start, end)?
    };
    let pc = pool.register(writer).await?;
    let mut cs = next_chunk_state(ChunkState::Pending, ChunkState::Downloading);
    progress.lock().await.set_chunk_state(idx, cs);

    let chunk_started = Instant::now();
    let inner = run_chunk_inner(job, &pc).await;
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
            Ok(ChunkResult::Completed)
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

/// 分块主体：每一步网络读取与磁盘写入之前都检查取消。分块请求**不**设总时长超时
/// （依赖 Client 级别的 `connect_timeout` 与 `read_timeout`），避免大分块在慢速网络下失败。
async fn run_chunk_inner(job: ChunkJob, pc: &PoolChunk) -> Result<ChunkExit> {
    let ChunkJob {
        client,
        url,
        pool,
        chunk_id,
        start,
        end,
        if_range,
        verify_etag,
        progress,
        idx,
        streaming,
        mut cancel_rx,
        limiter,
        task_id,
        received: received_counter,
        acked: acked_counter,
        target: _,
    } = job;

    if rx_cancelled(&cancel_rx) {
        return Ok(ChunkExit::Cancelled);
    }

    let expected = end.saturating_sub(start);

    // 流式（总长未知）模式不发送 Range；分块模式发送精确区间。
    let mut req = client.get(url.clone());
    if !streaming {
        let range_hdr = if expected == 0 {
            "bytes=0-0".to_string()
        } else {
            format!("bytes={}-{}", start, end - 1)
        };
        req = req.header(RANGE, range_hdr);
        if let Some(ir) = &if_range {
            req = req.header(IF_RANGE, ir);
        }
    }

    // 等待响应头时同样监听取消。
    let mut resp = tokio::select! {
        biased;
        _ = wait_cancelled(&mut cancel_rx) => return Ok(ChunkExit::Cancelled),
        r = req.send() => r.map_err(|e| Error::Http { url: url.to_string(), source: e })?,
    };
    resp.error_for_status_ref().map_err(|e| Error::Http {
        url: url.to_string(),
        source: e,
    })?;
    let status = resp.status();

    // 非 identity 的 Content-Encoding → 绝不把压缩字节当文件内容。
    if let Some(ce) = resp.headers().get(CONTENT_ENCODING) {
        let ce = ce.to_str().unwrap_or("").trim().to_ascii_lowercase();
        if ce != "identity" {
            return Err(Error::UnsupportedContentEncoding { encoding: ce });
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
                });
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
                    _ => return Err(Error::RemoteChanged),
                }
            }
        }
        StatusCode::OK if streaming => {}
        StatusCode::OK if !streaming => {
            if if_range.is_some() {
                return Err(Error::RemoteChanged);
            }
            return Err(Error::RangeNotHonored);
        }
        _ => {
            return Err(Error::InvalidContentRange {
                requested: (start, end),
                actual: status.to_string(),
            });
        }
    }

    // 读取响应体并写入；网络 Bytes 零拷贝直接交给线程池。
    let mut received: u64 = 0;
    loop {
        // 读网络之前检查取消。
        if rx_cancelled(&cancel_rx) {
            return Ok(ChunkExit::Cancelled);
        }
        let bytes = tokio::select! {
            biased;
            _ = wait_cancelled(&mut cancel_rx) => return Ok(ChunkExit::Cancelled),
            r = resp.chunk() => match r {
                Ok(Some(b)) => b,
                Ok(None) => break,
                Err(e) => {
                    // 读取空闲超时（read_timeout）属于超时相关错误，必须如实返回，
                    // 不能误判为"响应体被截断"。
                    if e.is_timeout() {
                        return Err(Error::Http { url: url.to_string(), source: e });
                    }
                    // 响应体被中途截断时按"提前结束"处理，而不是笼统的 HTTP 错误。
                    if !streaming && received < expected {
                        received_counter.store(received, Ordering::Relaxed);
                        return Err(Error::UnexpectedEof { chunk_id, expected, received });
                    }
                    return Err(Error::Http { url: url.to_string(), source: e });
                }
            },
        };
        let len = bytes.len() as u64;
        if !streaming && received + len > expected {
            return Err(Error::InvalidContentRange {
                requested: (start, end),
                actual: format!("received more than {expected} bytes"),
            });
        }
        if len > 0 {
            // 限速：读到一块数据之后、交给写入线程池之前领取配额。等待被延后后，
            // TCP 接收窗口填满，服务器端随之放慢发送，从而真正限制网络流量。
            // acquire 与取消信号并列，等待令牌时也能立刻响应取消。
            if let Some(rl) = &limiter {
                tokio::select! {
                    biased;
                    _ = wait_cancelled(&mut cancel_rx) => return Ok(ChunkExit::Cancelled),
                    _ = rl.acquire(len) => {}
                }
            }
            // 写磁盘之前检查取消。
            if rx_cancelled(&cancel_rx) {
                return Ok(ChunkExit::Cancelled);
            }
            trace!(task_id, chunk_id, len, "chunk data");
            pool.write(pc, bytes).await?;
            // 只有落盘线程池返回成功回执之后才推进 acked。
            acked_counter.fetch_add(len, Ordering::Relaxed);
            received += len;
            received_counter.store(received, Ordering::Relaxed);
            let mut p = progress.lock().await;
            p.set_chunk_current(idx, start + received);
            p.publish_if_due();
        }
    }
    received_counter.store(received, Ordering::Relaxed);

    if rx_cancelled(&cancel_rx) {
        return Ok(ChunkExit::Cancelled);
    }
    if !streaming && received != expected {
        return Err(Error::UnexpectedEof {
            chunk_id,
            expected,
            received,
        });
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

/// 在目标文件创建之前取消：不创建任何文件，直接返回 `Cancelled`。
fn early_cancelled() -> Result<(DownloadOutcome, watch::Receiver<ProgressSnapshot>)> {
    let (_tx, rx) = watch::channel(ProgressSnapshot::default());
    Ok((DownloadOutcome::Cancelled, rx))
}
