//! Phase 5b-4b 回归测试：主机连接受限时分块拆分不得失控。
//! 全部使用本地回环服务器，不访问外网。

#![cfg(windows)]

mod support;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use idm_core::disk::{AllocationStrategy, DiskWriterPool};
use idm_core::net::HostConnectionLimiter;
use idm_core::net::client::DownloadConfig;
use idm_core::store::{NewTask, Store};
use idm_core::task::{
    CancelSignal, ChunkState, DownloadContext, DownloadOptions, download_with_store,
};
use support::{ServerConfig, SpeedPolicy, TestServer, start};
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

/// 限速确认：服务器单连接 256 KiB/s 下载 2 MiB，耗时必须 ≥ 6 秒（理论 8 秒）。
#[tokio::test]
async fn server_speed_limit_is_effective() -> TestResult {
    let size = 2 * MIB;
    let srv = start(ServerConfig {
        size,
        speed: Some(SpeedPolicy(Arc::new(|_, _| Some(256 * KIB)))),
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempdir()?;
    let store = Store::open(tmp.path().join("idm.db"))?;
    let id = store
        .create_task(new_task(&srv.url("/limited.bin"), tmp.path(), 1))
        .await?;
    let pool = Arc::new(DiskWriterPool::new(1, 4)?);
    let opts = DownloadOptions {
        global_limiter: None,
        host_limiter: None,
        config: DownloadConfig {
            connections: 1,
            dynamic_split: false,
            ..DownloadConfig::default()
        },
        strategy: AllocationStrategy::Sparse,
        dir: tmp.path().to_path_buf(),
        out_name: Some("limited.bin".to_string()),
        progress_tx: None,
        max_speed: None,
    };
    let started = Instant::now();
    let (outcome, _rx) = download_with_store(
        &srv.url("/limited.bin"),
        pool,
        &opts,
        None,
        Some(DownloadContext::new(Arc::clone(&store), id)),
    )
    .await?;
    let elapsed = started.elapsed();
    srv.shutdown();
    assert!(
        elapsed >= Duration::from_secs(6),
        "服务器限速未生效：256 KiB/s 下载 2 MiB 仅耗时 {:?}（应 ≥ 6s）",
        elapsed
    );
    let _ = outcome;
    Ok(())
}

/// 主测试：32 连接、每主机 4、无限速拆分时，分块数不得失控。
///
/// 上限说明：有效并发 = min(connections, 每主机上限) = min(32,4) = 4。
/// 分块总数 ≤ 2*4 + 2 = 10（+2 余量给拆分瞬间的待分配/调度延迟）；
/// 从未开始下载（downloaded==0）的分块数 ≤ 4。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn split_does_not_run_away_when_host_limited() -> TestResult {
    let size = 64 * MIB;
    let srv: TestServer = start(ServerConfig {
        size,
        speed: Some(SpeedPolicy(Arc::new(|_, _| Some(128 * KIB)))),
        ..ServerConfig::default()
    })
    .await?;
    let tmp: TempDir = tempdir()?;
    let store = Store::open(tmp.path().join("idm.db"))?;
    let id = store
        .create_task(new_task(&srv.url("/big.bin"), tmp.path(), 32))
        .await?;
    let pool = Arc::new(DiskWriterPool::new(32, 64)?);

    let (tx, rx) = watch::channel(idm_core::task::ProgressSnapshot::default());
    let opts = DownloadOptions {
        global_limiter: None,
        host_limiter: None,
        config: DownloadConfig {
            connections: 32,
            dynamic_split: true,
            max_connections_per_host: 4,
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
    let url = srv.url("/big.bin");
    let handle = tokio::spawn(async move {
        download_with_store(
            &url,
            pool,
            &opts,
            Some(&dl_cancel),
            Some(DownloadContext::new(Arc::clone(&store), id)),
        )
        .await
    });

    let effective = 4usize; // min(32, 4)
    let max_total = 2 * effective + 2;
    let max_not_started = effective;

    for tick in 1..=10 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let snap = rx.borrow().clone();
        let total = snap.chunks.len();
        let not_started = snap
            .chunks
            .iter()
            .filter(|c| c.state == ChunkState::Downloading && c.current == c.start)
            .count();
        println!(
            "t={}s chunks={} not_started={} downloaded={}",
            tick * 2,
            total,
            not_started,
            snap.downloaded
        );
        assert!(
            total <= max_total,
            "t={}s 分块数 {} 超过上限 {}",
            tick * 2,
            total,
            max_total
        );
        assert!(
            not_started <= max_not_started,
            "t={}s 未开始下载的分块数 {} 超过上限 {}",
            tick * 2,
            not_started,
            max_not_started
        );
    }

    cancel.cancel();
    let _ = handle.await;
    srv.shutdown();
    Ok(())
}

/// 队列注入路径：显式注入上限=4 的 HostConnectionLimiter，但任务 config 的
/// max_connections_per_host 保持默认 8（模拟「队列注入的 limiter 与任务 config
/// 不一致」）。修复后所有并发口径都必须以注入 limiter 的 4 为准：分块数 ≤ 10、
/// 未开始块 ≤ 4。采样 10 秒。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queue_path_split_bounded() -> TestResult {
    let size = 64 * MIB;
    let srv: TestServer = start(ServerConfig {
        size,
        speed: Some(SpeedPolicy(Arc::new(|_, _| Some(128 * KIB)))),
        ..ServerConfig::default()
    })
    .await?;
    let tmp: TempDir = tempdir()?;
    let store = Store::open(tmp.path().join("idm.db"))?;
    let id = store
        .create_task(new_task(&srv.url("/big.bin"), tmp.path(), 32))
        .await?;
    let pool = Arc::new(DiskWriterPool::new(32, 64)?);

    // 关键：注入的 limiter 上限=4，但 config.max_connections_per_host 保持默认 8。
    let injected_limiter = Arc::new(HostConnectionLimiter::new(4)?);
    let (tx, rx) = watch::channel(idm_core::task::ProgressSnapshot::default());
    let opts = DownloadOptions {
        global_limiter: None,
        host_limiter: Some(injected_limiter),
        config: DownloadConfig {
            connections: 32,
            dynamic_split: true,
            // 特意保持默认 8，与注入 limiter 的 4 不同。
            max_connections_per_host: 8,
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
    let url = srv.url("/big.bin");
    let handle = tokio::spawn(async move {
        download_with_store(
            &url,
            pool,
            &opts,
            Some(&dl_cancel),
            Some(DownloadContext::new(Arc::clone(&store), id)),
        )
        .await
    });

    let max_total = 10usize; // 2*4 + 2
    let max_not_started = 4usize;
    // 首次采样（下载刚起步）用于观察初始分块数是否按注入 limiter=4 生成。
    tokio::time::sleep(Duration::from_millis(500)).await;
    {
        let snap = rx.borrow().clone();
        println!(
            "t=0.5s initial_chunks={} not_started={}",
            snap.chunks.len(),
            snap.chunks
                .iter()
                .filter(|c| c.state == ChunkState::Downloading && c.current == c.start)
                .count()
        );
    }
    for tick in 1..=5 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let snap = rx.borrow().clone();
        let total = snap.chunks.len();
        let not_started = snap
            .chunks
            .iter()
            .filter(|c| c.state == ChunkState::Downloading && c.current == c.start)
            .count();
        println!(
            "t={}s chunks={} not_started={} downloaded={}",
            tick * 2,
            total,
            not_started,
            snap.downloaded
        );
        assert!(
            total <= max_total,
            "t={}s 分块数 {} 超过上限 {}（注入 limiter=4，config=8）",
            tick * 2,
            total,
            max_total
        );
        assert!(
            not_started <= max_not_started,
            "t={}s 未开始下载的分块数 {} 超过上限 {}",
            tick * 2,
            not_started,
            max_not_started
        );
    }

    cancel.cancel();
    let _ = handle.await;
    srv.shutdown();
    Ok(())
}
