//! Phase 2：动态切分与工作窃取（全部使用本地测试服务器，不访问外网）。
//!
//! 单元测试（切分点计算、10.2 写入截断、10.3 第 4 步竞争，以及 Phase 2.1 的
//! 速度窗口 / 被切分分块选择）在 `src/task/split.rs` 的 `tests` 模块里；
//! 本文件覆盖 Phase 2 第 4 ~ 10 项与 Phase 2.1 的集成测试。

#![cfg(windows)]

mod support;

#[path = "support/speed.rs"]
mod speed;

#[path = "support/verify.rs"]
mod verify;

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use idm_core::disk::{AllocationStrategy, DiskWriterPool};
use idm_core::net::client::DownloadConfig;
use idm_core::store::{ChunkRow, NewTask, PauseReasonDb, ProbeInfo, Store, TaskStateDb};
use idm_core::task::{
    CancelSignal, ChunkState, DownloadContext, DownloadOptions, DownloadOutcome, ProgressSnapshot,
    download_with_store, resume,
};
use support::{ServerConfig, StallConfig, TestServer, start};
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
    let config = DownloadConfig {
        connections,
        dynamic_split,
        ..DownloadConfig::default()
    };
    run_download_ex(cfg, connections, out, config).await
}

/// 与 [`run_download`] 相同，但允许注入自定义 `DownloadConfig`
/// （Phase 2.2 退避接管测试需要把退避基数设为 10 秒）。
async fn run_download_ex(
    cfg: ServerConfig,
    connections: usize,
    out: &str,
    config: DownloadConfig,
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
    let opts = options(tmp.path(), out, config, None);
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
///
/// Phase 2.2.1 起允许空区间 `[start, start)`：零进度接管把被接管分块的记录
/// 缩成空区间。排序时按 `(start, end)`，空区间排在同起点的非空区间之前。
async fn assert_chunks_partition(store: &Store, id: i64, total: u64) -> TestResult {
    let rec = store.get_task(id).await?;
    let mut ranges: Vec<(i64, u64, u64, u64)> = rec
        .chunks
        .iter()
        .map(|c| (c.chunk_index, c.start, c.end, c.committed))
        .collect();
    ranges.sort_by_key(|r| (r.1, r.2));
    let mut expected = 0u64;
    for (chunk_index, start, end, committed) in ranges {
        assert_eq!(
            start, expected,
            "分块 {chunk_index} 的 start 必须紧接上一段的 end"
        );
        assert!(end >= start, "分块 {chunk_index} 的区间倒置");
        assert!(
            committed >= start && committed <= end,
            "分块 {chunk_index} 的 committed 不在 [start, end] 内"
        );
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
            // [0, 4 MiB) 前缀内的请求都慢：第 0 号分块重试后（起点变成 256 KiB）
            // 仍然明显慢于第 1 号分块，保证"重试 + 切分"这个组合稳定复现，
            // 不再依赖两个分块谁先完成的竞态。
            speed: Some(speed::slow_in_prefix(4 * MIB, 2 * MIB)),
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
        "重试之后仍然必须能切分，实际 {} 次: {:?}",
        run.snap.split_count,
        run.snap.chunks
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

// ---------------------------------------------------------------------------
// Phase 2.1 第一部分：续传时编号最大的分块（切分产生）已经完成
// ---------------------------------------------------------------------------

/// 直接构造一个已暂停的任务：编号 2 是切分产物且已下完（committed == end），
/// 编号 0、1 未完成。续传时 `inits` 的最大编号只有 1，新分块若按 1+1=2 分配
/// 会与已完成的分块 2 主键冲突。修复后必须从**全部**记录的最大值 2 起算 3。
#[tokio::test]
async fn resume_when_largest_split_chunk_already_completed() -> TestResult {
    let size = 16 * MIB;
    let srv = start(ServerConfig {
        size,
        // 第 0 号分块慢、第 1 号分块全速：第 1 号先完成 → 空闲连接触发切分。
        speed: Some(speed::slow_in_prefix(4 * MIB, 2 * MIB)),
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempdir()?;
    let dir = tmp.path();
    let url = srv.url("/file.bin");
    let store = Store::open(dir.join("idm.db"))?;
    let id = store.create_task(new_task(&url, dir, 2)).await?;
    let part = dir.join("resume.bin.idm-part");
    store
        .update_probe(
            id,
            ProbeInfo {
                final_url: url.clone(),
                file_name: "resume.bin".to_string(),
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
            vec![
                ChunkRow {
                    chunk_index: 0,
                    start: 0,
                    end: 4 * MIB,
                    committed: MIB,
                    parent: None,
                },
                ChunkRow {
                    chunk_index: 1,
                    start: 8 * MIB,
                    end: 16 * MIB,
                    committed: 8 * MIB,
                    parent: None,
                },
                // 编号最大、由切分产生、且已经完成。
                ChunkRow {
                    chunk_index: 2,
                    start: 4 * MIB,
                    end: 8 * MIB,
                    committed: 8 * MIB,
                    parent: Some(0),
                },
            ],
        )
        .await?;
    store
        .update_state(id, TaskStateDb::Paused, Some(PauseReasonDb::User), None)
        .await?;

    // .idm-part：已提交区间 [0,1 MiB) 与 [4 MiB,8 MiB) 必须逐字节正确，
    // 其余区间由续传重新下载。
    let mut data = vec![0u8; size as usize];
    for (a, b) in [(0u64, MIB), (4 * MIB, 8 * MIB)] {
        for off in a..b {
            data[off as usize] = support::byte_at(off);
        }
    }
    std::fs::write(&part, &data)?;

    let pool = Arc::new(DiskWriterPool::new(4, 16)?);
    let opts = options(
        dir,
        "resume.bin",
        DownloadConfig {
            connections: 2,
            ..DownloadConfig::default()
        },
        None,
    );
    let (outcome, rx) = resume(id, Arc::clone(&store), Arc::clone(&pool), &opts, None).await?;
    srv.shutdown();
    assert_eq!(outcome, DownloadOutcome::Completed);

    let snap = rx.borrow().clone();
    assert!(
        snap.split_count >= 1,
        "续传后必须至少再切分一次，实际 {} 次",
        snap.split_count
    );
    let rec = store.get_task(id).await?;
    assert_eq!(rec.state, TaskStateDb::Completed);
    let indices: Vec<i64> = rec.chunks.iter().map(|c| c.chunk_index).collect();
    let mut unique = indices.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), indices.len(), "分块编号必须唯一: {indices:?}");
    assert!(
        indices.iter().any(|i| *i >= 3),
        "新分块编号必须接在全部已有编号（含已完成的 2）之后: {indices:?}"
    );
    assert_chunks_partition(&store, id, size).await?;
    verify_file(&dir.join("resume.bin"), size)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Phase 2.2 第三部分：卡住 / 极慢 / 退避期间被接管
// ---------------------------------------------------------------------------

/// 构造"第 0 号分块写出 `after_bytes` 后完全停住"的服务器。
/// `after_bytes == 0` 时连响应头都不发（零进度停顿）；`variable_blocks` 为真时
/// 按 1~16 KiB 大小不一的块发送。停住期间不发送任何数据，也不主动关闭连接。
fn stall_cfg(
    size: u64,
    duration: Duration,
    after_bytes: u64,
    variable_blocks: bool,
) -> (ServerConfig, Arc<AtomicBool>, Arc<AtomicU64>) {
    let closed_early = Arc::new(AtomicBool::new(false));
    let close_after_ms = Arc::new(AtomicU64::new(u64::MAX));
    let cfg = ServerConfig {
        size,
        stall: Some(StallConfig {
            after_bytes,
            duration,
            stall_used: Arc::new(AtomicBool::new(false)),
            variable_blocks,
            closed_early: Arc::clone(&closed_early),
            close_after_ms: Arc::clone(&close_after_ms),
        }),
        ..ServerConfig::default()
    };
    (cfg, closed_early, close_after_ms)
}

/// 等服务器把"客户端提前关闭连接"记录下来（收尾调度可能晚一个时间点）。
async fn wait_for_close_record(closed: &Arc<AtomicBool>) -> bool {
    for _ in 0..50 {
        if closed.load(Ordering::SeqCst) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    closed.load(Ordering::SeqCst)
}

/// 重新打开一次下载留下的数据库，校验 10.4 的分块不变式（允许空区间）。
async fn assert_run_partition(run: &Run, size: u64) -> TestResult<(Arc<Store>, i64)> {
    let store = Store::open(run._tmp.path().join("idm.db"))?;
    let tasks = store.list_tasks().await?;
    let id = tasks.first().ok_or("database has no task")?.id;
    assert_chunks_partition(&store, id, size).await?;
    Ok((store, id))
}

/// Phase 2.2.1 第四部分第 1 项：服务器按 1~16 KiB 大小不一的块先发 100000
/// 字节（不是 64 KiB 的整数倍），然后停住 20 秒。接管切分点必须**等于**
/// reserved（100000），原分块不留尾巴，5 秒内完成。
#[tokio::test]
async fn unaligned_stalled_first_chunk_is_taken_over() -> TestResult {
    let size = 8 * MIB;
    let stall = Duration::from_secs(20);
    const SENT: u64 = 100_000;

    let (cfg, closed, ms) = stall_cfg(size, stall, SENT, true);
    let run = run_download(cfg, 2, true, "unaligned.bin").await?;
    assert!(
        run.snap.takeover_count >= 1,
        "卡住的分块必须触发接管: {:?}",
        run.snap.chunks
    );
    assert!(
        run.elapsed < Duration::from_secs(5),
        "开启切分时必须在 5 秒内完成，实际 {:?}",
        run.elapsed
    );
    let victim = run
        .snap
        .chunks
        .iter()
        .find(|c| c.db_index == 0)
        .ok_or("missing chunk 0")?;
    assert_eq!(
        victim.end, SENT,
        "接管切分点必须等于已接收字节数，不能向上对齐到 64 KiB"
    );
    assert!(
        wait_for_close_record(&closed).await,
        "服务器必须观察到连接被提前关闭"
    );
    let closed_ms = ms.load(Ordering::SeqCst);
    assert!(
        closed_ms < stall.as_millis() as u64,
        "连接关闭时间 {closed_ms}ms 必须早于停住期满 {}ms",
        stall.as_millis()
    );
    assert_run_partition(&run, size).await?;
    println!(
        "UNALIGNED split={:?} takeovers={} split_point={} client_close_after={}ms stall={:?}",
        run.elapsed, run.snap.takeover_count, victim.end, closed_ms, stall
    );
    Ok(())
}

/// Phase 2.2.1 第四部分第 2 项：零进度停顿——服务器接受连接后 20 秒内不返回
/// 任何响应头，也不关闭连接。接管把原分块缩成空区间，唤醒卡在 `req.send()`
/// 的任务，延迟分配新分块的写入器；数据库允许存在空区间记录。
#[tokio::test]
async fn zero_progress_stalled_first_chunk_is_taken_over() -> TestResult {
    let size = 8 * MIB;
    let stall = Duration::from_secs(20);

    let (cfg, closed, ms) = stall_cfg(size, stall, 0, false);
    let run = run_download(cfg, 2, true, "zero-progress.bin").await?;
    assert!(
        run.snap.takeover_count >= 1,
        "零进度分块必须触发接管: {:?}",
        run.snap.chunks
    );
    assert!(
        run.elapsed < Duration::from_secs(5),
        "开启切分时必须在 5 秒内完成，实际 {:?}",
        run.elapsed
    );
    assert!(
        wait_for_close_record(&closed).await,
        "服务器必须观察到连接被提前关闭"
    );
    let closed_ms = ms.load(Ordering::SeqCst);
    assert!(
        closed_ms < stall.as_millis() as u64,
        "连接关闭时间 {closed_ms}ms 必须早于停住期满 {}ms",
        stall.as_millis()
    );

    let (store, id) = assert_run_partition(&run, size).await?;
    let rec = store.get_task(id).await?;
    assert!(
        rec.chunks.iter().any(|c| c.start == c.end),
        "零进度接管必须留下空区间记录: {:?}",
        rec.chunks
    );
    println!(
        "ZERO split={:?} takeovers={} chunks={} client_close_after={}ms stall={:?}",
        run.elapsed,
        run.snap.takeover_count,
        rec.chunks.len(),
        closed_ms,
        stall
    );
    Ok(())
}

/// 极慢分块：第 0 号分块 10 KiB/s（1 KiB 小块持续发送），其余分块分别
/// 128 KiB/s 与 96 KiB/s，切分出来的新分块全速。3 个 320 KiB 分块保证
/// 接管后仍有 ≥256 KiB 可交给新分块。
#[tokio::test]
async fn very_slow_first_chunk_is_taken_over() -> TestResult {
    let size = 960 * KIB; // 3 × 320 KiB
    let connections = 3;
    let cfg = || ServerConfig {
        size,
        // 4 KiB 块：极慢分块每块 ~0.4s（持续有数据，不会被判成"卡住"），
        // 其它分块每块 ~16/26ms，两者速度差足以触发 10.5 的"明显慢"。
        block: Some(4096),
        speed: Some(speed::slow_first_and_two_others(
            10 * KIB,
            320 * KIB,
            256 * KIB,
            640 * KIB,
            160 * KIB,
        )),
        ..ServerConfig::default()
    };
    let config = |dynamic_split: bool| DownloadConfig {
        connections,
        min_chunk_size: 320 * KIB,
        dynamic_split,
        ..DownloadConfig::default()
    };

    let split = run_download_ex(cfg(), connections, "slow-takeover.bin", config(true)).await?;
    assert!(
        split.snap.takeover_count >= 1,
        "10 KiB/s 的分块必须触发接管: {:?}",
        split.snap.chunks
    );
    let no_split = run_download_ex(cfg(), connections, "slow-nosplit.bin", config(false)).await?;
    assert_eq!(no_split.snap.split_count, 0, "--no-split 时不得切分");
    assert!(
        split.elapsed.as_secs_f64() * 3.0 < no_split.elapsed.as_secs_f64(),
        "接管应显著缩短耗时: split={:?} no_split={:?}",
        split.elapsed,
        no_split.elapsed
    );
    println!(
        "SLOW split={:?} takeovers={} no_split={:?}",
        split.elapsed, split.snap.takeover_count, no_split.elapsed
    );
    Ok(())
}

/// 解析 `bytes=a-b`；用于断言没有"起点等于终点"（空/倒置）的 Range 请求。
fn range_bounds(h: &str) -> Option<(u64, u64)> {
    let v = h.strip_prefix("bytes=")?;
    let (a, b) = v.split_once('-')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

/// 退避期间被接管：第 0 号分块写到 256 KiB 时断开，进入 10 秒退避；
/// 退避中被接管到 acked 并唤醒，以 Completed 结束，不再发任何请求。
#[tokio::test]
async fn backoff_wait_is_taken_over() -> TestResult {
    let size = 8 * MIB;
    let ranges: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let cfg = ServerConfig {
        size,
        break_at_start: Some((0, 256 * KIB)),
        chunk_range_sink: Some(Arc::clone(&ranges)),
        ..ServerConfig::default()
    };
    let config = DownloadConfig {
        connections: 2,
        retry_base_delay: Duration::from_secs(10),
        retry_max_delay: Duration::from_secs(10),
        ..DownloadConfig::default()
    };
    let run = run_download_ex(cfg, 2, "backoff-takeover.bin", config).await?;
    assert!(
        run.snap.takeover_count >= 1,
        "退避期间必须发生接管: {:?}",
        run.snap.chunks
    );
    let victim = run
        .snap
        .chunks
        .iter()
        .find(|c| c.db_index == 0)
        .ok_or("missing chunk 0")?;
    assert_eq!(victim.state, ChunkState::Done, "第 0 号分块必须以完成结束");

    let recorded = ranges.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert!(!recorded.is_empty(), "服务器必须记录到分块请求");
    for h in &recorded {
        let (a, b) = range_bounds(h).ok_or_else(|| format!("无法解析 Range 头: {h}"))?;
        assert!(b >= a, "不允许发出起点等于终点/倒置的 Range 请求: {h}");
        assert_ne!(h, "bytes=0-0", "分块请求不得使用旧的 bytes=0-0 兜底分支");
    }
    // 起点 256 KiB 的请求只应有一次：接管产生的新分块。若旧分块在退避结束后
    // 又重试了一次，这里会变成 2 次。
    let from_acked = recorded
        .iter()
        .filter(|h| range_bounds(h).is_some_and(|(a, _)| a == 256 * KIB))
        .count();
    assert_eq!(
        from_acked, 1,
        "退避被接管后不应再有旧分块的重试请求: {recorded:?}"
    );
    println!(
        "BACKOFF elapsed={:?} takeovers={} requests={:?}",
        run.elapsed, run.snap.takeover_count, recorded
    );
    Ok(())
}
