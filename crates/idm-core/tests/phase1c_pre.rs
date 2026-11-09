//! Phase 1c A 部分：分块汇总表所需的进度字段（已写入字节数、耗时、平均速度、最终状态）。
//! 全部使用本地测试服务器，不访问外网。

#![cfg(windows)]

mod support;

#[path = "support/verify.rs"]
mod verify;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use idm_core::disk::{AllocationStrategy, DiskWriterPool};
use idm_core::net::client::DownloadConfig;
use idm_core::task::state::ChunkState;
use idm_core::task::{CancelSignal, DownloadOptions, DownloadOutcome, download};
use support::{ServerConfig, start};
use tempfile::tempdir;
use verify::verify_file;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

fn options(dir: &Path, out: &str) -> DownloadOptions {
    DownloadOptions {
        global_limiter: None,
        host_limiter: None,
        config: DownloadConfig {
            connections: 8,
            ..DownloadConfig::default()
        },
        strategy: AllocationStrategy::Sparse,
        dir: dir.to_path_buf(),
        out_name: Some(out.to_string()),
        progress_tx: None,
        max_speed: None,
    }
}

async fn shutdown(pool: Arc<DiskWriterPool>) -> TestResult {
    let pool = Arc::try_unwrap(pool).map_err(|_| "pool still referenced")?;
    pool.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn completed_chunks_report_written_elapsed_speed_and_state() -> TestResult {
    // 8 MiB / 默认最小分块 1 MiB → 8 个分块。
    let size = 8 * 1024 * 1024;
    let srv = start(ServerConfig {
        size,
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempdir()?;
    let pool = Arc::new(DiskWriterPool::new(4, 16)?);
    let (outcome, rx) = download(
        &srv.url("/file.bin"),
        pool.clone(),
        &options(tmp.path(), "summary.bin"),
        None,
    )
    .await?;
    assert_eq!(outcome, DownloadOutcome::Completed);

    let snap = rx.borrow().clone();
    // Phase 2.2.1：动态切分（含对明显慢分块的接管）可能额外产生分块，
    // 因此这里要求初始的 8 个分块都在，而不是恰好 8 个；其余断言
    // （终态、耗时、速度、覆盖字节数、文件逐字节正确）保持不变。
    assert!(
        snap.chunks.len() >= 8,
        "初始应有 8 个分块: split_count={} takeover_count={} chunks={:?}",
        snap.split_count,
        snap.takeover_count,
        snap.chunks
    );
    let mut written_total = 0u64;
    for c in &snap.chunks {
        assert_eq!(c.state, ChunkState::Done, "chunk ended in {:?}", c.state);
        assert!(c.elapsed_secs > 0.0, "elapsed must be recorded");
        assert!(c.avg_speed > 0.0, "avg_speed must be recorded");
        written_total += c.current.saturating_sub(c.start);
    }
    assert_eq!(written_total, size, "written bytes must sum to file size");
    verify_file(&tmp.path().join("summary.bin"), size)?;

    shutdown(pool).await?;
    srv.shutdown();
    Ok(())
}

#[tokio::test]
async fn cancelled_chunks_report_cancelled_terminal_state_and_timing() -> TestResult {
    let size = 8 * 1024 * 1024;
    let srv = start(ServerConfig {
        size,
        chunk_delay: Some(Duration::from_millis(20)),
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempdir()?;
    let pool = Arc::new(DiskWriterPool::new(4, 16)?);
    let sig = Arc::new(CancelSignal::new());
    let s = sig.clone();
    let cancel_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        s.cancel();
    });

    let (outcome, rx) = download(
        &srv.url("/file.bin"),
        pool.clone(),
        &options(tmp.path(), "cancelled.bin"),
        Some(sig.as_ref()),
    )
    .await?;
    assert_eq!(outcome, DownloadOutcome::Cancelled);

    let snap = rx.borrow().clone();
    assert_eq!(snap.chunks.len(), 8);
    assert!(
        snap.chunks.iter().any(|c| c.state == ChunkState::Cancelled),
        "at least one chunk must end as Cancelled"
    );
    assert!(
        snap.chunks
            .iter()
            .all(|c| c.state != ChunkState::Downloading),
        "no chunk may remain in Downloading after cancellation"
    );
    assert!(
        snap.chunks.iter().all(|c| c.elapsed_secs > 0.0),
        "timing must be recorded for cancelled chunks too"
    );

    cancel_task.await?;
    shutdown(pool).await?;
    srv.shutdown();
    Ok(())
}
