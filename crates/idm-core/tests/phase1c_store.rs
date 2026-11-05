//! Phase 1c F：数据库、检查点协议与启动恢复（全部本地，不访问外网）。

#![cfg(windows)]

mod support;

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use idm_core::disk::{AllocationStrategy, DiskWriterPool};
use idm_core::net::client::DownloadConfig;
use idm_core::store::{
    ChunkRow, NewTask, PauseReasonDb, ProbeInfo, SCHEMA_VERSION, Store, TaskStateDb,
};
use idm_core::task::{
    CheckpointHooks, DownloadContext, DownloadOptions, DownloadOutcome, download_with_store,
};
use support::{ServerConfig, start, verify_file};
use tempfile::tempdir;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;
type Snapshots = Vec<Vec<(i64, u64)>>;

fn new_task(url: &str, dir: &Path) -> NewTask {
    NewTask {
        url: url.to_string(),
        dir: dir.to_string_lossy().into_owned(),
        strategy: "Sparse".to_string(),
        connections: 4,
    }
}

fn options(dir: &Path, out: &str) -> DownloadOptions {
    DownloadOptions {
        config: DownloadConfig {
            connections: 4,
            ..DownloadConfig::default()
        },
        strategy: AllocationStrategy::Sparse,
        dir: dir.to_path_buf(),
        out_name: Some(out.to_string()),
        progress_tx: None,
        max_speed: None,
    }
}

fn probe_info(part: &Path, name: &str, total: u64) -> ProbeInfo {
    ProbeInfo {
        final_url: "http://127.0.0.1/file.bin".to_string(),
        file_name: name.to_string(),
        part_path: part.to_string_lossy().into_owned(),
        total_size: Some(total),
        supports_range: true,
        etag: None,
        etag_weak: false,
        last_modified: None,
        resumable: true,
    }
}

