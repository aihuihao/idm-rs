//! Phase 1d：分块重试 / 退避 / Retry-After（全部使用本地测试服务器，不访问外网）。

#![cfg(windows)]

mod support;

#[path = "support/verify.rs"]
mod verify;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use idm_core::disk::{AllocationStrategy, DiskWriterPool};
use idm_core::error::Error;
use idm_core::net::client::DownloadConfig;
use idm_core::store::{NewTask, PauseReasonDb, Store, TaskStateDb};
use idm_core::task::{
    CancelSignal, DownloadContext, DownloadOptions, DownloadOutcome, PauseReason, ProgressSnapshot,
    download, download_with_store, resume,
};
use support::{RangeMode, ServerConfig, start};
use tempfile::tempdir;
use tokio::sync::watch;
use verify::verify_file;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;

fn options(dir: &Path, cfg: DownloadConfig, out: &str) -> DownloadOptions {
    DownloadOptions {
        config: cfg,
        strategy: AllocationStrategy::Sparse,
        dir: dir.to_path_buf(),
        out_name: Some(out.to_string()),
        progress_tx: None,
        max_speed: None,
    }
}

/// 单连接 + 毫秒级退避：默认配置（多连接、秒级退避）对重试测试太慢。
fn retry_cfg(max_retries: u32) -> DownloadConfig {
    DownloadConfig {
        connections: 1,
        max_retries,
        retry_base_delay: Duration::from_millis(20),
        retry_max_delay: Duration::from_millis(50),
        ..DownloadConfig::default()
    }
}

/// 每个分块的累计重试次数。
fn retries_of(rx: &watch::Receiver<ProgressSnapshot>) -> u64 {
    rx.borrow().chunks.iter().map(|c| c.retries).sum()
}

fn requests(count: &Arc<AtomicU64>) -> u64 {
    count.load(Ordering::SeqCst)
}

fn new_task(url: &str, dir: &Path, connections: usize) -> NewTask {
    NewTask {
        url: url.to_string(),
        dir: dir.to_string_lossy().into_owned(),
        strategy: "Sparse".to_string(),
        connections,
    }
}

async fn shutdown_pool(pool: Arc<DiskWriterPool>) -> TestResult {
    let pool = Arc::try_unwrap(pool)
        .map_err(|_| -> Box<dyn std::error::Error> { "pool still referenced".into() })?;
    pool.shutdown().await?;
    Ok(())
}

/// 修复 1：`max_retries = 0` 时首次错误必须原样返回，不得包装成 `RetryExhausted`。
///
/// 服务器每次响应写到 64 KiB 就断开，必然产生可重试的 `UnexpectedEof`；由于禁用
/// 重试，该错误应作为原始错误直接冒泡，并保留其具体变体（可续传）。
#[tokio::test]
async fn zero_retries_returns_original_error() -> TestResult {
    let size = 2 * MIB;
    let srv = start(ServerConfig {
        size,
        break_after_bytes: Some(64 * KIB),
        ..Default::default()
    })
    .await?;
    let tmp = tempfile::tempdir()?;
    let pool = Arc::new(DiskWriterPool::new(2, 8)?);
    let opts = options(
        tmp.path(),
        DownloadConfig {
            connections: 1,
            max_retries: 0,
            ..DownloadConfig::default()
        },
        "zero.bin",
    );

    let err = download(&srv.url("/file.bin"), pool.clone(), &opts, None)
        .await
        .expect_err("download must fail since retries are disabled");
    assert!(
        !matches!(err, Error::RetryExhausted { .. }),
        "max_retries=0 must return the original error, not RetryExhausted: {err:?}"
    );
    assert!(
        matches!(err, Error::UnexpectedEof { .. }),
        "expected the original UnexpectedEof error, got {err:?}"
    );

    shutdown_pool(pool).await?;
    srv.shutdown();
    Ok(())
}

