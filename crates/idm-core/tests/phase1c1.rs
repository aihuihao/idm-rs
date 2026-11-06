//! Phase 1c.1：退出路径收尾、最终检查点门禁、多进程所有权与恢复清理。
//! 全部使用本地测试服务器，不访问外网。

#![cfg(windows)]

mod support;

#[path = "support/verify.rs"]
mod verify;

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use idm_core::disk::{AllocationStrategy, DiskWriterPool};
use idm_core::error::{Error, Result as IdmResult};
use idm_core::net::client::DownloadConfig;
use idm_core::store::{ChunkRow, NewTask, PauseReasonDb, ProbeInfo, Store, TaskStateDb};
use idm_core::task::{
    CancelSignal, DownloadContext, DownloadOptions, DownloadOutcome, PauseReason, ProgressSnapshot,
    download_with_store, resume,
};
use support::{ServerConfig, TestServer, start};
use tempfile::{TempDir, tempdir};
use tokio::sync::watch;
use verify::verify_file;

const MIB: u64 = 1024 * 1024;
const KIB: u64 = 1024;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;
type DownloadReply = IdmResult<(DownloadOutcome, watch::Receiver<ProgressSnapshot>)>;

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

/// 目录中除 SQLite 文件以外的内容（用于确认"磁盘上没有文件"）。
fn unexpected_files(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| !n.starts_with("idm.db"))
                .collect()
        })
        .unwrap_or_default()
}

struct Fixture {
    _tmp: TempDir,
    srv: TestServer,
    store: Arc<Store>,
    pool: Arc<DiskWriterPool>,
    id: i64,
    url: String,
}

impl Fixture {
    async fn new(size: u64, out: &str) -> TestResult<Fixture> {
        let srv = start(ServerConfig {
            size,
            ..ServerConfig::default()
        })
        .await?;
        let tmp = tempdir()?;
        let store = Store::open(tmp.path().join("idm.db"))?;
        let url = srv.url("/file.bin");
        let id = store.create_task(new_task(&url, tmp.path())).await?;
        let pool = Arc::new(DiskWriterPool::new(4, 16)?);
        let _ = out;
        Ok(Fixture {
            _tmp: tmp,
            srv,
            store,
            pool,
            id,
            url,
        })
    }

    fn dir(&self) -> &Path {
        self._tmp.path()
    }

    fn opts(&self, out: &str) -> DownloadOptions {
        options(self.dir(), out)
    }
}

// ---------------------------------------------------------------------------
// 第一部分：五条退出路径，每条一个测试
// ---------------------------------------------------------------------------

/// 路径 1：Probe 之前取消 → Paused(User)，part_path 为空，磁盘无文件。
#[tokio::test]
async fn exit_path_1_cancel_before_probe() -> TestResult {
    let f = Fixture::new(2 * MIB, "p1.bin").await?;
    let ctx = DownloadContext::new(Arc::clone(&f.store), f.id);
    let sig = CancelSignal::new();
    sig.cancel();
    let (outcome, _rx) = download_with_store(
        &f.url,
        Arc::clone(&f.pool),
        &f.opts("p1.bin"),
        Some(&sig),
        Some(ctx),
    )
    .await?;
    assert_eq!(outcome, DownloadOutcome::Cancelled);
    let rec = f.store.get_task(f.id).await?;
    assert_eq!(rec.state, TaskStateDb::Paused);
    assert_eq!(rec.pause_reason, Some(PauseReasonDb::User));
    assert!(rec.part_path.is_none(), "part_path must remain empty");
    assert!(
        unexpected_files(f.dir()).is_empty(),
        "no file may exist: {:?}",
        unexpected_files(f.dir())
    );
    f.srv.shutdown();
    Ok(())
}