async fn shutdown(pool: Arc<DiskWriterPool>) -> TestResult {
    let pool = Arc::try_unwrap(pool).map_err(|_| "pool still referenced")?;
    pool.shutdown().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// F.1 迁移
// ---------------------------------------------------------------------------

#[tokio::test]
async fn migration_reaches_v1_and_is_idempotent() -> TestResult {
    let tmp = tempdir()?;
    let db = tmp.path().join("idm.db");
    let (id1, id2) = {
        let s = Store::open(&db)?;
        let a = s.create_task(new_task("http://x/a", tmp.path())).await?;
        s.shutdown().await?;
        // 已经是版本 1 的库重复打开/迁移，不应报错，也不应重置自增 ID。
        let s = Store::open(&db)?;
        let b = s.create_task(new_task("http://x/b", tmp.path())).await?;
        s.shutdown().await?;
        (a, b)
    };
    assert_eq!(id1, 1);
    assert_eq!(id2, 2);
    let conn = rusqlite::Connection::open(&db)?;
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    assert_eq!(version, SCHEMA_VERSION);
    let tables: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('tasks','chunks')",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(tables, 2);
    Ok(())
}

// ---------------------------------------------------------------------------
// F.2 检查点：只增不减；事务失败时全部分块不变
// ---------------------------------------------------------------------------

#[tokio::test]
async fn checkpoint_committed_never_decreases() -> TestResult {
    let tmp = tempdir()?;
    let s = Store::open(tmp.path().join("idm.db"))?;
    let id = s.create_task(new_task("http://x/a", tmp.path())).await?;
    s.save_chunks(
        id,
        vec![
            ChunkRow {
                chunk_index: 0,
                start: 0,
                end: 1000,
                committed: 0,
            },
            ChunkRow {
                chunk_index: 1,
                start: 1000,
                end: 2000,
                committed: 1000,
            },
        ],
    )
    .await?;

    s.checkpoint(id, &[(0, 500), (1, 1800)]).await?;
    let rec = s.get_task(id).await?;
    assert_eq!(rec.chunks[0].committed, 500);
    assert_eq!(rec.chunks[1].committed, 1800);

    // 回退的 S 不能把 committed 改小。
    s.checkpoint(id, &[(0, 300), (1, 1900)]).await?;
    let rec = s.get_task(id).await?;
    assert_eq!(
        rec.chunks[0].committed, 500,
        "committed must never decrease"
    );
    assert_eq!(rec.chunks[1].committed, 1900);
    assert_eq!(rec.committed_bytes(), 500 + 900);
    Ok(())
}

#[tokio::test]
async fn checkpoint_transaction_failure_leaves_every_chunk_unchanged() -> TestResult {
    let tmp = tempdir()?;
    let s = Store::open(tmp.path().join("idm.db"))?;
    let id = s.create_task(new_task("http://x/a", tmp.path())).await?;
    s.save_chunks(
        id,
        vec![
            ChunkRow {
                chunk_index: 0,
                start: 0,
                end: 1000,
                committed: 0,
            },
            ChunkRow {
                chunk_index: 1,
                start: 1000,
                end: 2000,
                committed: 1000,
            },
        ],
    )
    .await?;
    s.set_fail_checkpoints(true);
    let r = s.checkpoint(id, &[(0, 999), (1, 1999)]).await;
    assert!(r.is_err(), "injected transaction failure must surface");
    let rec = s.get_task(id).await?;
    assert_eq!(rec.chunks[0].committed, 0);
    assert_eq!(rec.chunks[1].committed, 1000);
    Ok(())
}

// ---------------------------------------------------------------------------
// F.3 两个 Store 实例同时使用同一个数据库
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_store_instances_share_one_database() -> TestResult {
    let tmp = tempdir()?;
    let db = tmp.path().join("idm.db");
    let a = Store::open(&db)?;
    let b = Store::open(&db)?;
    let id_a = a.create_task(new_task("http://x/a", tmp.path())).await?;
    let id_b = b.create_task(new_task("http://x/b", tmp.path())).await?;
    assert_ne!(id_a, id_b);
    let list_a = a.list_tasks().await?;
    let list_b = b.list_tasks().await?;
    assert_eq!(list_a.len(), 2);
    assert_eq!(list_b.len(), 2);
    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// F.4 / F.5 检查点协议（通过 DownloadContext 的测试钩子）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sync_failure_does_not_advance_committed() -> TestResult {
    let size = 2 * 1024 * 1024;
    let srv = start(ServerConfig {
        size,
        chunk_delay: Some(Duration::from_millis(5)),
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempdir()?;
    let store = Store::open(tmp.path().join("idm.db"))?;
    let id = store
        .create_task(new_task(&srv.url("/file.bin"), tmp.path()))
        .await?;
    let mut ctx = DownloadContext::new(Arc::clone(&store), id);
    ctx.checkpoint_interval = Duration::from_millis(50);
    ctx.hooks.fail_sync = Arc::new(AtomicBool::new(true));
    let pool = Arc::new(DiskWriterPool::new(4, 16)?);
    let r = download_with_store(
        &srv.url("/file.bin"),
        Arc::clone(&pool),
        &options(tmp.path(), "sync_fail.bin"),
        None,
        Some(ctx),
    )
    .await;
    // 1c.1 第二部分：最后一次检查点失败时不得完成，也不得改名。
    assert!(
        r.is_err(),
        "final checkpoint failure must prevent completion"
    );
    assert!(
        !tmp.path().join("sync_fail.bin").exists(),
        "must not rename .idm-part when final checkpoint fails"
    );
    verify_file(&tmp.path().join("sync_fail.bin.idm-part"), size)?;
    let rec = store.get_task(id).await?;
    assert_eq!(rec.state, TaskStateDb::Failed);
    assert!(!rec.chunks.is_empty());
    for c in &rec.chunks {
        assert_eq!(
            c.committed, c.start,
            "committed must stay at its initial value when sync fails"
        );
    }
    shutdown(pool).await?;
    srv.shutdown();
    Ok(())
}

#[tokio::test]
async fn checkpoint_never_writes_more_than_acked_snapshot() -> TestResult {
    let size = 2 * 1024 * 1024;
    let srv = start(ServerConfig {
        size,
        chunk_delay: Some(Duration::from_millis(5)),
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempdir()?;
    let store = Store::open(tmp.path().join("idm.db"))?;
    let id = store
        .create_task(new_task(&srv.url("/file.bin"), tmp.path()))
        .await?;

    let observed: Arc<Mutex<Snapshots>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&observed);
    let mut ctx = DownloadContext::new(Arc::clone(&store), id);
    ctx.checkpoint_interval = Duration::from_millis(50);
    ctx.hooks = CheckpointHooks {
        fail_sync: Arc::new(AtomicBool::new(false)),
        observe_snapshot: Some(Arc::new(move |s| {
            sink.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(s.to_vec());
        })),
        before_chunks: None,
        after_probe: None,
    };
    let pool = Arc::new(DiskWriterPool::new(4, 16)?);
    let (outcome, _rx) = download_with_store(
        &srv.url("/file.bin"),
        Arc::clone(&pool),
        &options(tmp.path(), "snap.bin"),
        None,
        Some(ctx),
    )
    .await?;
    assert_eq!(outcome, DownloadOutcome::Completed);

    let snapshots = observed.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert!(!snapshots.is_empty(), "snapshot hook must have fired");
    let rec = store.get_task(id).await?;
    for c in &rec.chunks {
        let max_s = snapshots
            .iter()
            .filter_map(|s| s.iter().find(|(i, _)| *i == c.chunk_index).map(|(_, p)| *p))
            .max()
            .unwrap_or(0);
        assert!(
            c.committed <= max_s,
            "chunk {} committed={} exceeds max snapshot {}",
            c.chunk_index,
            c.committed,
            max_s
        );
    }
    shutdown(pool).await?;
    srv.shutdown();
    Ok(())
}

// ---------------------------------------------------------------------------
// F.12 / 9.4 启动恢复
// ---------------------------------------------------------------------------

async fn make_finalizing_task(
    dir: &Path,
    store: &Store,
    name: &str,
    size: u64,
) -> TestResult<(i64, PathBuf, PathBuf)> {
    let part = dir.join(format!("{name}.idm-part"));
    let final_path = dir.join(name);
    let id = store.create_task(new_task("http://x/a", dir)).await?;
    store
        .update_probe(id, probe_info(&part, name, size))
        .await?;
    store
        .save_chunks(
            id,
            vec![ChunkRow {
                chunk_index: 0,
                start: 0,
                end: size,
                committed: size,
            }],
        )
        .await?;
    store
        .update_state(id, TaskStateDb::Finalizing, None, None)
        .await?;
    Ok((id, part, final_path))
}

#[tokio::test]
async fn recovery_downloading_becomes_paused_interrupted() -> TestResult {
    let tmp = tempdir()?;
    let store = Store::open(tmp.path().join("idm.db"))?;
    let id = store
        .create_task(new_task("http://x/a", tmp.path()))
        .await?;
    store
        .update_state(id, TaskStateDb::Downloading, None, None)
        .await?;
    let report = store.recover().await?;
    assert_eq!(report.interrupted, 1);
    let rec = store.get_task(id).await?;
    assert_eq!(rec.state, TaskStateDb::Paused);
    assert_eq!(rec.pause_reason, Some(PauseReasonDb::Interrupted));
    Ok(())
}

#[tokio::test]
async fn recovery_finalizing_before_rename_stays_finalizing() -> TestResult {
    let tmp = tempdir()?;
    let store = Store::open(tmp.path().join("idm.db"))?;
    let (id, part, _final) = make_finalizing_task(tmp.path(), &store, "a.bin", 1024).await?;
    std::fs::write(&part, vec![7u8; 1024])?;
    let report = store.recover().await?;
    assert_eq!(report.kept_finalizing, 1);
    assert_eq!(store.get_task(id).await?.state, TaskStateDb::Finalizing);
    Ok(())
}

#[tokio::test]
async fn recovery_finalizing_after_rename_becomes_completed() -> TestResult {
    let tmp = tempdir()?;
    let store = Store::open(tmp.path().join("idm.db"))?;
    let (id, _part, final_path) = make_finalizing_task(tmp.path(), &store, "b.bin", 2048).await?;
    // 重命名已完成但数据库未更新：.idm-part 不存在，最终文件存在且大小正确。
    std::fs::write(&final_path, vec![3u8; 2048])?;
    let report = store.recover().await?;
    assert_eq!(report.completed, 1);
    assert_eq!(store.get_task(id).await?.state, TaskStateDb::Completed);
    Ok(())
}

#[tokio::test]
async fn recovery_finalizing_without_any_file_becomes_failed() -> TestResult {
    let tmp = tempdir()?;
    let store = Store::open(tmp.path().join("idm.db"))?;
    let (id, _part, _final) = make_finalizing_task(tmp.path(), &store, "c.bin", 512).await?;
    let report = store.recover().await?;
    assert_eq!(report.failed, 1);
    let rec = store.get_task(id).await?;
    assert_eq!(rec.state, TaskStateDb::Failed);
    assert!(rec.last_error.is_some(), "failure reason must be recorded");
    Ok(())
}

#[tokio::test]
async fn default_db_path_is_under_localappdata() -> TestResult {
    let p = idm_core::store::default_db_path()?;
    assert!(p.ends_with(PathBuf::from("idm-rs").join("idm.db")));
    Ok(())
}
