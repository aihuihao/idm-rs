//! Phase 1c F：断点续传（全部使用本地测试服务器，不访问外网）。

#![cfg(windows)]

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use idm_core::disk::{AllocationStrategy, DiskWriterPool};
use idm_core::error::Error;
use idm_core::net::client::DownloadConfig;
use idm_core::store::{ChunkRow, NewTask, PauseReasonDb, ProbeInfo, Store, TaskStateDb};
use idm_core::task::{
    CancelSignal, DownloadContext, DownloadOptions, DownloadOutcome, download_with_store, resume,
};
use support::{ServerConfig, TestServer, start, verify_file};
use tempfile::{TempDir, tempdir};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

const MIB: u64 = 1024 * 1024;

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

struct Half {
    _tmp: TempDir,
    srv: TestServer,
    store: Arc<Store>,
    pool: Arc<DiskWriterPool>,
    db_path: PathBuf,
    part_path: PathBuf,
    id: i64,
    size: u64,
    out: String,
}

impl Half {
    fn dir(&self) -> &Path {
        self._tmp.path()
    }
    fn opts(&self) -> DownloadOptions {
        options(self.dir(), &self.out)
    }
}

impl Drop for Half {
    fn drop(&mut self) {
        self.srv.shutdown();
    }
}

/// 下载到一半取消：任务应停在 Paused(User)，且 committed 严格介于 0 与总长之间。
async fn half_download(size: u64, out: &str) -> TestResult<Half> {
    let srv = start(ServerConfig {
        size,
        chunk_delay: Some(Duration::from_millis(30)),
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempdir()?;
    let db_path = tmp.path().join("idm.db");
    let store = Store::open(&db_path)?;
    let id = store
        .create_task(new_task(&srv.url("/file.bin"), tmp.path()))
        .await?;
    let mut ctx = DownloadContext::new(Arc::clone(&store), id);
    ctx.checkpoint_interval = Duration::from_millis(100);
    let pool = Arc::new(DiskWriterPool::new(4, 16)?);
    let sig = Arc::new(CancelSignal::new());
    let s = Arc::clone(&sig);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        s.cancel();
    });
    let (outcome, _rx) = download_with_store(
        &srv.url("/file.bin"),
        Arc::clone(&pool),
        &options(tmp.path(), out),
        Some(sig.as_ref()),
        Some(ctx),
    )
    .await?;
    assert_eq!(outcome, DownloadOutcome::Cancelled);
    let rec = store.get_task(id).await?;
    assert_eq!(rec.state, TaskStateDb::Paused);
    assert_eq!(rec.pause_reason, Some(PauseReasonDb::User));
    let committed = rec.committed_bytes();
    assert!(
        committed > 0 && committed < size,
        "committed={committed}, size={size}"
    );
    Ok(Half {
        part_path: tmp.path().join(format!("{out}.idm-part")),
        _tmp: tmp,
        srv,
        store,
        pool,
        db_path,
        id,
        size,
        out: out.to_string(),
    })
}

