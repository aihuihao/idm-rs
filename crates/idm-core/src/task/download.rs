//! 下载编排：Probe → 选择模式（静态分块 / 单连接顺序）→ JoinSet 并发 → 完成/取消/暂停。
//!
//! 取消基于 [`CancelSignal`]（`watch` 状态语义），在下载各阶段与每个分块任务的
//! 网络读取/磁盘写入之前检查，绝不会因"取消发生在等待开始之前"而漏掉。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use reqwest::header::{CONTENT_ENCODING, CONTENT_RANGE, ETAG, IF_RANGE, RANGE};
use reqwest::{Client, StatusCode, Url};
use tokio::sync::{Mutex, watch};
use tokio::task::JoinSet;
use tracing::{info, trace};

use crate::disk::{AllocationStrategy, DiskWriterPool, PoolChunk, SpaceCheck, TargetFile};
use crate::error::{Error, Result};
use crate::net::client::DownloadConfig;
use crate::net::filename::{infer_filename, resolve_conflict, sanitize_windows_filename};
use crate::net::headers::parse_content_range;
use crate::net::probe::Probe;
use crate::task::cancel::{CancelSignal, wait_cancelled};
use crate::task::progress::{ProgressPublisher, ProgressSnapshot};
use crate::task::state::{ChunkState, TaskState};

/// 进程内递增的任务编号（仅用于日志关联，1c 持久化时再替换为真实 ID）。
static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);

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
}