/// 修复 1（流式）：服务器不支持 Range（`NoRange` → 走单连接 streaming）时，首个
/// 分块请求直接 500。流式模式强制 `max_retries = 0`，因此必须返回原始 `Http`
/// 错误而不得包装成 `RetryExhausted`。
#[tokio::test]
async fn streaming_first_failure_returns_original_error() -> TestResult {
    let size = MIB;
    // NoRange：Probe 得到 200 → supports_range=false → streaming；
    // error_status=500：Probe 被豁免，但真正的下载请求立即 500。
    let srv = start(ServerConfig {
        size,
        range: RangeMode::NoRange,
        error_status: Some(500),
        ..Default::default()
    })
    .await?;
    let tmp = tempfile::tempdir()?;
    let pool = Arc::new(DiskWriterPool::new(2, 8)?);
    let opts = options(
        tmp.path(),
        DownloadConfig {
            connections: 1,
            // 即便调用方给了重试，流式也必须把它压成 0。
            max_retries: 5,
            ..DownloadConfig::default()
        },
        "stream.bin",
    );

    let err = download(&srv.url("/file.bin"), pool.clone(), &opts, None)
        .await
        .expect_err("streaming download must fail on the first error");
    assert!(
        !matches!(err, Error::RetryExhausted { .. }),
        "streaming mode must return the original error, not RetryExhausted: {err:?}"
    );
    assert!(
        matches!(err, Error::Http { .. }),
        "expected the original Http error from status 500, got {err:?}"
    );

    shutdown_pool(pool).await?;
    srv.shutdown();
    Ok(())
}

// ---------------------------------------------------------------------------
// 第 1 项：前 3 个响应中途断开 → 重试 3 次后成功
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retry_recovers_after_three_mid_body_breaks() -> TestResult {
    let size = 4 * MIB;
    let counter = Arc::new(AtomicU64::new(0));
    let srv = start(ServerConfig {
        size,
        break_after_bytes: Some(MIB),
        break_responses: Some(3),
        request_counter: Some(Arc::clone(&counter)),
        ..Default::default()
    })
    .await?;
    let tmp = tempdir()?;
    let pool = Arc::new(DiskWriterPool::new(2, 8)?);
    let opts = options(tmp.path(), retry_cfg(5), "three.bin");

    let (outcome, rx) = download(&srv.url("/file.bin"), pool.clone(), &opts, None).await?;
    assert_eq!(outcome, DownloadOutcome::Completed);
    verify_file(&tmp.path().join("three.bin"), size)?;
    assert_eq!(retries_of(&rx), 3, "exactly three retries must be recorded");
    assert_eq!(requests(&counter), 4, "probe + 4 chunk attempts");

    shutdown_pool(pool).await?;
    srv.shutdown();
    Ok(())
}

// ---------------------------------------------------------------------------
// 第 2 项：重试请求从"已写入的位置"继续，终点不变
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retry_resumes_from_written_offset() -> TestResult {
    let size = 4 * MIB;
    let sink = Arc::new(std::sync::Mutex::new(Vec::new()));
    let srv = start(ServerConfig {
        size,
        break_after_bytes: Some(512 * KIB),
        break_responses: Some(1),
        range_sink: Some(Arc::clone(&sink)),
        ..Default::default()
    })
    .await?;
    let tmp = tempdir()?;
    let pool = Arc::new(DiskWriterPool::new(2, 8)?);
    let opts = options(tmp.path(), retry_cfg(5), "offset.bin");

    let (outcome, _rx) = download(&srv.url("/file.bin"), pool.clone(), &opts, None).await?;
    assert_eq!(outcome, DownloadOutcome::Completed);
    verify_file(&tmp.path().join("offset.bin"), size)?;

    let ranges = sink.lock().unwrap_or_else(|e| e.into_inner()).clone();
    println!("RANGE_HEADERS={ranges:?}");
    assert_eq!(
        ranges.len(),
        3,
        "probe + first attempt + one retry: {ranges:?}"
    );
    assert_eq!(ranges[0], "bytes=0-0", "probe request");
    assert_eq!(ranges[1], format!("bytes=0-{}", size - 1), "first attempt");
    assert_eq!(
        ranges[2],
        format!("bytes={}-{}", 512 * KIB, size - 1),
        "retry must start exactly at the written offset and keep the same end"
    );

    shutdown_pool(pool).await?;
    srv.shutdown();
    Ok(())
}

