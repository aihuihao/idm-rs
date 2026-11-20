//! Phase 5b-4c：运行中调整每主机连接上限，分块并发应立即跟随。
//! 全部使用本地回环服务器，不访问外网。

#![cfg(windows)]

mod support;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use idm_core::disk::{AllocationStrategy, DiskWriterPool};
use idm_core::net::HostConnectionLimiter;
use idm_core::net::client::DownloadConfig;
use idm_core::store::{NewTask, Store};
use idm_core::task::{
    CancelSignal, ChunkState, DownloadContext, DownloadOptions, ProgressSnapshot,
    download_with_store,
};
use support::{ServerConfig, SpeedPolicy, start};
use tempfile::{TempDir, tempdir};
use tokio::sync::watch;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;

fn new_task(url: &str, dir: &Path, connections: usize) -> NewTask {
    NewTask {
        url: url.to_string(),
        dir: dir.to_string_lossy().into_owned(),
        strategy: "Sparse".to_string(),
        connections,
    }
}

/// 调大上限：正在持许可下载的分块数应上升到 3 以上且不超过 6；
/// 分块总数不超过 max(4×6,16)=24。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn running_task_follows_raised_host_limit() -> TestResult {
    let size = 256 * MIB;
    let srv = start(ServerConfig {
        size,
        speed: Some(SpeedPolicy(Arc::new(|_, _| Some(256 * KIB)))),
        ..ServerConfig::default()
    })
    .await?;
    let tmp: TempDir = tempdir()?;
    let store = Store::open(tmp.path().join("idm.db"))?;
    let url = srv.url("/big.bin");
    let id = store.create_task(new_task(&url, tmp.path(), 8)).await?;
    let pool = Arc::new(DiskWriterPool::new(8, 16)?);
    let limiter = Arc::new(HostConnectionLimiter::new(2)?);

    let (tx, rx) = watch::channel(ProgressSnapshot::default());
    let opts = DownloadOptions {
        global_limiter: None,
        host_limiter: Some(Arc::clone(&limiter)),
        config: DownloadConfig {
            connections: 8,
            dynamic_split: true,
            ..DownloadConfig::default()
        },
        strategy: AllocationStrategy::Sparse,
        dir: tmp.path().to_path_buf(),
        out_name: Some("big.bin".to_string()),
        progress_tx: Some(tx),
        max_speed: None,
    };

    let cancel = CancelSignal::new();
    let dl_cancel = cancel.clone();
    let url_dl = url.clone();
    let handle = tokio::spawn(async move {
        download_with_store(
            &url_dl,
            pool,
            &opts,
            Some(&dl_cancel),
            Some(DownloadContext::new(Arc::clone(&store), id)),
        )
        .await
    });

    // 起始上限 2：持许可下载的分块数应 ≤ 2。
    tokio::time::sleep(Duration::from_secs(3)).await;
    let snap = rx.borrow().clone();
    let active_before = snap
        .chunks
        .iter()
        .filter(|c| c.state == ChunkState::Downloading && !c.waiting_for_permit)
        .count();
    assert!(
        active_before <= 2,
        "调大前持许可分块数应 ≤ 2，实际 {active_before}"
    );

    // 运行中调大到 6。
    limiter.set_max_per_host(6)?;

    let mut saw_above_three = false;
    for _ in 0..12 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let snap = rx.borrow().clone();
        let active = snap
            .chunks
            .iter()
            .filter(|c| c.state == ChunkState::Downloading && !c.waiting_for_permit)
            .count();
        let total = snap.chunks.len();
        println!(
            "RAISED t_active={active} total={total} downloaded={}",
            snap.downloaded
        );
        assert!(active <= 6, "持许可分块数 {} 超过新上限 6", active);
        assert!(total <= 24, "分块总数 {} 超过上限 24", total);
        if active >= 3 {
            saw_above_three = true;
        }
    }
    cancel.cancel();
    let _ = handle.await;
    srv.shutdown();
    assert!(saw_above_three, "调大后持许可分块数必须曾上升到 3 以上");
    Ok(())
}

/// 调小上限：10 秒后同时持许可的分块数 ≤ 2，且下载最终完成、文件正确。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn running_task_follows_lowered_host_limit() -> TestResult {
    let size = 64 * MIB;
    let srv = start(ServerConfig {
        size,
        // 每连接 1 MiB/s：初始 8 块各 8 MiB，约 8 秒完成，使调小后已持有块
        // 能在 10 秒观测窗口内轮转归还，欠额机制得以生效。
        speed: Some(SpeedPolicy(Arc::new(|_, _| Some(MIB)))),
        ..ServerConfig::default()
    })
    .await?;
    let tmp: TempDir = tempdir()?;
    let store = Store::open(tmp.path().join("idm.db"))?;
    let url = srv.url("/small.bin");
    let id = store.create_task(new_task(&url, tmp.path(), 8)).await?;
    let pool = Arc::new(DiskWriterPool::new(8, 16)?);
    let limiter = Arc::new(HostConnectionLimiter::new(8)?);

    let (tx, rx) = watch::channel(ProgressSnapshot::default());
    let opts = DownloadOptions {
        global_limiter: None,
        host_limiter: Some(Arc::clone(&limiter)),
        config: DownloadConfig {
            connections: 8,
            dynamic_split: true,
            ..DownloadConfig::default()
        },
        strategy: AllocationStrategy::Sparse,
        dir: tmp.path().to_path_buf(),
        out_name: Some("small.bin".to_string()),
        progress_tx: Some(tx),
        max_speed: None,
    };

    let url_dl = url.clone();
    let handle = tokio::spawn(async move {
        download_with_store(
            &url_dl,
            pool,
            &opts,
            None,
            Some(DownloadContext::new(Arc::clone(&store), id)),
        )
        .await
    });

    tokio::time::sleep(Duration::from_secs(3)).await;
    limiter.set_max_per_host(2)?;
    tokio::time::sleep(Duration::from_secs(10)).await;

    let snap = rx.borrow().clone();
    let active = snap
        .chunks
        .iter()
        .filter(|c| c.state == ChunkState::Downloading && !c.waiting_for_permit)
        .count();
    println!("LOWERED t_active={active} downloaded={}", snap.downloaded);
    assert!(active <= 2, "调小 10 秒后持许可分块数 {} 超过 2", active);

    let (outcome, _) = handle.await??;
    assert!(matches!(
        outcome,
        idm_core::task::DownloadOutcome::Completed
    ));
    srv.shutdown();

    // 下载完成后所有许可均已归还：欠额机制应使物理可用许可数从 8 降到 ≤2，
    // 而不是回到 8。这验证了调小上限真正回收了许可（而非仅靠下载结束掩盖）。
    let parsed_url = idm_core::net::Url::parse(&url)?;
    let available = limiter.available_permits(&parsed_url)?;
    println!("LOWERED final_available={available}");
    assert!(
        available <= 2,
        "调小到 2 后完成时应仅剩 ≤2 个可用许可，实际 {available}"
    );

    let final_path = tmp.path().join("small.bin");
    let meta = std::fs::metadata(&final_path)?;
    assert_eq!(meta.len(), size, "文件大小不符");
    Ok(())
}
