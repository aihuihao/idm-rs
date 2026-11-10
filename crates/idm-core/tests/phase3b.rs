#![cfg(windows)]

//! Phase 3b B 部分测试：多任务下载队列与调度器。
//! 全部测试使用本地回环服务器，不访问外网。

mod support;

#[path = "support/verify.rs"]
mod verify;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use idm_core::disk::AllocationStrategy;
use idm_core::disk::DiskWriterPool;
use idm_core::net::client::DownloadConfig;
use idm_core::queue::{DownloadQueue, EnqueueRequest, QueueConfig, QueueEvent, QueueMove};
use idm_core::store::{NewTask, PauseReasonDb, QueuedTask, Store, TaskStateDb};
use support::{ServerConfig, SpeedPolicy, start};
use tempfile::Builder as TempfileBuilder;
use tempfile::tempdir;
use tokio::sync::Mutex as AsyncMutex;
use verify::verify_file;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

const MIB: u64 = 1024 * 1024;

fn pool() -> Result<Arc<DiskWriterPool>, Box<dyn std::error::Error>> {
    Ok(Arc::new(DiskWriterPool::new(4, 16)?))
}

/// 计算 idm-cli 二进制路径（集成测试通过子进程调用）。
fn cli_bin() -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let exe = std::env::current_exe()?;
    let name = if cfg!(windows) {
        "idm-cli.exe"
    } else {
        "idm-cli"
    };
    let bin = exe
        .parent()
        .ok_or_else(|| std::io::Error::other("current_exe has no parent"))?
        .parent()
        .ok_or_else(|| std::io::Error::other("current_exe has no parent"))?
        .join(name);
    Ok(bin)
}

/// 启动事件收集器，记录 `TaskStarted` 的顺序与并发峰值（in-flight 峰值）。
fn start_collector(
    q: &DownloadQueue,
) -> (
    tokio::task::JoinHandle<()>,
    Arc<AsyncMutex<Vec<i64>>>,
    Arc<AtomicU64>,
) {
    let mut rx = q.subscribe_events();
    let order = Arc::new(AsyncMutex::new(Vec::new()));
    let max_inflight = Arc::new(AtomicU64::new(0));
    let o2 = order.clone();
    let m2 = max_inflight.clone();
    let h = tokio::spawn(async move {
        let mut inflight = 0u64;
        while let Ok(ev) = rx.recv().await {
            match ev {
                QueueEvent::TaskStarted { id } => {
                    o2.lock().await.push(id);
                    inflight += 1;
                    let _ = m2.fetch_max(inflight, Ordering::SeqCst);
                }
                QueueEvent::TaskCompleted { .. }
                | QueueEvent::TaskPaused { .. }
                | QueueEvent::TaskFailed { .. } => {
                    inflight = inflight.saturating_sub(1);
                }
                _ => {}
            }
        }
    });
    (h, order, max_inflight)
}

// --- B5.1：Schema V3 迁移与启动恢复 ------------------------------------

#[tokio::test]
async fn migration_v2_to_v3_and_recover_preserves_queued() -> TestResult {
    let dir = tempdir()?;
    let db = dir.path().join("q.db");
    let store = Store::open(&db)?; // 打开即应用 V3 迁移（新增 queue_order / max_speed 列与 Queued 状态）。
    let q_id = store
        .enqueue_task(QueuedTask {
            url: "http://h/q.bin".into(),
            dir: dir.path().to_string_lossy().into_owned(),
            strategy: "Auto".into(),
            connections: 8,
            max_speed: None,
        })
        .await?;
    let d_id = store
        .create_task(NewTask {
            url: "http://h/d.bin".into(),
            dir: dir.path().to_string_lossy().into_owned(),
            strategy: "Auto".into(),
            connections: 8,
        })
        .await?;
    store
        .update_state(d_id, TaskStateDb::Downloading, None, None)
        .await?;
    let before = store.get_task(q_id).await?;
    assert_eq!(before.state, TaskStateDb::Queued);
    let qo = before.queue_order;

    let _report = store.recover().await?;

    let q = store.get_task(q_id).await?;
    assert_eq!(q.state, TaskStateDb::Queued, "recover 必须保留 Queued 任务");
    assert_eq!(q.queue_order, qo, "recover 不得改变 queue_order");
    let d = store.get_task(d_id).await?;
    assert_eq!(d.state, TaskStateDb::Paused, "正在下载的任务应被中断");
    assert_eq!(d.pause_reason, Some(PauseReasonDb::Interrupted));
    Ok(())
}

// --- B5.2：并发任务上限与自动接续 --------------------------------------