// ---------------------------------------------------------------------------
// 第 3 项：响应中途停顿超过 read_timeout → 重试 1 次后成功
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retry_after_read_timeout_stall() -> TestResult {
    let size = 2 * MIB;
    let counter = Arc::new(AtomicU64::new(0));
    let srv = start(ServerConfig {
        size,
        freeze_after_bytes: Some(64 * KIB),
        freeze_duration: Some(Duration::from_secs(3)),
        freeze_responses: Some(1),
        request_counter: Some(Arc::clone(&counter)),
        ..Default::default()
    })
    .await?;
    let tmp = tempdir()?;
    let pool = Arc::new(DiskWriterPool::new(2, 8)?);
    let opts = options(
        tmp.path(),
        DownloadConfig {
            read_timeout: Duration::from_secs(1),
            ..retry_cfg(5)
        },
        "stall.bin",
    );

    let (outcome, rx) = download(&srv.url("/file.bin"), pool.clone(), &opts, None).await?;
    assert_eq!(outcome, DownloadOutcome::Completed);
    verify_file(&tmp.path().join("stall.bin"), size)?;
    assert_eq!(
        retries_of(&rx),
        1,
        "the stalled attempt must be retried once"
    );
    assert_eq!(requests(&counter), 2, "probe + 2 chunk attempts");

    shutdown_pool(pool).await?;
    srv.shutdown();
    Ok(())
}

// ---------------------------------------------------------------------------
// 第 4 项：503 + Retry-After 决定等待时长（而不是退避基数）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retry_after_header_controls_wait() -> TestResult {
    let size = MIB;
    let times = Arc::new(std::sync::Mutex::new(Vec::new()));
    let srv = start(ServerConfig {
        size,
        error_status: Some(503),
        error_responses: Some(1),
        retry_after: Some(2),
        request_times: Some(Arc::clone(&times)),
        ..Default::default()
    })
    .await?;
    let tmp = tempdir()?;
    let pool = Arc::new(DiskWriterPool::new(2, 8)?);
    // 退避基数 10 秒：若等待来自退避，间隔会是 8–12 秒。
    let opts = options(
        tmp.path(),
        DownloadConfig {
            connections: 1,
            max_retries: 5,
            retry_base_delay: Duration::from_secs(10),
            retry_max_delay: Duration::from_secs(30),
            ..DownloadConfig::default()
        },
        "retryafter.bin",
    );

    let (outcome, _rx) = download(&srv.url("/file.bin"), pool.clone(), &opts, None).await?;
    assert_eq!(outcome, DownloadOutcome::Completed);
    verify_file(&tmp.path().join("retryafter.bin"), size)?;

    let times = times.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert_eq!(times.len(), 2, "one failing request + one retry");
    let gap = times[1].duration_since(times[0]);
    println!("RETRY_AFTER_GAP_MS={}", gap.as_millis());
    assert!(
        (Duration::from_millis(1500)..=Duration::from_millis(2500)).contains(&gap),
        "Retry-After: 2 must produce a ~2s gap, got {gap:?}"
    );

    shutdown_pool(pool).await?;
    srv.shutdown();
    Ok(())
}

// ---------------------------------------------------------------------------
// 第 5 项：持续 500 → 6 次请求后 RetryExhausted → Failed → resume 完成
// ---------------------------------------------------------------------------