/// 路径 2：Probe 之后、建文件之前取消 → Paused(User)，part_path 为空，磁盘无文件。
/// 同时确认"part_path 为空的 Paused 任务执行 resume 会重新走完整流程"。
#[tokio::test]
async fn exit_path_2_cancel_after_probe_and_resume_from_scratch() -> TestResult {
    let f = Fixture::new(2 * MIB, "p2.bin").await?;
    let size = 2 * MIB;
    let sig = Arc::new(CancelSignal::new());
    let s = Arc::clone(&sig);
    let mut ctx = DownloadContext::new(Arc::clone(&f.store), f.id);
    // after_probe 钩子在 Probe 成功之后、第一处建文件前取消检查之前触发，
    // 精确命中路径 2 而不是路径 1。
    ctx.hooks.after_probe = Some(Arc::new(move || s.cancel()));
    let (outcome, _rx) = download_with_store(
        &f.url,
        Arc::clone(&f.pool),
        &f.opts("p2.bin"),
        Some(sig.as_ref()),
        Some(ctx),
    )
    .await?;
    assert_eq!(outcome, DownloadOutcome::Cancelled);
    let rec = f.store.get_task(f.id).await?;
    assert_eq!(rec.state, TaskStateDb::Paused);
    assert_eq!(rec.pause_reason, Some(PauseReasonDb::User));
    assert!(rec.part_path.is_none(), "part_path must remain empty");
    assert!(
        unexpected_files(f.dir()).is_empty(),
        "no file may exist: {:?}",
        unexpected_files(f.dir())
    );

    // part_path 为空的 Paused 任务 → 重新 Probe → 创建文件 → 完整下载。
    let (o2, _rx2) = resume(
        f.id,
        Arc::clone(&f.store),
        Arc::clone(&f.pool),
        &f.opts("p2.bin"),
        None,
    )
    .await?;
    assert_eq!(o2, DownloadOutcome::Completed);
    assert_eq!(f.store.get_task(f.id).await?.state, TaskStateDb::Completed);
    verify_file(&f.dir().join("p2.bin"), size)?;
    f.srv.shutdown();
    Ok(())
}

