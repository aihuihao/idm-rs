#![cfg(windows)]

//! Phase 4a 测试：请求头透传 / 迁移 V4 / Cookie 不泄露。
//! 全部测试使用本地回环服务器，不访问外网。

mod support;

#[path = "support/verify.rs"]
mod verify;

use std::sync::Arc;
use std::time::Duration;

use idm_core::disk::DiskWriterPool;
use idm_core::net::client::DownloadConfig;
use idm_core::net::client::validate_extra_headers;
use idm_core::queue::{DownloadQueue, QueueConfig};
use idm_core::store::{QueuedTask, Store, TaskStateDb};
use support::{HeaderSink, ServerConfig, start};
use tempfile::tempdir;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

const MIB: u64 = 1024 * 1024;

fn pool() -> Result<Arc<DiskWriterPool>, Box<dyn std::error::Error>> {
    Ok(Arc::new(DiskWriterPool::new(4, 16)?))
}

fn headers_vec() -> Vec<(String, String)> {
    vec![
        ("Referer".to_string(), "http://example.com/page".to_string()),
        (
            "Cookie".to_string(),
            "secret_session=abcd1234xyz".to_string(),
        ),
        (
            "User-Agent".to_string(),
            "Mozilla/5.0 (idm-rs-test)".to_string(),
        ),
    ]
}

async fn run_queue_to_idle(
    store: Arc<Store>,
    task: QueuedTask,
    headers: Option<Vec<(String, String)>>,
) -> TestResult {
    let q = DownloadQueue::new(
        Arc::clone(&store),
        pool()?,
        QueueConfig {
            max_concurrent_tasks: 4,
            global_max_speed: None,
            default_download_config: DownloadConfig::default(),
            default_strategy: idm_core::disk::AllocationStrategy::Auto,
        },
    );
    store.enqueue_task(task, headers).await?;
    q.adopt_queued().await?;
    let _summary = q.wait_for_idle().await?;
    Ok(())
}

/// 仅接纳并运行队列中已有任务（不额外入队），用于校验「入队由调用方完成」的场景。
async fn run_queue(store: Arc<Store>) -> TestResult {
    let q = DownloadQueue::new(
        Arc::clone(&store),
        pool()?,
        QueueConfig {
            max_concurrent_tasks: 4,
            global_max_speed: None,
            default_download_config: DownloadConfig::default(),
            default_strategy: idm_core::disk::AllocationStrategy::Auto,
        },
    );
    q.adopt_queued().await?;
    let _summary = q.wait_for_idle().await?;
    Ok(())
}

// --- 10：core 拒绝非法请求头 -------------------------------------------

#[test]
fn invalid_headers_rejected_in_core() -> TestResult {
    // 非白名单名字
    assert!(validate_extra_headers(&[("X-Evil".to_string(), "v".to_string())]).is_err());
    // 含换行
    assert!(validate_extra_headers(&[("Cookie".to_string(), "a\r\nb".to_string())]).is_err());
    // 超过 8 KiB
    let big = "x".repeat(8 * 1024 + 1);
    assert!(validate_extra_headers(&[("Cookie".to_string(), big)]).is_err());
    // 合法
    assert!(validate_extra_headers(&headers_vec()).is_ok());
    // 错误信息不得包含 Cookie 值：用一个确实会被拒（含 CR/LF）且带密钥的 Cookie。
    let err = validate_extra_headers(&[(
        "Cookie".to_string(),
        "secret_session=abcd1234xyz\r\n".to_string(),
    )])
    .err()
    .ok_or("expected validation error")?;
    let msg = format!("{err:?}");
    assert!(
        !msg.contains("abcd1234xyz"),
        "错误信息不得泄露 Cookie 值: {msg}"
    );
    Ok(())
}

// --- 14：迁移 V4 ------------------------------------------------------