#[tokio::test]
async fn persistent_500_exhausts_retries_then_resume_completes() -> TestResult {
    let size = 2 * MIB;
    let switch = Arc::new(AtomicBool::new(true));
    let counter = Arc::new(AtomicU64::new(0));
    let srv = start(ServerConfig {
        size,
        error_status: Some(500),
        error_switch: Some(Arc::clone(&switch)),
        request_counter: Some(Arc::clone(&counter)),
        ..Default::default()
    })
    .await?;
    let tmp = tempdir()?;
    let store = Store::open(tmp.path().join("idm.db"))?;
    let url = srv.url("/file.bin");
    let id = store.create_task(new_task(&url, tmp.path(), 1)).await?;
    let pool = Arc::new(DiskWriterPool::new(2, 8)?);
    let opts = options(tmp.path(), retry_cfg(5), "exhaust.bin");

    let err = download_with_store(
        &url,
        Arc::clone(&pool),
        &opts,
        None,
        Some(DownloadContext::new(Arc::clone(&store), id)),
    )
    .await
    .expect_err("the server keeps answering 500");
    match &err {
        Error::RetryExhausted { attempts, .. } => {
            assert_eq!(*attempts, 6, "max_retries=5 must produce 6 attempts");
        }
        other => {
            return Err(format!("expected RetryExhausted, got {other:?}").into());
        }
    }
    assert!(
        err.to_string().contains("failed after 6 attempts"),
        "the message must carry the attempt count: {err}"
    );
    assert_eq!(requests(&counter), 6, "server must see exactly 6 requests");

    let rec = store.get_task(id).await?;
    assert_eq!(rec.state, TaskStateDb::Failed);
    assert!(
        !tmp.path().join("exhaust.bin").exists(),
        "failed download must not be renamed"
    );
    assert!(
        tmp.path().join("exhaust.bin.idm-part").exists(),
        ".idm-part must be kept"
    );

    switch.store(false, Ordering::SeqCst);
    let (outcome, _rx) = resume(id, Arc::clone(&store), Arc::clone(&pool), &opts, None).await?;
    assert_eq!(outcome, DownloadOutcome::Completed);
    assert_eq!(store.get_task(id).await?.state, TaskStateDb::Completed);
    verify_file(&tmp.path().join("exhaust.bin"), size)?;

    shutdown_pool(pool).await?;
    srv.shutdown();
    Ok(())
}

// ---------------------------------------------------------------------------
// 第 6a 项：每响应 2 MiB 后断开，但有进展 → 计数清零 → 最终成功
// ---------------------------------------------------------------------------

#[tokio::test]
async fn progress_reset_allows_repeated_breaks_to_finish() -> TestResult {
    let size = 10 * MIB;
    let srv = start(ServerConfig {
        size,
        break_after_bytes: Some(2 * MIB),
        ..Default::default()
    })
    .await?;
    let tmp = tempdir()?;
    let pool = Arc::new(DiskWriterPool::new(2, 8)?);
    // 分块大小 10 MiB = 整个文件：单连接下只有一个分块。
    let opts = options(
        tmp.path(),
        DownloadConfig {
            min_chunk_size: size,
            ..retry_cfg(5)
        },
        "progress.bin",
    );

    let (outcome, rx) = download(&srv.url("/file.bin"), pool.clone(), &opts, None).await?;
    assert_eq!(outcome, DownloadOutcome::Completed);
    verify_file(&tmp.path().join("progress.bin"), size)?;
    assert_eq!(retries_of(&rx), 4, "2 MiB per attempt → 4 retries");

    shutdown_pool(pool).await?;
    srv.shutdown();
    Ok(())
}

// ---------------------------------------------------------------------------
// 第 6b 项：每响应 1 KiB，max_retries=1000，硬上限 100 次
// ---------------------------------------------------------------------------

#[tokio::test]
async fn attempt_cap_after_unproductive_breaks() -> TestResult {
    let size = MIB;
    let counter = Arc::new(AtomicU64::new(0));
    let srv = start(ServerConfig {
        size,
        break_after_bytes: Some(KIB),
        request_counter: Some(Arc::clone(&counter)),
        ..Default::default()
    })
    .await?;
    let tmp = tempdir()?;
    let pool = Arc::new(DiskWriterPool::new(2, 8)?);
    let opts = options(
        tmp.path(),
        DownloadConfig {
            connections: 1,
            max_retries: 1000,
            retry_base_delay: Duration::from_millis(1),
            retry_max_delay: Duration::from_millis(5),
            ..DownloadConfig::default()
        },
        "cap.bin",
    );

    let err = download(&srv.url("/file.bin"), pool.clone(), &opts, None)
        .await
        .expect_err("1 KiB per response can never finish");
    match &err {
        Error::RetryExhausted { attempts, .. } => {
            assert_eq!(*attempts, 100, "the hard cap is 100 attempts");
        }
        other => {
            return Err(format!("expected RetryExhausted, got {other:?}").into());
        }
    }
    assert_eq!(
        requests(&counter),
        100,
        "server must see exactly 100 requests"
    );

    shutdown_pool(pool).await?;
    srv.shutdown();
    Ok(())
}

