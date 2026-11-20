//! Phase 5b-4d：每任务连接闸门限制续传时的活跃分段数。
//! 全部使用本地回环服务器，不访问外网。

#![cfg(windows)]

mod support;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use idm_core::disk::{AllocationStrategy, DiskWriterPool};
use idm_core::net::HostConnectionLimiter;
use idm_core::net::client::DownloadConfig;
use idm_core::store::{ChunkRow, NewTask, PauseReasonDb, ProbeInfo, Store, TaskStateDb};
use idm_core::task::{ChunkState, DownloadOptions, DownloadOutcome, ProgressSnapshot, resume};
use support::{ServerConfig, SpeedPolicy, byte_at, start};
use tempfile::tempdir;
use tokio::sync::watch;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;

fn verify_bytes(path: &Path, size: u64) -> TestResult {
    let data = std::fs::read(path)?;
    if data.len() as u64 != size {
        return Err(format!("length mismatch: {} vs {size}", data.len()).into());
    }
    for (i, &b) in data.iter().enumerate() {
        if b != byte_at(i as u64) {
            return Err(format!("byte {i} mismatch").into());
        }
    }
    Ok(())
}

/// 续传场景：预置 8 个分段，任务 connections 被调低为 2，而每主机上限仍为 8。
/// 任务级闸门必须把「正在持许可下载」的分段数限制到 2，其余分段排队；
/// 分段总数保持不变，最终文件逐字节正确。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resumed_task_respects_connections_gate() -> TestResult {
    let size = 4 * MIB;
    let srv = start(ServerConfig {
        size,
        speed: Some(SpeedPolicy(Arc::new(|_, _| Some(256 * KIB)))),
        ..ServerConfig::default()
    })
    .await?;

    let dir = tempdir()?;
    let url = srv.url("/gate.bin");
    let store = Store::open(dir.path().join("idm.db"))?;
    let id = store
        .create_task(NewTask {
            url: url.clone(),
            dir: dir.path().to_string_lossy().into_owned(),
            strategy: "Sparse".to_string(),
            connections: 8,
        })
        .await?;

    let part = dir.path().join("gate.bin.idm-part");
    store
        .update_probe(
            id,
            ProbeInfo {
                final_url: url.clone(),
                file_name: "gate.bin".to_string(),
                part_path: part.to_string_lossy().into_owned(),
                total_size: Some(size),
                supports_range: true,
                etag: None,
                etag_weak: false,
                last_modified: None,
                resumable: true,
            },
        )
        .await?;
    let block = size / 8;
    store
        .save_chunks(
            id,
            (0u64..8)
                .map(|i| ChunkRow {
                    chunk_index: i as i64,
                    start: i * block,
                    end: (i + 1) * block,
                    committed: i * block,
                    parent: None,
                })
                .collect(),
        )
        .await?;
    std::fs::write(&part, vec![0u8; size as usize])?;
    store
        .update_state(id, TaskStateDb::Paused, Some(PauseReasonDb::User), None)
        .await?;

    let limiter = Arc::new(HostConnectionLimiter::new(8)?);
    let (tx, rx) = watch::channel(ProgressSnapshot::default());
    let opts = DownloadOptions {
        global_limiter: None,
        host_limiter: Some(Arc::clone(&limiter)),
        // 模拟用户暂停后把默认连接数改为 2 并继续：DB 中任务 connections=2。
        config: DownloadConfig {
            connections: 2,
            dynamic_split: true,
            ..DownloadConfig::default()
        },
        strategy: AllocationStrategy::Sparse,
        dir: dir.path().to_path_buf(),
        out_name: Some("gate.bin".to_string()),
        progress_tx: Some(tx),
        max_speed: None,
    };

    let handle = tokio::spawn(async move {
        resume(
            id,
            Arc::clone(&store),
            Arc::new(DiskWriterPool::new(8, 16)?),
            &opts,
            None,
        )
        .await
    });

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut samples = 0usize;
    while std::time::Instant::now() < deadline && !handle.is_finished() {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let snap = rx.borrow().clone();
        let active = snap
            .chunks
            .iter()
            .filter(|c| c.state == ChunkState::Downloading && !c.waiting_for_permit)
            .count();
        println!(
            "TASK_GATE active={active} total={} downloaded={}",
            snap.chunks.len(),
            snap.downloaded
        );
        assert!(
            active <= 2,
            "任务 connections=2 时持许可分段数不得超过 2，实际 {active}"
        );
        assert_eq!(snap.chunks.len(), 8, "续传不得重新切分：分段数应保持 8");
        samples += 1;
    }
    assert!(samples >= 2, "观测窗口内应至少有 2 次采样");

    let (outcome, _) = handle.await??;
    assert!(
        matches!(outcome, DownloadOutcome::Completed),
        "续传应正常完成"
    );
    srv.shutdown();
    verify_bytes(&dir.path().join("gate.bin"), size)?;
    Ok(())
}
