//! Phase 2：动态切分与工作窃取（全部使用本地测试服务器，不访问外网）。
//!
//! 单元测试（切分点计算、10.2 写入截断、10.3 第 4 步竞争）在
//! `src/task/split.rs` 的 `tests` 模块里；本文件覆盖第 4 ~ 10 项集成测试。

#![cfg(windows)]

mod support;

#[path = "support/speed.rs"]
mod speed;

#[path = "support/verify.rs"]
mod verify;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use idm_core::disk::{AllocationStrategy, DiskWriterPool};
use idm_core::net::client::DownloadConfig;
use idm_core::store::{NewTask, PauseReasonDb, Store, TaskStateDb};
use idm_core::task::{
    CancelSignal, DownloadContext, DownloadOptions, DownloadOutcome, ProgressSnapshot,
    download_with_store, resume,
};
use support::{ServerConfig, TestServer, start};
use tempfile::{TempDir, tempdir};
use verify::verify_file;

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

fn options(
    dir: &Path,
    out: &str,
    config: DownloadConfig,
    max_speed: Option<u64>,
) -> DownloadOptions {
    DownloadOptions {
        config,
        strategy: AllocationStrategy::Sparse,
        dir: dir.to_path_buf(),
        out_name: Some(out.to_string()),
        progress_tx: None,
        max_speed,
    }
}

/// 单次下载的观测结果。
struct Run {
    elapsed: Duration,
    snap: ProgressSnapshot,
    _tmp: TempDir,
    srv: TestServer,
}

impl Drop for Run {
    fn drop(&mut self) {
        self.srv.shutdown();
    }
}

/// 跑一次完整下载（含数据库），返回耗时与最终进度快照。
async fn run_download(
    cfg: ServerConfig,
    connections: usize,
    dynamic_split: bool,
    out: &str,
) -> TestResult<Run> {
    let size = cfg.size;
    let srv = start(cfg).await?;
    let tmp = tempdir()?;
    let db_path = tmp.path().join("idm.db");
    let store = Store::open(&db_path)?;
    let id = store
        .create_task(new_task(&srv.url("/file.bin"), tmp.path(), connections))
        .await?;
    let pool = Arc::new(DiskWriterPool::new(4, 16)?);
    let opts = options(
        tmp.path(),
        out,
        DownloadConfig {
            connections,
            dynamic_split,
            ..DownloadConfig::default()
        },
        None,
    );
    let started = Instant::now();
    let (outcome, rx) = download_with_store(
        &srv.url("/file.bin"),
        Arc::clone(&pool),
        &opts,
        None,
        Some(DownloadContext::new(Arc::clone(&store), id)),
    )
    .await?;
    let elapsed = started.elapsed();
    assert_eq!(outcome, DownloadOutcome::Completed);
    let snap = rx.borrow().clone();
    verify_file(&tmp.path().join(out), size)?;
    Ok(Run {
        elapsed,
        snap,
        _tmp: tmp,
        srv,
    })
}

/// 校验数据库中的分块记录满足 10.4 的第一条不变式：
/// 所有 `[start, end)` 互不重叠，并且拼起来正好覆盖 `[0, total)`。
///
/// 切分出来的分块编号是递增分配的，空间上却夹在被切分分块的中间，因此这里
/// 先按 `start` 排序，再校验相邻两段首尾相接（不变式只要求覆盖与不重叠，
/// 不要求编号顺序等于空间顺序）。
async fn assert_chunks_partition(store: &Store, id: i64, total: u64) -> TestResult {
    let rec = store.get_task(id).await?;
    let mut ranges: Vec<(i64, u64, u64, u64)> = rec
        .chunks
        .iter()
        .map(|c| (c.chunk_index, c.start, c.end, c.committed))
        .collect();
    ranges.sort_by_key(|r| r.1);
    let mut expected = 0u64;
    for (chunk_index, start, end, committed) in ranges {
        assert_eq!(
            start, expected,
            "分块 {chunk_index} 的 start 必须紧接上一段的 end"
        );
        assert!(end > start, "分块 {chunk_index} 的区间为空");
        assert!(committed <= end, "分块 {chunk_index} 的 committed 超过 end");
        expected = end;
    }
    assert_eq!(expected, total, "分块记录必须恰好覆盖 [0, total)");
    Ok(())
}

// ---------------------------------------------------------------------------
// 第 4 项：慢速第 0 号分块，对比 --no-split 的耗时
// ---------------------------------------------------------------------------