#[tokio::test]
async fn migration_v4_creates_tables_and_is_idempotent() -> TestResult {
    let dir = tempdir()?;
    let db = dir.path().join("v4.db");
    let store = Store::open(&db)?;
    // 确认 V4 表存在：直接对两张表做查询/写入不应报错。
    // settings 表：读写设置。
    store.set_setting("download-dir", "C:\\tmp").await?;
    let v = store.get_setting("download-dir").await?;
    assert_eq!(v.as_deref(), Some("C:\\tmp"));
    // task_headers 表：对不存在的任务读取应为空、不报错。
    let hdr = store.get_task_headers(999).await?;
    assert!(hdr.is_empty(), "task_headers 表必须存在并可查询: {hdr:?}");
    // 幂等：再次打开不得出错
    drop(store);
    let store2 = Store::open(&db)?;
    let _id = store2
        .enqueue_task(
            QueuedTask {
                url: "http://h/x.bin".into(),
                dir: dir.path().to_string_lossy().into_owned(),
                strategy: "Auto".into(),
                connections: 4,
                max_speed: None,
                file_name: None,
            },
            None,
        )
        .await?;
    // 设置与读取
    store2.set_setting("download-dir", "C:\\tmp").await?;
    let v = store2.get_setting("download-dir").await?;
    assert_eq!(v.as_deref(), Some("C:\\tmp"));
    Ok(())
}

// --- 7：请求头到达服务器（Probe + 分块，含重试）------------------------

#[tokio::test]
async fn headers_reach_server_including_retry() -> TestResult {
    let dir = tempdir()?;
    let size = 2 * MIB;
    let hsink: HeaderSink = Default::default();
    let srv = start(ServerConfig {
        size,
        // 第一个非 Probe 分块请求返回错误，触发客户端重试。
        error_responses: Some(1),
        header_sink: Some(Arc::clone(&hsink)),
        ..ServerConfig::default()
    })
    .await?;
    let store = Store::open(dir.path().join("h.db"))?;
    let task = QueuedTask {
        url: srv.url("/file.bin"),
        dir: dir.path().to_string_lossy().into_owned(),
        strategy: "Auto".into(),
        connections: 2,
        max_speed: None,
        file_name: None,
    };
    run_queue_to_idle(Arc::clone(&store), task, Some(headers_vec())).await?;
    let headers = hsink.lock().unwrap().clone();
    let by_name = |n: &str| {
        headers
            .iter()
            .filter(|(k, _)| k == n)
            .map(|(_, v)| v.clone())
            .collect::<Vec<_>>()
    };
    assert!(
        by_name("cookie")
            .iter()
            .any(|v| v.contains("secret_session=abcd1234xyz")),
        "Cookie 必须到达服务器: {headers:?}"
    );
    assert!(
        by_name("referer")
            .iter()
            .any(|v| v.contains("example.com/page")),
        "Referer 必须到达服务器: {headers:?}"
    );
    assert!(
        by_name("user-agent")
            .iter()
            .any(|v| v.contains("idm-rs-test")),
        "User-Agent 必须到达服务器: {headers:?}"
    );
    // 重试：至少出现 2 条带 Cookie 的请求（初始 + 重试）
    let cookie_reqs = by_name("cookie").len();
    assert!(
        cookie_reqs >= 2,
        "重试请求也应带 Cookie，实际 {cookie_reqs}"
    );
    verify::verify_file(&dir.path().join("file.bin"), size)?;
    srv.shutdown();
    Ok(())
}

// --- 7b：续传请求也带请求头 ------------------------------------------