/// 路径 3：建文件时磁盘满 → Paused(DiskFull)，part_path 为空，磁盘无文件。
#[tokio::test]
async fn exit_path_3_disk_full_before_create() -> TestResult {
    let srv = start(ServerConfig {
        size: u64::MAX / 2,
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempdir()?;
    let store = Store::open(tmp.path().join("idm.db"))?;
    let url = srv.url("/file.bin");
    let id = store.create_task(new_task(&url, tmp.path())).await?;
    let ctx = DownloadContext::new(Arc::clone(&store), id);
    let pool = Arc::new(DiskWriterPool::new(4, 16)?);
    let (outcome, _rx) = download_with_store(
        &url,
        Arc::clone(&pool),
        &options(tmp.path(), "p3.bin"),
        None,
        Some(ctx),
    )
    .await?;
    assert_eq!(outcome, DownloadOutcome::Paused(PauseReason::DiskFull));
    let rec = store.get_task(id).await?;
    assert_eq!(rec.state, TaskStateDb::Paused);
    assert_eq!(rec.pause_reason, Some(PauseReasonDb::DiskFull));
    assert!(rec.part_path.is_none(), "part_path must remain empty");
    assert!(
        unexpected_files(tmp.path()).is_empty(),
        "no file may exist: {:?}",
        unexpected_files(tmp.path())
    );
    srv.shutdown();
    Ok(())
}

/// 路径 4：建文件出错（目标目录不存在）→ Failed 并记录错误，磁盘无文件。
#[tokio::test]
async fn exit_path_4_create_error_marks_failed() -> TestResult {
    let f = Fixture::new(2 * MIB, "p4.bin").await?;
    let bad_dir = f.dir().join("missing-dir");
    let ctx = DownloadContext::new(Arc::clone(&f.store), f.id);
    let r = download_with_store(
        &f.url,
        Arc::clone(&f.pool),
        &options(&bad_dir, "p4.bin"),
        None,
        Some(ctx),
    )
    .await;
    assert!(r.is_err(), "create error must surface");
    let rec = f.store.get_task(f.id).await?;
    assert_eq!(rec.state, TaskStateDb::Failed);
    assert!(rec.last_error.is_some(), "error text must be recorded");
    assert!(rec.part_path.is_none(), "part_path must remain empty");
    assert!(!bad_dir.join("p4.bin.idm-part").exists());
    f.srv.shutdown();
    Ok(())
}

/// 路径 5：分块启动循环中取消 → 最终检查点 → Paused(User)，保留 .idm-part。
/// 并在返回后等待 3 个检查点间隔，确认检查点任务已经停止（updated_at 不再变化）。
#[tokio::test]
async fn exit_path_5_aborted_before_all_spawned_stops_checkpoint_task() -> TestResult {
    let interval = Duration::from_millis(400);
    let f = Fixture::new(8 * MIB, "p5.bin").await?;
    let sig = Arc::new(CancelSignal::new());
    let s = Arc::clone(&sig);
    let mut ctx = DownloadContext::new(Arc::clone(&f.store), f.id);
    ctx.checkpoint_interval = interval;
    ctx.hooks.before_chunks = Some(Arc::new(move || s.cancel()));
    let (outcome, _rx) = download_with_store(
        &f.url,
        Arc::clone(&f.pool),
        &f.opts("p5.bin"),
        Some(sig.as_ref()),
        Some(ctx),
    )
    .await?;
    assert_eq!(outcome, DownloadOutcome::Cancelled);
    let rec = f.store.get_task(f.id).await?;
    assert_eq!(rec.state, TaskStateDb::Paused);
    assert_eq!(rec.pause_reason, Some(PauseReasonDb::User));
    assert!(rec.part_path.is_some(), "part_path must be recorded");
    assert!(f.dir().join("p5.bin.idm-part").exists());

    let before = rec.updated_at.clone();
    tokio::time::sleep(interval * 3).await;
    let after = f.store.get_task(f.id).await?.updated_at;
    assert_eq!(
        before, after,
        "checkpoint task must have stopped after download returned"
    );
    f.srv.shutdown();
    Ok(())
}

/// "下载进行中取消"：同样验证检查点任务在返回后不再更新 updated_at。
#[tokio::test]
async fn mid_download_cancel_stops_checkpoint_task() -> TestResult {
    let interval = Duration::from_millis(400);
    let srv = start(ServerConfig {
        size: 16 * MIB,
        chunk_delay: Some(Duration::from_millis(40)),
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempdir()?;
    let store = Store::open(tmp.path().join("idm.db"))?;
    let url = srv.url("/file.bin");
    let id = store.create_task(new_task(&url, tmp.path())).await?;
    let mut ctx = DownloadContext::new(Arc::clone(&store), id);
    ctx.checkpoint_interval = interval;
    let pool = Arc::new(DiskWriterPool::new(4, 16)?);
    let sig = Arc::new(CancelSignal::new());
    let s = Arc::clone(&sig);
    let cancel_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(700)).await;
        s.cancel();
    });
    let (outcome, _rx) = download_with_store(
        &url,
        Arc::clone(&pool),
        &options(tmp.path(), "mid.bin"),
        Some(sig.as_ref()),
        Some(ctx),
    )
    .await?;
    assert_eq!(outcome, DownloadOutcome::Cancelled);
    cancel_task.abort();
    let rec = store.get_task(id).await?;
    assert_eq!(rec.state, TaskStateDb::Paused);
    assert!(tmp.path().join("mid.bin.idm-part").exists());
    let before = rec.updated_at.clone();
    tokio::time::sleep(interval * 3).await;
    let after = store.get_task(id).await?.updated_at;
    assert_eq!(
        before, after,
        "checkpoint task must have stopped after mid-download cancel"
    );
    srv.shutdown();
    Ok(())
}

// ---------------------------------------------------------------------------
// 第二部分：最后一次检查点失败时不得完成，清除钩子后 resume 可完成
// ---------------------------------------------------------------------------

#[tokio::test]
async fn final_checkpoint_failure_then_resume_completes() -> TestResult {
    let size = 2 * MIB;
    let srv = start(ServerConfig {
        size,
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempdir()?;
    let store = Store::open(tmp.path().join("idm.db"))?;
    let url = srv.url("/file.bin");
    let id = store.create_task(new_task(&url, tmp.path())).await?;
    let mut ctx = DownloadContext::new(Arc::clone(&store), id);
    ctx.checkpoint_interval = Duration::from_millis(50);
    let fail = Arc::new(AtomicBool::new(true));
    ctx.hooks.fail_sync = Arc::clone(&fail);
    let pool = Arc::new(DiskWriterPool::new(4, 16)?);
    let opts = options(tmp.path(), "gate.bin");

    // 所有检查点都失败：数据可以全部下载完，但绝不能进入 Finalizing / 改名。
    let r = download_with_store(&url, Arc::clone(&pool), &opts, None, Some(ctx)).await;
    assert!(
        r.is_err(),
        "final checkpoint failure must prevent completion"
    );
    assert_eq!(store.get_task(id).await?.state, TaskStateDb::Failed);
    assert!(
        !tmp.path().join("gate.bin").exists(),
        ".idm-part must not be renamed"
    );
    assert!(tmp.path().join("gate.bin.idm-part").exists());
    let rec = store.get_task(id).await?;
    assert!(!rec.chunks.is_empty());
    for c in &rec.chunks {
        assert_eq!(c.committed, c.start, "committed must still equal start");
    }

    // 清除钩子；resume 使用全新的默认 CheckpointHooks。
    fail.store(false, Ordering::SeqCst);
    let (outcome, _rx) = resume(id, Arc::clone(&store), Arc::clone(&pool), &opts, None).await?;
    assert_eq!(outcome, DownloadOutcome::Completed);
    assert_eq!(store.get_task(id).await?.state, TaskStateDb::Completed);
    verify_file(&tmp.path().join("gate.bin"), size)?;
    srv.shutdown();
    Ok(())
}

// ---------------------------------------------------------------------------
// 第三部分：多进程所有权（FILE_SHARE_READ）
// ---------------------------------------------------------------------------

struct Running {
    _tmp: TempDir,
    srv: TestServer,
    store: Arc<Store>,
    db_path: PathBuf,
    part: PathBuf,
    id: i64,
    sig: Arc<CancelSignal>,
    handle: tokio::task::JoinHandle<DownloadReply>,
}

impl Running {
    async fn start() -> TestResult<Running> {
        let srv = start(ServerConfig {
            size: 16 * MIB,
            chunk_delay: Some(Duration::from_millis(40)),
            ..ServerConfig::default()
        })
        .await?;
        let tmp = tempdir()?;
        let db_path = tmp.path().join("idm.db");
        let store = Store::open(&db_path)?;
        let url = srv.url("/file.bin");
        let id = store.create_task(new_task(&url, tmp.path())).await?;
        let mut ctx = DownloadContext::new(Arc::clone(&store), id);
        // 检查点间隔拉长，避免它改变我们要观察的 Downloading 状态。
        ctx.checkpoint_interval = Duration::from_secs(30);
        let part = tmp.path().join("owner.bin.idm-part");
        let opts = options(tmp.path(), "owner.bin");
        let pool = Arc::new(DiskWriterPool::new(4, 16)?);
        let sig = Arc::new(CancelSignal::new());
        let sig_dl = Arc::clone(&sig);
        let handle = tokio::spawn(async move {
            download_with_store(&url, pool, &opts, Some(sig_dl.as_ref()), Some(ctx)).await
        });
        // 等 .idm-part 出现且任务元信息已回填。
        for _ in 0..250 {
            let r = store.get_task(id).await?;
            if part.exists() && r.part_path.is_some() && r.state == TaskStateDb::Downloading {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let r = store.get_task(id).await?;
        assert!(part.exists(), "download must have created the .idm-part");
        assert_eq!(r.state, TaskStateDb::Downloading);
        assert!(r.part_path.is_some());
        Ok(Running {
            _tmp: tmp,
            srv,
            store,
            db_path,
            part,
            id,
            sig,
            handle,
        })
    }

    async fn stop(self) -> TestResult {
        self.sig.cancel();
        let _ = self.handle.await?;
        self.srv.shutdown();
        Ok(())
    }
}

/// 另一个 Store 实例执行 recover：占用中的任务状态不变，skipped = 1。
#[tokio::test]
async fn recover_skips_task_owned_by_another_process() -> TestResult {
    let r = Running::start().await?;
    let other = Store::open(&r.db_path)?;
    let report = other.recover().await?;
    assert_eq!(
        report.skipped, 1,
        "occupied Downloading task must be skipped"
    );
    assert_eq!(report.interrupted, 0);
    assert_eq!(
        other.get_task(r.id).await?.state,
        TaskStateDb::Downloading,
        "state must stay unchanged while another process downloads"
    );
    r.stop().await
}

/// 对正在下载的任务执行 resume → TaskInUse，数据库状态不变。
#[tokio::test]
async fn resume_on_occupied_task_returns_task_in_use() -> TestResult {
    let r = Running::start().await?;
    let tmp_opts_dir = r.part.parent().unwrap_or(Path::new(".")).to_path_buf();
    let pool = Arc::new(DiskWriterPool::new(2, 8)?);
    let result = resume(
        r.id,
        Arc::clone(&r.store),
        pool,
        &options(&tmp_opts_dir, "owner.bin"),
        None,
    )
    .await;
    match result {
        Err(Error::TaskInUse { id }) => assert_eq!(id, r.id),
        Err(e) => return Err(format!("expected TaskInUse, got {e:?}").into()),
        Ok(_) => return Err("expected TaskInUse, resume succeeded".into()),
    }
    assert_eq!(
        r.store.get_task(r.id).await?.state,
        TaskStateDb::Downloading
    );
    r.stop().await
}

/// 下载过程中：只读打开 .idm-part 成功，读写打开失败（错误码 32）。
#[tokio::test]
async fn share_mode_allows_read_but_denies_write() -> TestResult {
    let r = Running::start().await?;
    let ro = OpenOptions::new().read(true).open(&r.part);
    assert!(ro.is_ok(), "FILE_SHARE_READ must allow a read-only handle");
    drop(ro);
    let rw = OpenOptions::new().read(true).write(true).open(&r.part);
    match rw {
        Ok(_) => return Err("read-write open must fail while download owns the file".into()),
        Err(e) => assert_eq!(
            e.raw_os_error(),
            Some(32),
            "expected ERROR_SHARING_VIOLATION, got {e:?}"
        ),
    }
    r.stop().await
}

// ---------------------------------------------------------------------------
// 第四部分：Finalizing 文件名冲突 / Pending-Probing 残留清理
// ---------------------------------------------------------------------------

#[tokio::test]
async fn finalizing_resume_uses_conflict_name() -> TestResult {
    let size = 64 * KIB;
    let tmp = tempdir()?;
    let store = Store::open(tmp.path().join("idm.db"))?;
    let part = tmp.path().join("fin.bin.idm-part");
    let taken = tmp.path().join("fin.bin");
    fs::write(&part, vec![7u8; size as usize])?;
    fs::write(&taken, b"already exists")?;
    let id = store
        .create_task(new_task("http://127.0.0.1/fin.bin", tmp.path()))
        .await?;
    store
        .update_probe(
            id,
            ProbeInfo {
                final_url: "http://127.0.0.1/fin.bin".to_string(),
                file_name: "fin.bin".to_string(),
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
                committed: size,
            }],
        )
        .await?;
    store
        .update_state(id, TaskStateDb::Finalizing, None, None)
        .await?;
    let pool = Arc::new(DiskWriterPool::new(2, 8)?);
    let (outcome, _rx) = resume(
        id,
        Arc::clone(&store),
        pool,
        &options(tmp.path(), "fin.bin"),
        None,
    )
    .await?;
    assert_eq!(outcome, DownloadOutcome::Completed);
    assert!(
        tmp.path().join("fin (1).bin").exists(),
        "must pick a free name"
    );
    assert_eq!(
        store.get_task(id).await?.file_name.as_deref(),
        Some("fin (1).bin"),
        "database must record the actual final name"
    );
    assert_eq!(fs::read(&taken)?, b"already exists", "never overwrite");
    Ok(())
}

#[tokio::test]
async fn recover_fails_stale_pending_and_probing_tasks() -> TestResult {
    let tmp = tempdir()?;
    let db_path = tmp.path().join("idm.db");
    let store = Store::open(&db_path)?;
    let stale = store
        .create_task(new_task("http://x/stale", tmp.path()))
        .await?;
    let fresh = store
        .create_task(new_task("http://x/fresh", tmp.path()))
        .await?;
    store
        .update_state(stale, TaskStateDb::Probing, None, None)
        .await?;
    {
        let conn = rusqlite::Connection::open(&db_path)?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
        let old = (now - 6 * 60 * 1000).to_string();
        conn.execute(
            "UPDATE tasks SET updated_at=?2 WHERE id=?1",
            rusqlite::params![stale, old],
        )?;
    }
    let report = store.recover().await?;
    assert_eq!(report.stale_before_download, 1);
    let a = store.get_task(stale).await?;
    assert_eq!(a.state, TaskStateDb::Failed);
    assert_eq!(
        a.last_error.as_deref(),
        Some("interrupted before download started")
    );
    assert_eq!(
        store.get_task(fresh).await?.state,
        TaskStateDb::Pending,
        "recently created task must not be touched"
    );
    Ok(())
}