/// 执行一次下载。返回结果，以及进度快照的 `watch` 接收端（可不读）。
///
/// `cancel` 为外部取消信号（基于 `watch`，不会漏掉任何时刻的取消）；传 `None`
/// 表示不可取消。取消、失败、磁盘满时均保留 `.idm-part` 文件。
pub async fn download(
    url: &str,
    pool: Arc<DiskWriterPool>,
    opts: &DownloadOptions,
    cancel: Option<&CancelSignal>,
) -> Result<(DownloadOutcome, watch::Receiver<ProgressSnapshot>)> {
    let task_id = NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed);
    let url = Url::parse(url).map_err(|e| Error::InvalidArgument {
        message: format!("invalid URL: {e}"),
    })?;
    DownloadConfig::check_scheme(&url)?;

    // 文件名在 Probe 之前先由 --out / URL 推断，保证任何时刻取消都能留下 .idm-part。
    let pre_name = match &opts.out_name {
        Some(n) => sanitize_windows_filename(n),
        None => infer_filename(&url, None),
    };
    let mut final_path = resolve_conflict(&opts.dir, &pre_name);
    let mut tmp_path = part_path(&final_path);

    let mut state = TaskState::Pending;
    let cancelled_now = || cancel.is_some_and(|c| c.is_cancelled());
    advance(&mut state, TaskState::Probing);

    // 取消检查点 1：Probe 开始之前（此时还没有任何文件，补建空的 .idm-part）。
    if cancelled_now() {
        advance(&mut state, TaskState::Cancelled);
        return early_cancelled(&tmp_path);
    }

    let client = opts.config.build_client()?;
    let probe = Probe::new(&client, &opts.config);
    let probe_result = match probe.probe(&url).await {
        Ok(r) => r,
        Err(e) => {
            // Probe 阶段的异常（如 gzip）也要留下 .idm-part。
            let _ = ensure_part_file(&tmp_path);
            return Err(e);
        }
    };

    // Probe 得到 Content-Disposition 后重算最终文件名（未显式指定 --out 时）。
    if opts.out_name.is_none() {
        final_path = resolve_conflict(&opts.dir, &probe_result.filename);
        tmp_path = part_path(&final_path);
    }

    // 取消检查点 2：Probe 完成、创建目标文件之前。
    if cancelled_now() {
        advance(&mut state, TaskState::Cancelled);
        return early_cancelled(&tmp_path);
    }

    // 选择模式：总长已知且支持 Range → 静态分块；否则单连接顺序（不预分配）。
    let (target, ranges, total_for_progress, streaming) = if let (Some(total), true) =
        (probe_result.total_size, probe_result.supports_range)
        && total > 0
    {
        let target = match TargetFile::create(&tmp_path, total, opts.strategy, SpaceCheck::Enforce)
        {
            Ok(t) => t,
            Err(e) if e.is_disk_full() => {
                let _ = ensure_part_file(&tmp_path);
                let rx = watch::channel(ProgressSnapshot::default()).1;
                return Ok((DownloadOutcome::Paused(PauseReason::DiskFull), rx));
            }
            Err(e) => return Err(e),
        };
        let ranges = chunk_ranges(total, opts.config.connections, opts.config.min_chunk_size);
        (target, ranges, Some(total), false)
    } else {
        let target = TargetFile::create(&tmp_path, 0, opts.strategy, SpaceCheck::Skip)?;
        // 无界区间用 (start, u64::MAX) 表示；run_chunk 会走
        // `allocate_unbounded_chunk` 专用入口。
        (target, vec![(0, u64::MAX)], None, true)
    };
    let target = Arc::new(target);
    advance(&mut state, TaskState::Downloading);

    // If-Range / 弱 ETag 校验策略。
    let if_range: Option<String> = match &probe_result.etag {
        Some((tag, weak)) if !*weak => Some(tag.clone()),
        None => probe_result.last_modified.clone(),
        Some(_) => None,
    };
    let verify_etag: Option<(String, bool)> = if if_range.is_none() {
        probe_result.etag.clone()
    } else {
        None
    };

    // 单连接顺序下载没有分块边界，明确标记为"不可续传"供 1c 使用。
    let (progress_pub, progress_rx) = ProgressPublisher::new(
        total_for_progress,
        &ranges,
        opts.progress_tx.clone(),
        !streaming,
    );
    let progress = Arc::new(Mutex::new(progress_pub));
    let download_url = probe_result.final_url.clone();

    // 没有外部取消信号时，也要给分块任务一个"永不取消"的接收端：其发送端由
    // `own_signal` 持有到本次下载结束，这样 `changed()` 不会误判为"发送端已关闭"。
    let own_signal = CancelSignal::new();
    let signal = cancel.unwrap_or(&own_signal);

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
            config: opts.config.clone(),
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
            cancel_rx: signal.subscribe(),
            task_id,
            received: Arc::new(AtomicU64::new(0)),
        };
        join_set.spawn(run_chunk(job));
        // 给外部取消一个插入点，使"分块全部启动之前取消"可被真实触发。
        tokio::task::yield_now().await;
    }

    if aborted_before_all_spawned || cancelled_now() {
        join_set.abort_all();
        drain(&mut join_set).await;
        let _ = pool.sync(&target).await;
        ensure_part_file(&tmp_path)?;
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
            // 取消检查点 4：下载进行中。`watch` 的状态语义保证此前任何时刻的取消
            // 都不会被漏掉——`cancelled()` 对已取消的信号立即返回。
            _ = cancel_fut(cancel) => {
                join_set.abort_all();
                drain(&mut join_set).await;
                outcome = Some(Ok(DownloadOutcome::Cancelled));
                break;
            }
            Some(joined) = join_set.join_next() => {
                match joined {
                    Ok(Ok(ChunkResult::Completed)) => {}
                    Ok(Ok(ChunkResult::Cancelled)) => {
                        join_set.abort_all();
                        drain(&mut join_set).await;
                        outcome = Some(Ok(DownloadOutcome::Cancelled));
                        break;
                    }
                    Ok(Err(e)) => {
                        join_set.abort_all();
                        drain(&mut join_set).await;
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
                        join_set.abort_all();
                        drain(&mut join_set).await;
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

    match outcome {
        Some(r) => {
            let _ = pool.sync(&target).await;
            ensure_part_file(&tmp_path)?;
            match r {
                Ok(o) => {
                    match o {
                        DownloadOutcome::Cancelled => advance(&mut state, TaskState::Cancelled),
                        DownloadOutcome::Paused(_) => advance(&mut state, TaskState::Paused),
                        DownloadOutcome::Completed => {}
                    }
                    info!(task_id, state = ?state, "download stopped");
                    Ok((o, progress_rx))
                }
                Err(e) => {
                    advance(&mut state, TaskState::Failed);
                    info!(task_id, state = ?state, error = %e, "download failed");
                    Err(e)
                }
            }
        }
        None => {
            // 所有分块成功：release 已在各分块内完成 → sync → 释放句柄 → 重命名。
            advance(&mut state, TaskState::Finalizing);
            progress.lock().await.publish_final();
            pool.sync(&target).await?;
            drop(target);
            std::fs::rename(&tmp_path, &final_path).map_err(|source| Error::Io {
                operation: "rename",
                path: final_path.clone(),
                offset: 0,
                source,
            })?;
            advance(&mut state, TaskState::Completed);
            info!(task_id, state = ?state, "download completed");
            Ok((DownloadOutcome::Completed, progress_rx))
        }
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
    config: DownloadConfig,
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
    task_id: u64,
    received: Arc<AtomicU64>,
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

    let inner = run_chunk_inner(job, &pc).await;
    // 无论成功、失败还是取消，都统一 release，避免文件句柄在 worker 线程中残留。
    let released = pool.release(pc).await;
    let received = received_counter.load(Ordering::Relaxed);

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

/// 分块主体：每一步网络读取与磁盘写入之前都检查取消。
async fn run_chunk_inner(job: ChunkJob, pc: &PoolChunk) -> Result<ChunkExit> {
    let ChunkJob {
        client,
        url,
        config,
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
        task_id,
        received: received_counter,
        target: _,
    } = job;

    if rx_cancelled(&cancel_rx) {
        return Ok(ChunkExit::Cancelled);
    }

    let expected = end.saturating_sub(start);

    // 流式（总长未知）模式不发送 Range；分块模式发送精确区间。
    let mut req = client.get(url.clone()).timeout(config.request_timeout());
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
            // 写磁盘之前检查取消。
            if rx_cancelled(&cancel_rx) {
                return Ok(ChunkExit::Cancelled);
            }
            trace!(task_id, chunk_id, len, "chunk data");
            pool.write(pc, bytes).await?;
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

/// 取消信号 future：有信号时等待其变为已取消；否则永久挂起。
async fn cancel_fut(cancel: Option<&CancelSignal>) {
    match cancel {
        Some(c) => c.cancelled().await,
        None => std::future::pending::<()>().await,
    }
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

/// 取消/失败时确保 `.idm-part` 存在（已存在则不动），供 1c 续传使用。
fn ensure_part_file(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| Error::Io {
            operation: "create_dir_all",
            path: parent.to_path_buf(),
            offset: 0,
            source,
        })?;
    }
    let _ = std::fs::File::create(path).map_err(|source| Error::Io {
        operation: "create partial file",
        path: path.to_path_buf(),
        offset: 0,
        source,
    })?;
    Ok(())
}

/// 在目标文件创建之前取消：补建空的 `.idm-part` 并返回 `Cancelled`。
fn early_cancelled(
    tmp_path: &Path,
) -> Result<(DownloadOutcome, watch::Receiver<ProgressSnapshot>)> {
    ensure_part_file(tmp_path)?;
    let (_tx, rx) = watch::channel(ProgressSnapshot::default());
    Ok((DownloadOutcome::Cancelled, rx))
}
