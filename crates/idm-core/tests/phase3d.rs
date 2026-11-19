#![cfg(windows)]

//! Phase 3d / 5b-4b 集成测试：主机许可排队与接管的配合。
//! 全部使用本地回环服务器，不访问外网。

mod support;

#[path = "support/verify.rs"]
mod verify;

use std::sync::Arc;
use std::time::Duration;

use idm_core::disk::{AllocationStrategy, DiskWriterPool};
use idm_core::net::client::DownloadConfig;
use idm_core::store::{ChunkRow, NewTask, PauseReasonDb, ProbeInfo, Store, TaskStateDb};
use idm_core::task::{DownloadOptions, DownloadOutcome, ProgressSnapshot, resume};
use support::{ServerConfig, start};
use tempfile::tempdir;
use tokio::sync::watch;
use verify::verify_file;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

const MIB: u64 = 1024 * 1024;

/// 主机名额饱和时不得拆分、也不得接管任何排队块。
///
/// Phase 3d 原测试验证的是"饱和时只接管持许可的块、排队块不被接管"，其前提是
/// 饱和状态下仍会发生拆分。Phase 5b-4b 起，饱和时不再拆分：此时拆出的新分块
/// 只能排队，无法加速，反而正是分块失控的起点。故新不变量是"饱和时既不拆分
/// 也不接管"。许可未饱和时的卡住分块接管由 phase2 的
/// `unaligned_stalled_first_chunk_is_taken_over` / `zero_progress_stalled_first_chunk_is_taken_over`
/// 等测试覆盖。
///
/// 构造：预置 4 个各 2 MiB 的分块（此前以 4 连接下载过），现在每主机上限为 2，
/// 以续传方式恢复；服务端让前 2 个请求各停 6 秒，于是 2 块持名额、2 块排队。
/// 整个下载每 200ms 采样：split_count/takeover_count 恒为 0；排队块的 end
/// 从未被缩小（从未被接管）；最终完成且文件逐字节正确。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn saturated_host_does_not_split_or_take_over_queued() -> TestResult {
    let size = 8 * MIB;
    let srv = start(ServerConfig {
        size,
        response_delay: Some((2, Duration::from_secs(6))),
        ..ServerConfig::default()
    })
    .await?;

    let dir = tempdir()?;
    let url = srv.url("/file.bin");
    let store = Store::open(dir.path().join("idm.db"))?;
    let id = store
        .create_task(NewTask {
            url: url.clone(),
            dir: dir.path().to_string_lossy().into_owned(),
            strategy: "Sparse".to_string(),
            connections: 4,
        })
        .await?;

    // 回填 probe 元信息并预置 4 个各 2 MiB 的分块（committed=start，尚未落盘），
    // 模拟"此前以 4 连接下载过、现在以每主机上限 2 续传"。
    let part = dir.path().join("file.bin.idm-part");
    store
        .update_probe(
            id,
            ProbeInfo {
                final_url: url.clone(),
                file_name: "file.bin".to_string(),
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
    let chunk = size / 4;
    store
        .save_chunks(
            id,
            (0u64..4)
                .map(|i| ChunkRow {
                    chunk_index: i as i64,
                    start: i * chunk,
                    end: (i + 1) * chunk,
                    committed: i * chunk,
                    parent: None,
                })
                .collect(),
        )
        .await?;

    // .idm-part 必须存在且长度等于 total_size；committed==start 表示尚未落盘。
    std::fs::write(&part, vec![0u8; size as usize])?;
    store
        .update_state(id, TaskStateDb::Paused, Some(PauseReasonDb::User), None)
        .await?;

    let (tx, rx) = watch::channel(ProgressSnapshot::default());
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
        out_name: Some("file.bin".to_string()),
        progress_tx: Some(tx),
        max_speed: None,
    };

    let dl_store = Arc::clone(&store);
    let dl_pool = Arc::new(DiskWriterPool::new(4, 16)?);
    let dl_pool_for_task = Arc::clone(&dl_pool);
    let handle =
        tokio::spawn(async move { resume(id, dl_store, dl_pool_for_task, &opts, None).await });
    drop(dl_pool);

    // 初始 end 表：db_index -> 原始 end。排队块若被接管，其 end 会被缩小。
    let original_ends: Vec<(i64, u64)> = (0u64..4).map(|i| (i as i64, (i + 1) * chunk)).collect();
    // 下载过程中每 200ms 采样 split/takeover 计数，以及对节流快照中可见的
    // 排队块校验 end 未被缩小。publish_if_due 只在写盘时触发，6 秒停顿期间
    // 节流快照不会更新，因此"是否确实出现排队块"改由最终的强制发布快照判定。
    while !handle.is_finished() {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let snap = rx.borrow().clone();
        assert_eq!(
            snap.split_count, 0,
            "主机名额饱和时不得拆分，实际 split_count={}",
            snap.split_count
        );
        assert_eq!(
            snap.takeover_count, 0,
            "主机名额饱和时不得接管，实际 takeover_count={}",
            snap.takeover_count
        );
        for c in &snap.chunks {
            if c.waiting_for_permit
                && let Some((_, original_end)) =
                    original_ends.iter().find(|(idx, _)| *idx == c.db_index)
            {
                assert_eq!(
                    c.end, *original_end,
                    "排队分块 {} 的 end 被缩小（{} -> {}）：排队块被接管了",
                    c.db_index, original_end, c.end
                );
            }
        }
    }

    let (outcome, final_rx) = handle.await??;
    // 最终快照由 publish_final 强制发布，含停顿期间同步的 waiting_for_permit。
    let final_snap = final_rx.borrow();
    assert_eq!(final_snap.split_count, 0, "最终快照 split_count 必须为 0");
    assert_eq!(
        final_snap.takeover_count, 0,
        "最终快照 takeover_count 必须为 0"
    );
    for c in &final_snap.chunks {
        if c.waiting_for_permit
            && let Some((_, original_end)) =
                original_ends.iter().find(|(idx, _)| *idx == c.db_index)
        {
            assert_eq!(
                c.end, *original_end,
                "排队分块 {} 的 end 被缩小（{} -> {}）：排队块被接管了",
                c.db_index, original_end, c.end
            );
        }
    }
    drop(final_snap);
    assert_eq!(
        outcome,
        DownloadOutcome::Completed,
        "饱和时不拆分/不接管时任务仍应完成"
    );
    verify_file(&dir.path().join("file.bin"), size)?;
    srv.shutdown();
    Ok(())
}