#[tokio::test]
async fn headers_reach_server_on_resume() -> TestResult {
    let dir = tempdir()?;
    let size = 8 * MIB;
    let hsink: HeaderSink = Default::default();
    let srv = start(ServerConfig {
        size,
        speed: Some(support::SpeedPolicy(Arc::new(|_, _| Some(MIB)))), // 1 MiB/s，慢速便于暂停
        header_sink: Some(Arc::clone(&hsink)),
        ..ServerConfig::default()
    })
    .await?;
    let store = Store::open(dir.path().join("r.db"))?;
    let task = QueuedTask {
        url: srv.url("/r.bin"),
        dir: dir.path().to_string_lossy().into_owned(),
        strategy: "Auto".into(),
        connections: 1,
        max_speed: None,
        file_name: None,
    };
    let id = store.enqueue_task(task, Some(headers_vec())).await?;
    let q = DownloadQueue::new(
        store.clone(),
        pool()?,
        QueueConfig {
            max_concurrent_tasks: 1,
            global_max_speed: None,
            default_download_config: DownloadConfig::default(),
            default_strategy: idm_core::disk::AllocationStrategy::Auto,
        },
    );
    q.adopt_queued().await?;
    // 等一小段时间让它开始并写入部分数据。
    tokio::time::sleep(Duration::from_millis(400)).await;
    q.pause(id).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    // 续传
    q.resume_queued(id, false).await?;
    let _summary = q.wait_for_idle().await?;
    let headers = hsink.lock().unwrap().clone();
    let cookies = headers
        .iter()
        .filter(|(k, _)| k == "cookie")
        .map(|(_, v)| v.clone())
        .collect::<Vec<_>>();
    assert!(
        cookies
            .iter()
            .any(|v| v.contains("secret_session=abcd1234xyz")),
        "续传请求也应带 Cookie: {headers:?}"
    );
    verify::verify_file(&dir.path().join("r.bin"), size)?;
    srv.shutdown();
    Ok(())
}

// --- 8：任务完成后清除请求头 ------------------------------------------

#[tokio::test]
async fn headers_cleared_on_completion() -> TestResult {
    let dir = tempdir()?;
    let size = MIB;
    let srv = start(ServerConfig {
        size,
        ..ServerConfig::default()
    })
    .await?;
    let store = Store::open(dir.path().join("c.db"))?;
    let task = QueuedTask {
        url: srv.url("/c.bin"),
        dir: dir.path().to_string_lossy().into_owned(),
        strategy: "Auto".into(),
        connections: 1,
        max_speed: None,
        file_name: None,
    };
    let id = store.enqueue_task(task, Some(headers_vec())).await?;
    // 入队后应能读到
    assert!(
        !store.get_task_headers(id).await?.is_empty(),
        "入队后应有请求头"
    );
    run_queue(Arc::clone(&store)).await?;
    // 任务完成，请求头应被清除
    let remaining = store.get_task_headers(id).await?;
    assert!(
        remaining.is_empty(),
        "完成后 task_headers 必须为空: {remaining:?}"
    );
    let state = store.get_task(id).await?.state;
    assert_eq!(state, TaskStateDb::Completed);
    srv.shutdown();
    Ok(())
}

// --- 9：Cookie 值不进入日志 ------------------------------------------

#[tokio::test]
async fn cookie_not_in_logs() -> TestResult {
    let dir = tempdir()?;
    let size = MIB;
    let log_path = dir.path().join("test.log");
    install_log_capture(&log_path)?;
    let srv = start(ServerConfig {
        size,
        ..ServerConfig::default()
    })
    .await?;
    let store = Store::open(dir.path().join("l.db"))?;
    let task = QueuedTask {
        url: srv.url("/l.bin"),
        dir: dir.path().to_string_lossy().into_owned(),
        strategy: "Auto".into(),
        connections: 1,
        max_speed: None,
        file_name: None,
    };
    run_queue_to_idle(Arc::clone(&store), task, Some(headers_vec())).await?;
    // 同时确认 DownloadConfig 的 Debug 也遮蔽 Cookie 值。
    let cfg = DownloadConfig {
        extra_headers: headers_vec(),
        ..DownloadConfig::default()
    };
    let dbg = format!("{cfg:?}");
    assert!(
        !dbg.contains("abcd1234xyz"),
        "DownloadConfig Debug 不得包含 Cookie 值: {dbg}"
    );
    srv.shutdown();
    // 读取日志文件，断言 Cookie 值不出现。
    drop(store);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        !log.contains("abcd1234xyz"),
        "日志中不得出现 Cookie 值。日志片段: {}",
        log.chars().take(500).collect::<String>()
    );
    Ok(())
}

/// 安装一个把 tracing 事件写入指定文件的订阅者（进程级，仅首个调用生效）。
fn install_log_capture(path: &std::path::Path) -> TestResult {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let _ = tracing_subscriber::fmt()
        .with_writer(std::sync::Mutex::new(file))
        .with_ansi(false)
        .with_target(false)
        .try_init();
    Ok(())
}