#[tokio::test]
async fn queue_respects_max_concurrent_tasks_and_fifo_order() -> TestResult {
    let dir = tempdir()?;
    let size = 4 * MIB;
    let max_active = Arc::new(AtomicU64::new(0));
    let srv = start(ServerConfig {
        size,
        max_active: Some(Arc::clone(&max_active)),
        ..ServerConfig::default()
    })
    .await?;
    let store = Store::open(dir.path().join("q.db"))?;
    let q = DownloadQueue::new(
        store.clone(),
        pool()?,
        QueueConfig {
            max_concurrent_tasks: 2,
            global_max_speed: None,
            default_download_config: DownloadConfig::default(),
            default_strategy: AllocationStrategy::Auto,
        },
    );
    let (h, order, max_inflight) = start_collector(&q);
    for i in 0..5 {
        q.enqueue(EnqueueRequest {
            url: srv.url(&format!("/file{i}.bin")),
            dir: dir.path().to_path_buf(),
            connections: 1,
            out_name: None,
            max_speed: None,
            strategy: AllocationStrategy::Auto,
        })
        .await?;
    }
    let summary = q.wait_for_idle().await?;
    h.abort();
    let order = order.lock().await.clone();
    let max_inflight = max_inflight.load(Ordering::SeqCst);
    assert_eq!(
        order,
        vec![1, 2, 3, 4, 5],
        "必须严格按入队顺序 FIFO 启动，实际 {order:?}"
    );
    assert_eq!(max_inflight, 2, "活动任务并发峰值必须严格 == 2");
    assert_eq!(summary.completed, 5);
    let peak = max_active.load(Ordering::SeqCst);
    assert!(peak <= 3, "服务器端并发峰值应 <= 3（实际 {peak}）");
    for i in 0..5 {
        verify_file(&dir.path().join(format!("file{i}.bin")), size)?;
    }
    srv.shutdown();
    Ok(())
}

// --- B5.3：暂停活动任务自动拉起排队任务 + 续传恢复 --------------------

#[tokio::test]
async fn pause_active_task_starts_next_queued_and_resume_completes() -> TestResult {
    let dir = tempdir()?;
    let a_size = 8 * MIB;
    let a_sent = Arc::new(AtomicU64::new(0));
    let a_ranges = Arc::new(std::sync::Mutex::new(Vec::new()));
    let srv_a = start(ServerConfig {
        size: a_size,
        speed: Some(SpeedPolicy(Arc::new(move |_, _| Some(MIB)))), // 1 MiB/s，慢速
        bytes_sent: Some(Arc::clone(&a_sent)),
        chunk_range_sink: Some(Arc::clone(&a_ranges)),
        ..ServerConfig::default()
    })
    .await?;
    let srv_b = start(ServerConfig {
        size: MIB, // 快速、小文件
        ..ServerConfig::default()
    })
    .await?;
    let store = Store::open(dir.path().join("q.db"))?;
    let q = DownloadQueue::new(
        store.clone(),
        pool()?,
        QueueConfig {
            max_concurrent_tasks: 1,
            global_max_speed: None,
            default_download_config: DownloadConfig::default(),
            default_strategy: AllocationStrategy::Auto,
        },
    );
    let a_id = q
        .enqueue(EnqueueRequest {
            url: srv_a.url("/a.bin"),
            dir: dir.path().to_path_buf(),
            connections: 1,
            out_name: None,
            max_speed: None,
            strategy: AllocationStrategy::Auto,
        })
        .await?;
    let b_id = q
        .enqueue(EnqueueRequest {
            url: srv_b.url("/b.bin"),
            dir: dir.path().to_path_buf(),
            connections: 1,
            out_name: None,
            max_speed: None,
            strategy: AllocationStrategy::Auto,
        })
        .await?;
    // 等任务 A 开始下载并写入部分数据。
    tokio::time::sleep(Duration::from_millis(400)).await;
    q.pause(a_id).await?;
    let a_state = store.get_task(a_id).await?.state;
    assert_eq!(
        a_state,
        TaskStateDb::Paused,
        "暂停的活动任务应变为 Paused(User)"
    );
    // 暂停 A 后调度器应立即拉起 B；poll 至多 2s 等待其被拉起（pause 返回早于调度器真正启动 B）。
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut b_state = store.get_task(b_id).await?.state;
    while !(matches!(b_state, TaskStateDb::Completed)
        || matches!(b_state, TaskStateDb::Downloading))
    {
        if Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        b_state = store.get_task(b_id).await?.state;
    }
    assert!(
        matches!(b_state, TaskStateDb::Completed) || matches!(b_state, TaskStateDb::Downloading),
        "任务 B 应立即被拉起，实际 {b_state:?}"
    );
    // 恢复 A 从断点续传。
    q.resume_queued(a_id, false).await?;
    let summary = q.wait_for_idle().await?;
    assert_eq!(summary.completed, 2);
    verify_file(&dir.path().join("a.bin"), a_size)?;
    verify_file(&dir.path().join("b.bin"), MIB)?;
    // 断点续传：服务器 A 不应被重下（总发送远小于 2 倍文件大小）。
    let sent = a_sent.load(Ordering::SeqCst);
    assert!(
        sent <= a_size + MIB,
        "A 应通过 Range 续传，总发送 {sent} 不得接近 2*size"
    );
    // 仅初始第一次请求 Range 起点为 0，证明没有从头重下。
    let zero_starts = a_ranges
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|r| r.contains("bytes=0-"))
        .count();
    assert_eq!(
        zero_starts, 1,
        "续传不应出现第二次从头下载，实际 {zero_starts} 次 bytes=0-"
    );
    srv_a.shutdown();
    srv_b.shutdown();
    Ok(())
}

