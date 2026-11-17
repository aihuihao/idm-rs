#![cfg(windows)]

//! `idm_core::runner::Runner` 的集成测试。
//!
//! 覆盖两条启动不变量：
//! 1. `Runner::start` 拿到 runner.lock 之后执行 recover，遗留的 `Downloading`
//!    任务（无占用）被改为 `Paused(Interrupted)`。
//! 2. runner.lock 被别的运行器独占时，`Runner::start` 直接返回 `AlreadyRunning`，
//!    不执行 recover，任务状态保持不变。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use idm_core::net::ProxyConfig;
use idm_core::runner::{Runner, RunnerConfig, RunnerStartError};
use idm_core::store::{NewTask, PauseReasonDb, Store, TaskStateDb};
use tempfile::tempdir;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

fn new_task(url: &str, dir: &Path) -> NewTask {
    NewTask {
        url: url.to_string(),
        dir: dir.to_string_lossy().into_owned(),
        strategy: "Sparse".to_string(),
        connections: 4,
    }
}

fn runner_cfg(db: &Path) -> RunnerConfig {
    RunnerConfig {
        db_path: db.to_path_buf(),
        max_tasks: 2,
        host_max_connections: 4,
        proxy: Some(ProxyConfig::Disabled),
    }
}

fn lock_path(db: &Path) -> PathBuf {
    db.parent()
        .unwrap_or_else(|| Path::new("."))
        .join("runner.lock")
}

/// 以 share_mode(0) 独占打开 runner.lock，模拟另一个运行器正在运行。
fn acquire_lock_exclusive(db: &Path) -> TestResult<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(lock_path(db))?;
    Ok(f)
}

#[tokio::test]
async fn start_recovers_downloading_task_to_paused_interrupted() -> TestResult {
    let tmp = tempdir()?;
    let db = tmp.path().join("idm.db");
    let store = Store::open(&db)?;

    let id = store
        .create_task(new_task("http://127.0.0.1/a", tmp.path()))
        .await?;
    store
        .update_state(id, TaskStateDb::Downloading, None, None)
        .await?;
    assert_eq!(store.get_task(id).await?.state, TaskStateDb::Downloading);

    let runner = Runner::start(Arc::clone(&store), runner_cfg(&db)).await?;
    // start 内部（拿到锁之后）执行 recover：无 part_path 占用的 Downloading → Paused(Interrupted)。
    let rec = store.get_task(id).await?;
    assert_eq!(rec.state, TaskStateDb::Paused);
    assert_eq!(rec.pause_reason, Some(PauseReasonDb::Interrupted));

    runner.stop().await?;
    Ok(())
}

#[tokio::test]
async fn start_returns_already_running_without_recover_when_lock_held() -> TestResult {
    let tmp = tempdir()?;
    let db = tmp.path().join("idm.db");
    let store = Store::open(&db)?;

    let id = store
        .create_task(new_task("http://127.0.0.1/b", tmp.path()))
        .await?;
    store
        .update_state(id, TaskStateDb::Downloading, None, None)
        .await?;

    // 另一个运行器独占持有 runner.lock。
    let _guard = acquire_lock_exclusive(&db)?;

    let started = Instant::now();
    let err = match Runner::start(Arc::clone(&store), runner_cfg(&db)).await {
        Ok(runner) => {
            runner.stop().await?;
            return Err("expected AlreadyRunning but start succeeded".into());
        }
        Err(e) => e,
    };
    assert!(
        matches!(err, RunnerStartError::AlreadyRunning),
        "expected AlreadyRunning, got {err:?}"
    );
    // 3 秒重试窗口：确认确实走完重试而不是秒退。
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "too fast: {started:?}"
    );

    // 拿不到锁时不执行 recover，任务状态保持 Downloading。
    let rec = store.get_task(id).await?;
    assert_eq!(rec.state, TaskStateDb::Downloading);
    assert_eq!(rec.pause_reason, None);
    Ok(())
}