// ---------------------------------------------------------------------------
// F.6 暂停后续传，逐字节正确
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resume_after_pause_produces_byte_exact_file() -> TestResult {
    let h = half_download(8 * MIB, "paused.bin").await?;
    let before = h.store.get_task(h.id).await?.committed_bytes();
    let (outcome, _rx) = resume(
        h.id,
        Arc::clone(&h.store),
        Arc::clone(&h.pool),
        &h.opts(),
        None,
    )
    .await?;
    assert_eq!(outcome, DownloadOutcome::Completed);
    let rec = h.store.get_task(h.id).await?;
    assert_eq!(rec.state, TaskStateDb::Completed);
    assert!(rec.committed_bytes() >= before);
    verify_file(&h.dir().join(&h.out), h.size)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// F.7 远端 ETag 变化 → RemoteChanged，.idm-part 保留
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resume_detects_remote_etag_change() -> TestResult {
    let h = half_download(8 * MIB, "etag.bin").await?;
    {
        let conn = rusqlite::Connection::open(&h.db_path)?;
        conn.execute(
            "UPDATE tasks SET etag='\"changed-by-test\"' WHERE id=?1",
            [h.id],
        )?;
    }
    let r = resume(
        h.id,
        Arc::clone(&h.store),
        Arc::clone(&h.pool),
        &h.opts(),
        None,
    )
    .await;
    match r {
        Err(Error::RemoteChanged) => {}
        Err(e) => return Err(format!("expected RemoteChanged, got {e:?}").into()),
        Ok(_) => return Err("expected RemoteChanged, resume succeeded".into()),
    }
    assert_eq!(h.store.get_task(h.id).await?.state, TaskStateDb::Failed);
    assert!(
        h.part_path.exists(),
        ".idm-part must be kept on RemoteChanged"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// F.8 .idm-part 丢失 / 长度不符 → 明确错误，状态不变
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resume_missing_part_returns_error_and_keeps_state() -> TestResult {
    let h = half_download(8 * MIB, "missing.bin").await?;
    std::fs::remove_file(&h.part_path)?;
    let r = resume(
        h.id,
        Arc::clone(&h.store),
        Arc::clone(&h.pool),
        &h.opts(),
        None,
    )
    .await;
    match r {
        Err(Error::Resume { .. }) => {}
        Err(e) => return Err(format!("expected Resume error, got {e:?}").into()),
        Ok(_) => return Err("expected Resume error, resume succeeded".into()),
    }
    assert_eq!(h.store.get_task(h.id).await?.state, TaskStateDb::Paused);
    Ok(())
}

#[tokio::test]
async fn resume_wrong_part_length_returns_error_and_keeps_state() -> TestResult {
    let h = half_download(8 * MIB, "short.bin").await?;
    let file = std::fs::OpenOptions::new().write(true).open(&h.part_path)?;
    file.set_len(123)?;
    drop(file);
    let r = resume(
        h.id,
        Arc::clone(&h.store),
        Arc::clone(&h.pool),
        &h.opts(),
        None,
    )
    .await;
    match r {
        Err(Error::Resume { .. }) => {}
        Err(e) => return Err(format!("expected Resume error, got {e:?}").into()),
        Ok(_) => return Err("expected Resume error, resume succeeded".into()),
    }
    assert_eq!(h.store.get_task(h.id).await?.state, TaskStateDb::Paused);
    Ok(())
}

// ---------------------------------------------------------------------------
// F.9 不可续传任务 → 从头下载
// ---------------------------------------------------------------------------

#[tokio::test]
async fn non_resumable_task_restarts_from_scratch() -> TestResult {
    let size = 4 * MIB;
    let srv = start(ServerConfig {
        size,
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempdir()?;
    let store = Store::open(tmp.path().join("idm.db"))?;
    let part = tmp.path().join("restart.bin.idm-part");
    std::fs::write(&part, vec![0u8; size as usize])?;
    let id = store
        .create_task(new_task(&srv.url("/file.bin"), tmp.path()))
        .await?;
    store
        .update_probe(
            id,
            ProbeInfo {
                final_url: srv.url("/file.bin"),
                file_name: "restart.bin".to_string(),
                part_path: part.to_string_lossy().into_owned(),
                total_size: Some(size),
                supports_range: false,
                etag: None,
                etag_weak: false,
                last_modified: None,
                resumable: false,
            },
        )
        .await?;
    store
        .save_chunks(
            id,
            vec![ChunkRow {
                chunk_index: 0,
                start: 0,
                end: size,
                committed: 0,
            }],
        )
        .await?;
    store
        .update_state(id, TaskStateDb::Paused, None, None)
        .await?;
    let pool = Arc::new(DiskWriterPool::new(4, 16)?);
    let (outcome, _rx) = resume(
        id,
        Arc::clone(&store),
        Arc::clone(&pool),
        &options(tmp.path(), "restart.bin"),
        None,
    )
    .await?;
    assert_eq!(outcome, DownloadOutcome::Completed);
    assert!(
        !part.exists(),
        "old .idm-part must be removed before restart"
    );
    verify_file(&tmp.path().join("restart.bin"), size)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// F.10 失败（中断）后续传
// ---------------------------------------------------------------------------

#[tokio::test]
async fn failed_task_can_be_resumed() -> TestResult {
    let h = half_download(8 * MIB, "failed.bin").await?;
    // 把取消后的 Paused(User) 改为 Failed，模拟中途失败后的状态。
    h.store
        .update_state(
            h.id,
            TaskStateDb::Failed,
            None,
            Some("simulated mid-transfer failure".into()),
        )
        .await?;
    let (outcome, _rx) = resume(
        h.id,
        Arc::clone(&h.store),
        Arc::clone(&h.pool),
        &h.opts(),
        None,
    )
    .await?;
    assert_eq!(outcome, DownloadOutcome::Completed);
    assert_eq!(h.store.get_task(h.id).await?.state, TaskStateDb::Completed);
    verify_file(&h.dir().join(&h.out), h.size)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 任务第 3 点确认：Probe 之前失败、没有 part_path 的任务重新走完整流程
// ---------------------------------------------------------------------------

#[tokio::test]
async fn failed_task_without_part_path_runs_full_probe_and_download() -> TestResult {
    let size = 4 * MIB;
    let srv = start(ServerConfig {
        size,
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempdir()?;
    let store = Store::open(tmp.path().join("idm.db"))?;
    let id = store
        .create_task(new_task(&srv.url("/file.bin"), tmp.path()))
        .await?;
    // 只写了 url/dir/strategy/connections/state=Pending；Probe 失败后应为 Failed 且无 part_path。
    store
        .update_state(id, TaskStateDb::Failed, None, Some("probe failed".into()))
        .await?;
    let rec = store.get_task(id).await?;
    assert!(rec.part_path.is_none());
    assert!(rec.file_name.is_none());
    let pool = Arc::new(DiskWriterPool::new(4, 16)?);
    let (outcome, _rx) = resume(
        id,
        Arc::clone(&store),
        Arc::clone(&pool),
        &options(tmp.path(), "probe_retry.bin"),
        None,
    )
    .await?;
    assert_eq!(outcome, DownloadOutcome::Completed);
    let rec = store.get_task(id).await?;
    assert_eq!(rec.state, TaskStateDb::Completed);
    assert!(rec.part_path.is_some());
    verify_file(&tmp.path().join("probe_retry.bin"), size)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 任务第 2 点确认：Finalizing 的 resume 先校验再直接完成重命名
// ---------------------------------------------------------------------------

async fn finalizing_fixture(
    size: u64,
    out: &str,
    committed: u64,
) -> TestResult<(
    TempDir,
    TestServer,
    Arc<Store>,
    Arc<DiskWriterPool>,
    PathBuf,
    i64,
)> {
    let srv = start(ServerConfig {
        size,
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempdir()?;
    let store = Store::open(tmp.path().join("idm.db"))?;
    let part = tmp.path().join(format!("{out}.idm-part"));
    std::fs::write(&part, vec![0u8; size as usize])?;
    let id = store
        .create_task(new_task(&srv.url("/file.bin"), tmp.path()))
        .await?;
    store
        .update_probe(
            id,
            ProbeInfo {
                final_url: srv.url("/file.bin"),
                file_name: out.to_string(),
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
    store
        .save_chunks(
            id,
            vec![ChunkRow {
                chunk_index: 0,
                start: 0,
                end: size,
                committed,
            }],
        )
        .await?;
    store
        .update_state(id, TaskStateDb::Finalizing, None, None)
        .await?;
    let pool = Arc::new(DiskWriterPool::new(4, 16)?);
    Ok((tmp, srv, store, pool, part, id))
}

#[tokio::test]
async fn resume_finalizing_task_completes_rename_without_downloading() -> TestResult {
    let size = 1024u64;
    let (tmp, _srv, store, pool, part, id) = finalizing_fixture(size, "fin_ok.bin", size).await?;
    let (outcome, _rx) = resume(
        id,
        Arc::clone(&store),
        Arc::clone(&pool),
        &options(tmp.path(), "fin_ok.bin"),
        None,
    )
    .await?;
    assert_eq!(outcome, DownloadOutcome::Completed);
    assert!(!part.exists(), ".idm-part must have been renamed");
    assert_eq!(store.get_task(id).await?.state, TaskStateDb::Completed);
    assert_eq!(
        std::fs::metadata(tmp.path().join("fin_ok.bin"))?.len(),
        size
    );
    Ok(())
}

#[tokio::test]
async fn resume_finalizing_task_rejects_incomplete_committed() -> TestResult {
    let size = 1024u64;
    let (tmp, _srv, store, pool, part, id) =
        finalizing_fixture(size, "fin_bad.bin", size - 1).await?;
    let r = resume(
        id,
        Arc::clone(&store),
        Arc::clone(&pool),
        &options(tmp.path(), "fin_bad.bin"),
        None,
    )
    .await;
    match r {
        Err(Error::Resume { .. }) => {}
        Err(e) => return Err(format!("expected Resume error, got {e:?}").into()),
        Ok(_) => return Err("expected Resume error, resume succeeded".into()),
    }
    assert_eq!(store.get_task(id).await?.state, TaskStateDb::Finalizing);
    assert!(part.exists(), ".idm-part must be untouched");
    Ok(())
}