// --- B5.4：队列重排（优先级调整）-------------------------------------

#[tokio::test]
async fn queue_reordering_changes_execution_order() -> TestResult {
    let dir = tempdir()?;
    let size = 2 * MIB;
    let srv = start(ServerConfig {
        size,
        speed: Some(SpeedPolicy(Arc::new(move |_, _| Some(MIB)))), // 1 MiB/s，每任务 2s
        ..ServerConfig::default()
    })
    .await?;
    let store = Store::open(dir.path().join("q.db"))?;
    let q = DownloadQueue::new(
        store.clone(),
        pool()?,
        QueueConfig {
            max_concurrent_tasks: 1,
            global_max_speed: None,
            default_download_config: DownloadConfig::default(),
            default_strategy: AllocationStrategy::Auto,
        },
    );
    let (h, order, _mi) = start_collector(&q);
    for i in 1..=4 {
        q.enqueue(EnqueueRequest {
            url: srv.url(&format!("/t{i}.bin")),
            dir: dir.path().to_path_buf(),
            connections: 1,
            out_name: None,
            max_speed: None,
            strategy: AllocationStrategy::Auto,
        })
        .await?;
    }
    // 等任务 1 开始后再重排。
    tokio::time::sleep(Duration::from_millis(300)).await;
    q.move_queue(4, QueueMove::Top).await?;
    q.move_queue(2, QueueMove::Down).await?;
    let summary = q.wait_for_idle().await?;
    h.abort();
    let order = order.lock().await.clone();
    assert_eq!(
        order,
        vec![1, 4, 3, 2],
        "重排后启动顺序应为 1->4->3->2，实际 {order:?}"
    );
    assert_eq!(summary.completed, 4);
    for i in 1..=4 {
        verify_file(&dir.path().join(format!("t{i}.bin")), size)?;
    }
    srv.shutdown();
    Ok(())
}

// --- B5.5：运行中动态调整最大并发任务数 ------------------------------

#[tokio::test]
async fn dynamic_max_concurrent_tasks_scales_up_and_down() -> TestResult {
    let dir = tempdir()?;
    let size = MIB;
    let srv = start(ServerConfig {
        size,
        speed: Some(SpeedPolicy(Arc::new(move |_, _| Some(MIB)))), // 1 MiB/s
        ..ServerConfig::default()
    })
    .await?;
    let store = Store::open(dir.path().join("q.db"))?;
    let q = DownloadQueue::new(
        store.clone(),
        pool()?,
        QueueConfig {
            max_concurrent_tasks: 1,
            global_max_speed: None,
            default_download_config: DownloadConfig::default(),
            default_strategy: AllocationStrategy::Auto,
        },
    );
    let (h, order, _mi) = start_collector(&q);
    for i in 0..4 {
        q.enqueue(EnqueueRequest {
            url: srv.url(&format!("/t{i}.bin")),
            dir: dir.path().to_path_buf(),
            connections: 1,
            out_name: None,
            max_speed: None,
            strategy: AllocationStrategy::Auto,
        })
        .await?;
    }
    // 初始 max=1：只有 1 个在跑。
    tokio::time::sleep(Duration::from_millis(150)).await;
    let t0 = Instant::now();
    q.set_max_concurrent_tasks(3)?;
    // 100ms 内活动任务数应升到 3。
    tokio::time::sleep(Duration::from_millis(120)).await;
    let total = order.lock().await.len();
    assert!(
        total >= 3,
        "100ms 内应有 >=3 个任务启动（升速立即生效），实际 {total}"
    );
    assert!(
        t0.elapsed() <= Duration::from_millis(200),
        "升速应在 100ms 内生效，实际 {:?}",
        t0.elapsed()
    );
    // 调小到 1：已在跑的 3 个不被强杀，全部完成。
    q.set_max_concurrent_tasks(1)?;
    let summary = q.wait_for_idle().await?;
    h.abort();
    assert_eq!(summary.completed, 4, "4 个任务都应正常完成");
    assert_eq!(summary.paused, 0, "调小并发不得强杀任务");
    for i in 0..3 {
        verify_file(&dir.path().join(format!("t{i}.bin")), size)?;
    }
    srv.shutdown();
    Ok(())
}