// ---------------------------------------------------------------------------
// 第 7 项：四种不可重试错误 → 该分块只有 1 次请求、0 次重试
// ---------------------------------------------------------------------------

/// 跑一次注定失败的下载，返回（服务器收到的分块请求数、进度里的重试次数、错误）。
async fn single_attempt(cfg: ServerConfig) -> TestResult<(u64, u64, Error)> {
    let counter = Arc::new(AtomicU64::new(0));
    let mut cfg = cfg;
    cfg.request_counter = Some(Arc::clone(&counter));
    let srv = start(cfg).await?;
    let tmp = tempdir()?;
    let pool = Arc::new(DiskWriterPool::new(2, 8)?);
    let (tx, rx) = watch::channel(ProgressSnapshot::default());
    let mut opts = options(tmp.path(), retry_cfg(5), "nonretry.bin");
    opts.progress_tx = Some(tx);

    let err = match download(&srv.url("/file.bin"), pool.clone(), &opts, None).await {
        Ok(_) => return Err("the error must not be retried".into()),
        Err(e) => e,
    };
    let seen = requests(&counter);
    let retries = retries_of(&rx);

    shutdown_pool(pool).await?;
    srv.shutdown();
    Ok((seen, retries, err))
}

#[tokio::test]
async fn non_retryable_errors_do_not_retry() -> TestResult {
    let size = 2 * MIB;

    let (seen, retries, err) = single_attempt(ServerConfig {
        size,
        etag: Some("\"v1\"".to_string()),
        change_etag_after: Some(0),
        ..Default::default()
    })
    .await?;
    assert!(matches!(err, Error::RemoteChanged), "ETag change: {err:?}");
    assert_eq!((seen, retries), (1, 0), "RemoteChanged must not retry");

    let (seen, retries, err) = single_attempt(ServerConfig {
        size,
        range: RangeMode::RangeReturn200,
        ..Default::default()
    })
    .await?;
    assert!(
        matches!(err, Error::RangeNotHonored),
        "range ignored: {err:?}"
    );
    assert_eq!((seen, retries), (1, 0), "RangeNotHonored must not retry");

    let (seen, retries, err) = single_attempt(ServerConfig {
        size,
        error_status: Some(404),
        ..Default::default()
    })
    .await?;
    assert!(
        matches!(err, Error::Http { .. }),
        "404 on a range request: {err:?}"
    );
    assert!(
        err.to_string().contains("404"),
        "the error must carry the 404 status: {err}"
    );
    assert_eq!((seen, retries), (1, 0), "404 must not retry");

    let (seen, retries, err) = single_attempt(ServerConfig {
        size,
        content_encoding: Some("gzip".to_string()),
        chunk_only_encoding: true,
        ..Default::default()
    })
    .await?;
    assert!(
        matches!(err, Error::UnsupportedContentEncoding { .. }),
        "gzip body: {err:?}"
    );
    assert_eq!((seen, retries), (1, 0), "gzip must not retry");

    Ok(())
}

