//! 下载编排：Probe → 选择模式（静态分块 / 单连接顺序）→ JoinSet 并发 → 完成/取消/暂停。

use std::path::PathBuf;
use std::sync::Arc;

use reqwest::header::{CONTENT_ENCODING, CONTENT_RANGE, ETAG, IF_RANGE, RANGE};
use reqwest::{Client, StatusCode, Url};
use tokio::sync::{Mutex, Notify, watch};
use tokio::task::JoinSet;

use crate::disk::{AllocationStrategy, DiskWriterPool, SpaceCheck, TargetFile};
use crate::error::{Error, Result};
use crate::net::client::DownloadConfig;
use crate::net::filename::{resolve_conflict, sanitize_windows_filename};
use crate::net::headers::parse_content_range;
use crate::net::probe::Probe;
use crate::task::progress::{ProgressPublisher, ProgressSnapshot};
use crate::task::state::ChunkState;

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
}

/// 执行一次下载。返回一个结果，以及进度快照的 `watch` 接收端（可不读）。
///
/// `cancel` 为外部取消信号（如 Ctrl+C）；传 `None` 表示不可取消。
pub async fn download(
    url: &str,
    pool: Arc<DiskWriterPool>,
    opts: &DownloadOptions,
    cancel: Option<&Notify>,
) -> Result<(DownloadOutcome, watch::Receiver<ProgressSnapshot>)> {
    let url = Url::parse(url).map_err(|e| Error::InvalidArgument {
        message: format!("invalid URL: {e}"),
    })?;
    DownloadConfig::check_scheme(&url)?;
    let client = opts.config.build_client()?;
    let probe = Probe::new(&client, &opts.config);
    let probe_result = probe.probe(&url).await?;

    // 计算最终文件名与临时（.idm-part）路径。
    let raw_name = match &opts.out_name {
        Some(n) => sanitize_windows_filename(n),
        None => probe_result.filename.clone(),
    };
    let final_path = resolve_conflict(&opts.dir, &raw_name);
    let tmp_path = final_path.with_file_name(format!(
        "{}.idm-part",
        final_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));

    // 选择模式：总长已知且支持 Range → 静态分块；否则单连接顺序（不预分配）。
    let (target, ranges, total_for_progress, streaming) =
        if let (Some(total), true) = (probe_result.total_size, probe_result.supports_range) {
            let target =
                match TargetFile::create(&tmp_path, total, opts.strategy, SpaceCheck::Enforce) {
                    Ok(t) => t,
                    Err(e) if e.is_disk_full() => {
                        let rx = watch::channel(ProgressSnapshot::default()).1;
                        return Ok((DownloadOutcome::Paused(PauseReason::DiskFull), rx));
                    }
                    Err(e) => return Err(e),
                };
            let ranges = chunk_ranges(total, opts.config.connections, opts.config.min_chunk_size);
            (target, ranges, Some(total), false)
        } else {
            let target = TargetFile::create(&tmp_path, 0, opts.strategy, SpaceCheck::Skip)?;
            (target, vec![(0, u64::MAX)], None, true)
        };

    let target = Arc::new(target);

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

    let (progress_pub, progress_rx) = ProgressPublisher::new(total_for_progress, &ranges);
    let progress = Arc::new(Mutex::new(progress_pub));

    let mut join_set: JoinSet<Result<u64>> = JoinSet::new();
    for (idx, &(start, end)) in ranges.iter().enumerate() {
        let job = ChunkJob {
            client: client.clone(),
            url: url.clone(),
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
        };
        join_set.spawn(run_chunk(job));
    }

    let mut outcome: Option<Result<DownloadOutcome>> = None;
    loop {
        tokio::select! {
            Some(joined) = join_set.join_next() => {
                match joined {
                    Ok(Ok(_pos)) => {
                        if join_set.is_empty() {
                            break;
                        }
                    }
                    Ok(Err(e)) => {
                        join_set.abort_all();
                        drain(&mut join_set).await;
                        if e.is_disk_full() {
                            outcome = Some(Ok(DownloadOutcome::Paused(PauseReason::DiskFull)));
                        } else {
                            outcome = Some(Err(e));
                        }
                        break;
                    }
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
            _ = cancel_fut(cancel) => {
                join_set.abort_all();
                drain(&mut join_set).await;
                outcome = Some(Ok(DownloadOutcome::Cancelled));
                break;
            }
        }
    }

    match outcome {
        Some(r) => match r {
            Ok(o @ (DownloadOutcome::Cancelled | DownloadOutcome::Paused(_))) => {
                // 保留 .idm-part：先刷盘（以备 1c 续传），不重命名。
                let _ = pool.sync(&target).await;
                Ok((o, progress_rx))
            }
            Err(e) => {
                let _ = pool.sync(&target).await;
                Err(e)
            }
            Ok(DownloadOutcome::Completed) => unreachable!("Completed handled below"),
        },
        None => {
            // 所有分块成功。
            progress.lock().await.publish_final();
            pool.sync(&target).await?;
            drop(target);
            std::fs::rename(&tmp_path, &final_path).map_err(|source| Error::Io {
                operation: "rename",
                path: final_path.clone(),
                offset: 0,
                source,
            })?;
            Ok((DownloadOutcome::Completed, progress_rx))
        }
    }
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
}

/// 单个分块的下载任务：注册写器 → 请求（带 Range / If-Range）→ 读取 → 经池写入。
async fn run_chunk(job: ChunkJob) -> Result<u64> {
    let ChunkJob {
        client,
        url,
        config,
        pool,
        target,
        chunk_id,
        start,
        end,
        if_range,
        verify_etag,
        progress,
        idx,
        streaming,
    } = job;
    let writer = target.allocate_chunk(chunk_id, start, end)?;
    let pc = pool.register(writer).await?;
    progress
        .lock()
        .await
        .set_chunk_state(idx, ChunkState::Downloading);

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

    let mut resp = req.send().await.map_err(|e| Error::Http {
        url: url.to_string(),
        source: e,
    })?;
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
            // 206：校验 Content-Range 与请求区间完全一致。
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
                && let Some(r_etag) = resp.headers().get(ETAG).and_then(|v| v.to_str().ok())
                && r_etag.trim() != p_etag.0.trim()
            {
                return Err(Error::RemoteChanged);
            }
        }
        StatusCode::OK if streaming => {
            // 流式模式本就不发送 Range，200 是预期的成功状态码。
        }
        StatusCode::OK if !streaming => {
            // 未带 If-Range 却返回 200 → 服务器不遵守 Range 请求。
            if if_range.is_some() {
                return Err(Error::RemoteChanged);
            } else {
                return Err(Error::RangeNotHonored);
            }
        }
        _ => {
            return Err(Error::InvalidContentRange {
                requested: (start, end),
                actual: status.to_string(),
            });
        }
    }

    // 读取响应体并写入；数据零拷贝直接交给线程池。
    let mut received: u64 = 0;
    loop {
        let bytes = match resp.chunk().await {
            Ok(Some(b)) => b,
            Ok(None) => break,
            Err(e) => {
                return Err(Error::Http {
                    url: url.to_string(),
                    source: e,
                });
            }
        };
        let len = bytes.len() as u64;
        if !streaming && received + len > expected {
            // 多出数据：不得写入。
            return Err(Error::InvalidContentRange {
                requested: (start, end),
                actual: format!("received more than {expected} bytes"),
            });
        }
        if len > 0 {
            pool.write(&pc, bytes).await?;
            received += len;
            let mut p = progress.lock().await;
            p.set_chunk_current(idx, start + received);
            p.publish_if_due();
        }
    }

    if !streaming && received != expected {
        return Err(Error::UnexpectedEof {
            chunk_id,
            expected,
            received,
        });
    }

    let final_pos = pool.release(pc).await?;
    if !streaming && final_pos != end {
        return Err(Error::UnexpectedEof {
            chunk_id,
            expected: end,
            received: final_pos,
        });
    }
    progress.lock().await.set_chunk_state(idx, ChunkState::Done);
    Ok(final_pos)
}

/// 把 `total` 均匀切成至多 `connections` 个分块（各分块尽量均匀）。
fn chunk_ranges(total: u64, connections: usize, min_chunk: u64) -> Vec<(u64, u64)> {
    let min_chunk = min_chunk.max(1);
    if total == 0 {
        return vec![(0, 0)];
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
async fn drain(set: &mut JoinSet<Result<u64>>) {
    while set.join_next().await.is_some() {}
}

/// 取消信号 future：有 `Notify` 时 await 它；否则永久挂起。
async fn cancel_fut(cancel: Option<&Notify>) {
    match cancel {
        Some(n) => n.notified().await,
        None => std::future::pending::<()>().await,
    }
}