// --- B5.6：队列共享全局限速与 Host 连接限制 ---------------------------

#[tokio::test]
async fn queue_shares_global_limiter_and_host_limiter() -> TestResult {
    let dir = tempdir()?;
    let size = 2 * MIB;
    let max_active = Arc::new(AtomicU64::new(0));
    let srv = start(ServerConfig {
        size,
        max_active: Some(Arc::clone(&max_active)),
        ..ServerConfig::default() // 无 speed 限制；限速由队列全局限速控制
    })
    .await?;
    let store = Store::open(dir.path().join("q.db"))?;
    let q = DownloadQueue::new(
        store.clone(),
        pool()?,
        QueueConfig {
            max_concurrent_tasks: 3,
            global_max_speed: Some(2 * MIB),
            default_download_config: DownloadConfig {
                max_connections_per_host: 5,
                ..DownloadConfig::default()
            },
            default_strategy: AllocationStrategy::Auto,
        },
    );
    for i in 0..3 {
        q.enqueue(EnqueueRequest {
            url: srv.url(&format!("/t{i}.bin")),
            dir: dir.path().to_path_buf(),
            connections: 4,
            out_name: None,
            max_speed: None,
            strategy: AllocationStrategy::Auto,
        })
        .await?;
    }
    let started = Instant::now();
    let summary = q.wait_for_idle().await?;
    let elapsed = started.elapsed();
    let peak = max_active.load(Ordering::SeqCst);
    assert!(peak <= 5, "每主机并发连接峰值必须 <= 5（实际 {peak}）");
    assert!(
        elapsed >= Duration::from_secs(2) + Duration::from_millis(500),
        "6 MiB @ 2 MiB/s 应 >= 2.5s，实际 {elapsed:?}"
    );
    assert!(
        elapsed <= Duration::from_secs(4),
        "应 <= 4s，实际 {elapsed:?}"
    );
    assert_eq!(summary.completed, 3);
    for i in 0..3 {
        verify_file(&dir.path().join(format!("t{i}.bin")), size)?;
    }
    srv.shutdown();
    Ok(())
}

// --- B5.7：CLI 队列子命令端到端 --------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn cli_enqueue_move_and_queue_run() -> TestResult {
    let cwd = std::env::current_dir()?;
    let dir = TempfileBuilder::new()
        .prefix("idm-cli-test")
        .tempdir_in(&cwd)?;
    let size = MIB;
    let srv = start(ServerConfig {
        size,
        ..ServerConfig::default()
    })
    .await?;
    let bin = cli_bin()?;
    assert!(bin.exists(), "idm-cli 二进制必须已构建: {bin:?}");
    let db = dir.path().join("q.db");
    let db_str = db.display().to_string();
    let dir_str = dir.path().display().to_string();

    // 入队 3 个任务（不同路径，同一服务器）。
    let mut ids = Vec::new();
    for i in 0..3 {
        let out = std::process::Command::new(&bin)
            .args([
                "enqueue",
                &srv.url(&format!("/f{i}.bin")),
                "--dir",
                &dir_str,
                "--connections",
                "4",
                "--db",
                &db_str,
            ])
            .output()?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "enqueue 应成功: stdout={stdout} stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        let id: i64 = stdout
            .lines()
            .find_map(|l| {
                l.strip_prefix("ENQUEUED id=")
                    .and_then(|s| s.split_whitespace().next())
                    .and_then(|s| s.parse::<i64>().ok())
            })
            .ok_or("解析 enqueue 输出的任务 id")?;
        ids.push(id);
    }
    // 调整顺序：把第 1 个入队任务移到队尾。
    let out = std::process::Command::new(&bin)
        .args([
            "queue",
            "move",
            &ids[0].to_string(),
            "bottom",
            "--db",
            &db_str,
        ])
        .output()?;
    assert!(
        out.status.success(),
        "queue move 应成功: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // 运行队列（max-tasks=2）。
    let out = std::process::Command::new(&bin)
        .args(["queue", "run", "--max-tasks", "2", "--db", &db_str])
        .output()?;
    assert!(
        out.status.success(),
        "queue run 应成功: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    for i in 0..3 {
        verify_file(&dir.path().join(format!("f{i}.bin")), size)?;
    }
    srv.shutdown();
    Ok(())
}