#[tokio::test]
async fn slow_chunk_split_beats_no_split() -> TestResult {
    let size = 8 * MIB;
    let cfg = || ServerConfig {
        size,
        // 第 0 号分块 2 MiB/s，其余不限速：不切分时总耗时由第 0 号分块决定。
        speed: Some(speed::only_slow_at(0, 2 * MIB)),
        ..ServerConfig::default()
    };

    let split = run_download(cfg(), 2, true, "split.bin").await?;
    assert!(
        split.snap.split_count >= 1,
        "开启切分时必须至少切分一次，实际 {} 次",
        split.snap.split_count
    );

    let no_split = run_download(cfg(), 2, false, "nosplit.bin").await?;
    assert_eq!(no_split.snap.split_count, 0, "--no-split 时不得切分");
    // 关闭切分时，第 0 号分块独占 4 MiB 的慢速区间。
    assert!(no_split.elapsed > Duration::from_millis(900));

    assert!(
        split.elapsed.as_secs_f64() < no_split.elapsed.as_secs_f64() * 0.85,
        "切分应显著缩短耗时：split={:?} no_split={:?}",
        split.elapsed,
        no_split.elapsed
    );

    // 分块汇总表：被切分方区间缩短，新分块来源为 split_from_0。
    let victim = split
        .snap
        .chunks
        .iter()
        .find(|c| c.db_index == 0)
        .ok_or("missing chunk 0")?;
    let ranges: Vec<(i64, u64, u64, Option<i64>)> = split
        .snap
        .chunks
        .iter()
        .map(|c| (c.db_index, c.start, c.end, c.parent))
        .collect();
    assert!(
        victim.end < 4 * MIB && victim.end >= MIB,
        "第 0 号分块区间应被缩小：victim.end={} ranges={:?}",
        victim.end,
        ranges
    );
    println!(
        "split={:?} ({} 次切分) no_split={:?}",
        split.elapsed, split.snap.split_count, no_split.elapsed
    );
    let stolen = split
        .snap
        .chunks
        .iter()
        .find(|c| c.parent == Some(0))
        .ok_or("missing split chunk")?;
    assert_eq!(stolen.start, victim.end, "新分块必须紧接被切分分块");
    Ok(())
}

// ---------------------------------------------------------------------------
// 第 5 项：8 连接 64 MiB × 50 个不同随机速度的种子
// ---------------------------------------------------------------------------

