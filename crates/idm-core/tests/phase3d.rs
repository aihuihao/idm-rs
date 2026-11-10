#![cfg(windows)]

//! Phase 3d 集成测试：Host 名额排队与接管的配合。
//! 全部使用本地回环服务器，不访问外网。

mod support;

#[path = "support/verify.rs"]
mod verify;

use std::sync::Arc;
use std::time::Duration;

use idm_core::disk::{AllocationStrategy, DiskWriterPool};
use idm_core::net::client::DownloadConfig;
use idm_core::store::{NewTask, Store};
use idm_core::task::{DownloadContext, DownloadOptions, DownloadOutcome, download_with_store};
use support::{ServerConfig, start};
use tempfile::tempdir;
use verify::verify_file;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

const MIB: u64 = 1024 * 1024;

/// `max_per_host = 2`、4 个连接：服务器让前 2 个请求各停 6 秒，另外 2 个分块
/// 只能排队等名额。排队分块不得被当作「卡住」接管（否则它们卡在 acquire 里
/// 收不到 end 缩小的通知，`PendingChunk` 会在 5 秒后超时导致任务失败）。
/// 断言：任务成功、只接管了持有名额的分块、最终文件逐字节正确。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_chunk_is_not_taken_over() -> TestResult {
    let size = 8 * MIB;
    let srv = start(ServerConfig {
        size,
        response_delay: Some((2, Duration::from_secs(6))),
        ..ServerConfig::default()
    })
    .await?;
    let dir = tempdir()?;
    let store = Store::open(dir.path().join("idm.db"))?;
    let id = store
        .create_task(NewTask {
            url: srv.url("/file.bin"),
            dir: dir.path().to_string_lossy().into_owned(),
            strategy: "Sparse".to_string(),
            connections: 4,
        })
        .await?;
    let pool = Arc::new(DiskWriterPool::new(4, 16)?);
    let out = "file.bin";
    let opts = DownloadOptions {
        global_limiter: None,
        host_limiter: None,
        config: DownloadConfig {
            connections: 4,
            dynamic_split: true,
            max_connections_per_host: 2,
            ..DownloadConfig::default()
        },
        strategy: AllocationStrategy::Sparse,
        dir: dir.path().to_path_buf(),
        out_name: Some(out.to_string()),
        progress_tx: None,
        max_speed: None,
    };
    let (outcome, rx) = download_with_store(
        &srv.url("/file.bin"),
        pool,
        &opts,
        None,
        Some(DownloadContext::new(Arc::clone(&store), id)),
    )
    .await?;
    assert_eq!(
        outcome,
        DownloadOutcome::Completed,
        "排队中的分块若被接管会触发 5 秒接管超时，任务不应失败"
    );
    let snap = rx.borrow().clone();
    assert!(
        (1..=2).contains(&snap.takeover_count),
        "只有持有名额、已发出请求的前 2 个分块可被接管，实际 {}: {:?}",
        snap.takeover_count,
        snap.chunks
    );
    verify_file(&dir.path().join(out), size)?;
    srv.shutdown();
    Ok(())
}