// ---------------------------------------------------------------------------
// 第 8 项：退避期间取消 → 1 秒内返回 Cancelled
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancel_during_backoff_returns_cancelled_fast() -> TestResult {
    let size = 4 * MIB;
    let counter = Arc::new(AtomicU64::new(0));
    let srv = start(ServerConfig {
        size,
        break_after_bytes: Some(64 * KIB),
        break_responses: Some(1),
        request_counter: Some(Arc::clone(&counter)),
        ..Default::default()
    })
    .await?;
    let tmp = tempdir()?;
    let pool = Arc::new(DiskWriterPool::new(2, 8)?);
    // 基数 5 秒：若不响应取消，本次调用至少要等 4 秒。
    let opts = options(
        tmp.path(),
        DownloadConfig {
            connections: 1,
            max_retries: 5,
            retry_base_delay: Duration::from_secs(5),
            retry_max_delay: Duration::from_secs(30),
            ..DownloadConfig::default()
        },
        "cancel.bin",
    );

    let sig = CancelSignal::new();
    let sig_watcher = sig.clone();
    let counter_watcher = Arc::clone(&counter);
    let watcher = async move {
        for _ in 0..1000 {
            if requests(&counter_watcher) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // 第一个响应 64 KiB 后断开，100ms 后必然已进入退避等待。
        tokio::time::sleep(Duration::from_millis(100)).await;
        let at = Instant::now();
        sig_watcher.cancel();
        at
    };

    let url = srv.url("/file.bin");
    let (reply, cancelled_at) =
        tokio::join!(download(&url, pool.clone(), &opts, Some(&sig)), watcher);
    let elapsed = cancelled_at.elapsed();
    println!("CANCEL_LATENCY_MS={}", elapsed.as_millis());
    let (outcome, _rx) = reply?;
    assert_eq!(outcome, DownloadOutcome::Cancelled);
    assert!(
        elapsed < Duration::from_secs(1),
        "cancellation must take effect within 1s, took {elapsed:?}"
    );

    shutdown_pool(pool).await?;
    srv.shutdown();
    Ok(())
}

// ---------------------------------------------------------------------------
// 第 9 项：限速下的重试不得突破总带宽
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rate_limit_caps_total_speed_across_retries() -> TestResult {
    let size = 8 * MIB;
    let srv = start(ServerConfig {
        size,
        break_after_bytes: Some(MIB),
        break_responses: Some(2),
        ..Default::default()
    })
    .await?;
    let tmp = tempdir()?;
    let pool = Arc::new(DiskWriterPool::new(2, 8)?);
    let mut opts = options(
        tmp.path(),
        DownloadConfig {
            connections: 2,
            ..retry_cfg(5)
        },
        "limited.bin",
    );
    opts.max_speed = Some(2 * MIB);

    let started = Instant::now();
    let (outcome, _rx) = download(&srv.url("/file.bin"), pool.clone(), &opts, None).await?;
    let elapsed = started.elapsed();
    assert_eq!(outcome, DownloadOutcome::Completed);
    verify_file(&tmp.path().join("limited.bin"), size)?;

    let avg = size as f64 / elapsed.as_secs_f64();
    println!(
        "LIMITED_AVG_BPS={} elapsed_ms={}",
        avg as u64,
        elapsed.as_millis()
    );
    assert!(
        avg <= 2.4 * MIB as f64,
        "average speed must stay near the 2 MiB/s cap, got {avg:.0} B/s"
    );

    shutdown_pool(pool).await?;
    srv.shutdown();
    Ok(())
}

// ---------------------------------------------------------------------------
// 第 11 项：最后一次检查点磁盘满 → Paused(DiskFull)，不改名，.idm-part 保留
// ---------------------------------------------------------------------------

#[tokio::test]
async fn final_checkpoint_disk_full_pauses() -> TestResult {
    let size = 2 * MIB;
    let srv = start(ServerConfig {
        size,
        ..Default::default()
    })
    .await?;
    let tmp = tempdir()?;
    let store = Store::open(tmp.path().join("idm.db"))?;
    let url = srv.url("/file.bin");
    let id = store.create_task(new_task(&url, tmp.path(), 1)).await?;
    let pool = Arc::new(DiskWriterPool::new(2, 8)?);
    let opts = options(tmp.path(), retry_cfg(5), "diskfull.bin");

    let ctx = DownloadContext::new(Arc::clone(&store), id);
    // 让 sync 以系统错误码 112（ERROR_DISK_FULL）失败。
    ctx.hooks.fail_sync_disk_full.store(true, Ordering::SeqCst);
    let (outcome, _rx) =
        download_with_store(&url, Arc::clone(&pool), &opts, None, Some(ctx)).await?;
    assert_eq!(outcome, DownloadOutcome::Paused(PauseReason::DiskFull));

    let rec = store.get_task(id).await?;
    assert_eq!(rec.state, TaskStateDb::Paused);
    assert_eq!(rec.pause_reason, Some(PauseReasonDb::DiskFull));
    assert!(
        !tmp.path().join("diskfull.bin").exists(),
        "the final file must not be renamed on Paused(DiskFull)"
    );
    assert!(
        tmp.path().join("diskfull.bin.idm-part").exists(),
        ".idm-part must be kept"
    );

    shutdown_pool(pool).await?;
    srv.shutdown();
    Ok(())
}