#[tokio::test]
async fn eight_connections_sixty_four_mib_fifty_seeds() -> TestResult {
    let size = 64 * MIB;
    for seed in 0..50u64 {
        let out = format!("seed-{seed}.bin");
        let run = run_download(
            ServerConfig {
                size,
                speed: Some(speed::seeded(seed, 32 * MIB)),
                ..ServerConfig::default()
            },
            8,
            true,
            &out,
        )
        .await?;
        assert!(
            run.snap.chunks.len() >= 8,
            "seed {seed}: 分块数 {} 少于连接数",
            run.snap.chunks.len()
        );
        // 每次运行都必须是完整、逐字节正确的文件（run_download 内已校验）。
        assert!(
            run.snap.downloaded == size,
            "seed {seed}: downloaded={} size={size}",
            run.snap.downloaded
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 第 6 项：切分与重试叠加
// ---------------------------------------------------------------------------

#[tokio::test]
async fn split_with_retries() -> TestResult {
    let size = 8 * MIB;
    let run = run_download(
        ServerConfig {
            size,
            speed: Some(speed::only_slow_at(0, 2 * MIB)),
            // 前两个响应各写 256 KiB 就断开：触发重试。
            break_after_bytes: Some(256 * KIB),
            break_responses: Some(2),
            ..ServerConfig::default()
        },
        2,
        true,
        "retry.bin",
    )
    .await?;

    let retries: u64 = run.snap.chunks.iter().map(|c| c.retries).sum();
    assert!(retries >= 1, "必须有分块发生重试，实际 {retries}");
    assert!(
        run.snap.split_count >= 1,
        "重试之后仍然必须能切分，实际 {} 次",
        run.snap.split_count
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// 第 7 项：切分过程中取消，再续传
// ---------------------------------------------------------------------------

struct Cancelled {
    _tmp: TempDir,
    srv: TestServer,
    store: Arc<Store>,
    pool: Arc<DiskWriterPool>,
    id: i64,
    size: u64,
    out: String,
}

impl Drop for Cancelled {
    fn drop(&mut self) {
        self.srv.shutdown();
    }
}

impl Cancelled {
    fn dir(&self) -> &Path {
        self._tmp.path()
    }
    fn opts(&self) -> DownloadOptions {
        options(
            self.dir(),
            &self.out,
            DownloadConfig {
                connections: 2,
                ..DownloadConfig::default()
            },
            None,
        )
    }
}

/// 慢速第 0 号分块 + 快速第 1 号分块：第 1 号完成后必然尝试切分，
/// 在这之后取消，使数据库里留下一个**尚未下完**的切分分块。
///
/// 慢速限在 `[0, 8 MiB)` 前缀内：切出来的分块起点也在这个前缀内，所以它在取消
/// 时一定还没下完（否则续传时会因为 `committed == end` 被跳过）。
async fn cancel_during_split(out: &str, cancel_after: Duration) -> TestResult<Cancelled> {
    let size = 16 * MIB;
    let srv = start(ServerConfig {
        size,
        speed: Some(speed::slow_in_prefix(8 * MIB, 4 * MIB)),
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempdir()?;
    let db_path = tmp.path().join("idm.db");
    let store = Store::open(&db_path)?;
    let id = store
        .create_task(new_task(&srv.url("/file.bin"), tmp.path(), 2))
        .await?;
    let pool = Arc::new(DiskWriterPool::new(4, 16)?);
    let mut ctx = DownloadContext::new(Arc::clone(&store), id);
    ctx.checkpoint_interval = Duration::from_millis(50);
    let opts = options(
        tmp.path(),
        out,
        DownloadConfig {
            connections: 2,
            ..DownloadConfig::default()
        },
        None,
    );
    let sig = Arc::new(CancelSignal::new());
    let s = Arc::clone(&sig);
    tokio::spawn(async move {
        tokio::time::sleep(cancel_after).await;
        s.cancel();
    });
    let (outcome, _rx) = download_with_store(
        &srv.url("/file.bin"),
        Arc::clone(&pool),
        &opts,
        Some(sig.as_ref()),
        Some(ctx),
    )
    .await?;
    assert_eq!(outcome, DownloadOutcome::Cancelled);
    Ok(Cancelled {
        _tmp: tmp,
        srv,
        store,
        pool,
        id,
        size,
        out: out.to_string(),
    })
}

#[tokio::test]
async fn cancel_during_split_then_resume() -> TestResult {
    let c = cancel_during_split("cancel.bin", Duration::from_millis(450)).await?;
    assert_chunks_partition(&c.store, c.id, c.size).await?;

    let rec = c.store.get_task(c.id).await?;
    assert_eq!(rec.state, TaskStateDb::Paused);
    assert_eq!(rec.pause_reason, Some(PauseReasonDb::User));
    let has_split = rec.chunks.iter().any(|ch| ch.parent.is_some());
    assert!(has_split, "取消前必须已经发生过切分");

    let (outcome, rx) = resume(
        c.id,
        Arc::clone(&c.store),
        Arc::clone(&c.pool),
        &c.opts(),
        None,
    )
    .await?;
    assert_eq!(outcome, DownloadOutcome::Completed);
    let rec = c.store.get_task(c.id).await?;
    assert_eq!(rec.state, TaskStateDb::Completed);
    assert_chunks_partition(&c.store, c.id, c.size).await?;
    verify_file(&c.dir().join(&c.out), c.size)?;
    // 续传时仍然允许切分。
    let snap = rx.borrow().clone();
    assert!(
        snap.split_count >= 1 || snap.chunks.len() >= 2,
        "续传应继续使用多个分块"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// 第 8 项：续传读取被切分分块的记录
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resume_reads_split_chunk_records() -> TestResult {
    let c = cancel_during_split("resume-split.bin", Duration::from_millis(500)).await?;
    let rec = c.store.get_task(c.id).await?;
    let split_chunk = rec
        .chunks
        .iter()
        .find(|ch| ch.parent.is_some())
        .ok_or("取消时数据库里应已有切分出来的分块")?
        .clone();
    // 切分出来的分块尚未下载完，续传必须继续它（而不是重新划分区间）。
    assert!(split_chunk.committed < split_chunk.end);

    let (outcome, rx) = resume(
        c.id,
        Arc::clone(&c.store),
        Arc::clone(&c.pool),
        &c.opts(),
        None,
    )
    .await?;
    assert_eq!(outcome, DownloadOutcome::Completed);
    verify_file(&c.dir().join(&c.out), c.size)?;

    // 续传的进度快照里，该分块以原来的数据库编号与来源出现。
    let snap = rx.borrow().clone();
    let seen = snap
        .chunks
        .iter()
        .find(|ch| ch.db_index == split_chunk.chunk_index)
        .ok_or("续传必须按数据库记录续传该分块")?;
    assert_eq!(seen.parent, split_chunk.parent, "来源必须来自数据库记录");
    assert_eq!(
        seen.start, split_chunk.committed,
        "续传起点必须是 committed"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// 第 9 项：切分之后限速仍然生效（≤ 120%）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rate_limit_holds_with_splits() -> TestResult {
    let size = 2 * MIB;
    let limit = MIB; // 1 MiB/s
    let srv = start(ServerConfig {
        size,
        // 服务器全速发送，限速完全由客户端负责。
        speed: Some(speed::constant(512 * MIB)),
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempdir()?;
    let store = Store::open(tmp.path().join("idm.db"))?;
    let id = store
        .create_task(new_task(&srv.url("/file.bin"), tmp.path(), 4))
        .await?;
    let pool = Arc::new(DiskWriterPool::new(4, 16)?);
    let opts = options(
        tmp.path(),
        "limited.bin",
        DownloadConfig {
            connections: 4,
            ..DownloadConfig::default()
        },
        Some(limit),
    );
    let started = Instant::now();
    let (outcome, _rx) = download_with_store(
        &srv.url("/file.bin"),
        Arc::clone(&pool),
        &opts,
        None,
        Some(DownloadContext::new(Arc::clone(&store), id)),
    )
    .await?;
    let elapsed = started.elapsed();
    srv.shutdown();
    assert_eq!(outcome, DownloadOutcome::Completed);
    verify_file(&tmp.path().join("limited.bin"), size)?;

    let avg = size as f64 / elapsed.as_secs_f64();
    assert!(
        avg <= 1.2 * limit as f64,
        "平均速度 {avg:.0} B/s 超过限速 120%（{} B/s）",
        1.2 * limit as f64
    );
    // 也不允许明显低于限速（说明限速把连接拖死了）。
    assert!(
        avg >= 0.8 * limit as f64,
        "平均速度 {avg:.0} B/s 远低于限速"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// 第 10 项：服务器端统计的最大并发请求数
// ---------------------------------------------------------------------------

#[tokio::test]
async fn server_side_concurrency_never_exceeds_connections() -> TestResult {
    let active = Arc::new(AtomicU64::new(0));
    let max_active = Arc::new(AtomicU64::new(0));
    let size = 16 * MIB;
    let srv = start(ServerConfig {
        size,
        // 每个请求 8 MiB/s 上下，保证多个请求真正重叠。
        speed: Some(speed::seeded(7, 8 * MIB)),
        active: Some(Arc::clone(&active)),
        max_active: Some(Arc::clone(&max_active)),
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempdir()?;
    let store = Store::open(tmp.path().join("idm.db"))?;
    let connections = 4usize;
    let id = store
        .create_task(new_task(&srv.url("/file.bin"), tmp.path(), connections))
        .await?;
    let pool = Arc::new(DiskWriterPool::new(4, 16)?);
    let opts = options(
        tmp.path(),
        "concurrent.bin",
        DownloadConfig {
            connections,
            ..DownloadConfig::default()
        },
        None,
    );
    let (outcome, rx) = download_with_store(
        &srv.url("/file.bin"),
        Arc::clone(&pool),
        &opts,
        None,
        Some(DownloadContext::new(Arc::clone(&store), id)),
    )
    .await?;
    srv.shutdown();
    assert_eq!(outcome, DownloadOutcome::Completed);
    verify_file(&tmp.path().join("concurrent.bin"), size)?;

    let peak = max_active.load(Ordering::SeqCst);
    assert!(peak >= 2, "应当有并行请求，实际峰值 {peak}");
    assert!(
        peak <= connections as u64,
        "服务器端并发请求峰值 {peak} 超过配置的连接数 {connections}"
    );
    // 客户端的下载已经结束，但服务器端处理任务的收尾（ActiveGuard 的 Drop）
    // 排在下一个调度点，因此这里短暂轮询等待计数归零。
    for _ in 0..100 {
        if active.load(Ordering::SeqCst) == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        active.load(Ordering::SeqCst),
        0,
        "所有响应结束后并发计数必须归零"
    );
    let snap = rx.borrow().clone();
    assert!(
        snap.split_count + snap.chunks.len() as u64 >= connections as u64,
        "切分不得减少有效连接数"
    );
    Ok(())
}
