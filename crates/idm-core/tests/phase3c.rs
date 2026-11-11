#![cfg(windows)]

//! Phase 3c 测试：队列并发修正。
//! 全部使用本地回环服务器 / 本地 SQLite，不访问外网。

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use idm_core::disk::{AllocationStrategy, DiskWriterPool};
use idm_core::queue::{DownloadQueue, EnqueueRequest, QueueConfig, QueueEvent};
use idm_core::store::{PauseReasonDb, ProbeInfo, QueuedTask, Store, TaskStateDb};
use support::{ServerConfig, SpeedPolicy, start};
use tempfile::tempdir;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

const MIB: u64 = 1024 * 1024;

fn pool() -> TestResult<Arc<DiskWriterPool>> {
    Ok(Arc::new(DiskWriterPool::new(4, 16)?))
}

// --- 测试 3：任务启动即失败且未写状态，不得被反复拉起 -------------------

#[tokio::test]
async fn failed_before_state_write_is_not_rescheduled() -> TestResult {
    let dir = tempdir()?;
    let store = Store::open(dir.path().join("q.db"))?;
    // 1. 先以 `Queued` 入队。
    let id = store
        .enqueue_task(
            QueuedTask {
                url: "http://127.0.0.1:9/never.bin".to_string(),
                dir: dir.path().to_string_lossy().into_owned(),
                strategy: "Auto".to_string(),
                connections: 1,
                max_speed: None,
                file_name: None,
            },
            None,
        )
        .await?;

    // 2. 回填 probe 元信息，但把 `.idm-part` 指向一个不存在的文件，然后把状态
    //    改回 `Queued`。这样 `resume()` 会在读取 `.idm-part` 元数据时立刻返回
    //    错误，且不执行任何数据库状态写入（状态仍是 `Queued`）。
    store
        .update_probe(
            id,
            ProbeInfo {
                final_url: "http://127.0.0.1:9/never.bin".to_string(),
                file_name: "never.bin".to_string(),
                part_path: dir
                    .path()
                    .join("missing.idm-part")
                    .to_string_lossy()
                    .into_owned(),
                total_size: Some(MIB),
                supports_range: true,
                etag: None,
                etag_weak: false,
                last_modified: None,
                resumable: true,
            },
        )
        .await?;
    store
        .update_state(id, TaskStateDb::Queued, None, None)
        .await?;
    assert_eq!(store.get_task(id).await?.state, TaskStateDb::Queued);

    // 3. 建队列并同步订阅事件（后台调度循环要到当前任务让出后才运行）。
    let q = DownloadQueue::new(store.clone(), pool()?, QueueConfig::default());
    let mut events = q.subscribe_events();
    let started = Arc::new(AtomicU64::new(0));
    let s2 = Arc::clone(&started);
    let collector = tokio::spawn(async move {
        while let Ok(ev) = events.recv().await {
            if matches!(ev, QueueEvent::TaskStarted { .. }) {
                s2.fetch_add(1, Ordering::SeqCst);
            }
        }
    });

    // 4. 给调度器充分时间：修复前该任务会被无限反复拉起，修复后只启动一次。
    tokio::time::sleep(Duration::from_millis(500)).await;
    collector.abort();

    assert_eq!(
        started.load(Ordering::SeqCst),
        1,
        "启动即失败的任务只能被尝试 1 次"
    );
    let rec = store.get_task(id).await?;
    assert_eq!(
        rec.state,
        TaskStateDb::Failed,
        "未写状态的失败任务必须落 Failed，否则会被调度器反复拉起"
    );
    assert!(
        rec.last_error.is_some(),
        "失败任务必须记录 last_error，实际 {:?}",
        rec.last_error
    );
    q.shutdown().await?;
    Ok(())
}

// --- 测试 4：shutdown 取消活动任务、保留排队任务 -----------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_cancels_active_tasks() -> TestResult {
    let dir = tempdir()?;
    let size = 32 * MIB;
    // 1 MiB/s：32 MiB 的任务在测试期间不会自然完成，确保有 2 个持续活动的任务。
    let srv = start(ServerConfig {
        size,
        speed: Some(SpeedPolicy(Arc::new(move |_, _| Some(MIB)))),
        ..ServerConfig::default()
    })
    .await?;
    let store = Store::open(dir.path().join("q.db"))?;
    let q = DownloadQueue::new(
        store.clone(),
        pool()?,
        QueueConfig {
            max_concurrent_tasks: 2,
            ..QueueConfig::default()
        },
    );
    let mut rx = q.subscribe_events();
    let started = Arc::new(AtomicU64::new(0));
    let s2 = Arc::clone(&started);
    let collector = tokio::spawn(async move {
        while let Ok(ev) = rx.recv().await {
            if matches!(ev, QueueEvent::TaskStarted { .. }) {
                s2.fetch_add(1, Ordering::SeqCst);
            }
        }
    });

    let mut ids = Vec::new();
    for i in 0..3 {
        let task_id = q
            .enqueue(EnqueueRequest {
                url: srv.url(&format!("/f{i}.bin")),
                dir: dir.path().to_path_buf(),
                connections: 1,
                out_name: None,
                max_speed: None,
                strategy: AllocationStrategy::Auto,
            })
            .await?;
        ids.push(task_id);
    }

    // 等待恰好 2 个活动任务启动，第 3 个仍在排队。
    let deadline = Instant::now() + Duration::from_secs(10);
    while started.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        started.load(Ordering::SeqCst),
        2,
        "max_concurrent_tasks=2 时应恰好有 2 个活动任务"
    );
    assert_eq!(
        store.get_task(ids[2]).await?.state,
        TaskStateDb::Queued,
        "第 3 个任务应仍在排队"
    );

    // shutdown：先取消所有活动任务，再等待它们收尾；不得拉起排队任务。
    match tokio::time::timeout(Duration::from_secs(10), q.shutdown()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => panic!("shutdown 挂起"),
    }
    collector.abort();

    for id in &ids[..2] {
        let rec = store.get_task(*id).await?;
        assert_eq!(
            rec.state,
            TaskStateDb::Paused,
            "活动任务 {id} 被取消后应变为 Paused，实际 {:?}",
            rec.state
        );
        assert_eq!(
            rec.pause_reason,
            Some(PauseReasonDb::User),
            "活动任务 {id} 的暂停原因应为 User"
        );
    }
    let queued_rec = store.get_task(ids[2]).await?;
    assert_eq!(
        queued_rec.state,
        TaskStateDb::Queued,
        "未被启动的排队任务必须保持 Queued"
    );

    srv.shutdown();
    Ok(())
}
