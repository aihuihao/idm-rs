//! Phase 1b B6 集成测试：Probe 识别、静态分块下载、顺序下载、异常、取消、
//! 进度、并发与压力测试。测试服务器见 `tests/support/`。
//!
//! 测试代码不使用 `unwrap`/`expect`（workspace lint 设为 warn，验收以 -D warnings
//! 运行 clippy），一律用 `?` 与 `assert!`。

#![cfg(windows)]

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use idm_core::disk::{AllocationStrategy, DiskWriterPool};
use idm_core::error::{Error, Result as IdmResult};
use idm_core::net::Url;
use idm_core::net::client::DownloadConfig;
use idm_core::net::filename::{resolve_conflict, sanitize_windows_filename};
use idm_core::net::headers::{parse_content_disposition, parse_content_range};
use idm_core::net::probe::Probe;
use idm_core::task::{CancelSignal, DownloadOptions, DownloadOutcome, ProgressSnapshot, download};
use support::{RangeMode, ServerConfig, byte_at, start, verify_file};
use tempfile::tempdir;
use tokio::sync::watch;
use tokio::task::JoinSet;

const MIB: u64 = 1024 * 1024;
const KIB: u64 = 1024;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

fn options(dir: &Path, connections: usize, min_chunk: u64, out: &str) -> DownloadOptions {
    DownloadOptions {
        config: DownloadConfig {
            connections,
            min_chunk_size: min_chunk,
            ..DownloadConfig::default()
        },
        strategy: AllocationStrategy::Sparse,
        dir: dir.to_path_buf(),
        out_name: Some(out.to_string()),
        progress_tx: None,
    }
}

fn part_path(dir: &Path, out: &str) -> PathBuf {
    dir.join(format!("{out}.idm-part"))
}

async fn shutdown_pool(pool: Arc<DiskWriterPool>) -> TestResult {
    let pool = Arc::try_unwrap(pool)
        .map_err(|_| -> Box<dyn std::error::Error> { "pool still referenced".into() })?;
    pool.shutdown().await?;
    Ok(())
}

/// 逐字节校验（返回 Send 友好错误，供 JoinSet 任务使用）。
fn verify_bytes(path: &Path, size: u64) -> IdmResult<()> {
    let data = std::fs::read(path).map_err(|e| Error::Io {
        operation: "read-back",
        path: path.to_path_buf(),
        offset: 0,
        source: e,
    })?;
    if data.len() as u64 != size {
        return Err(Error::UnexpectedEof {
            chunk_id: 0,
            expected: size,
            received: data.len() as u64,
        });
    }
    for (i, &b) in data.iter().enumerate() {
        assert_eq!(b, byte_at(i as u64), "byte mismatch at {i}");
    }
    Ok(())
}

async fn probe_once(
    url: &str,
    cfg: &DownloadConfig,
) -> TestResult<idm_core::net::probe::ProbeResult> {
    let client = cfg.build_client()?;
    let parsed = Url::parse(url)?;
    let r = Probe::new(&client, cfg).probe(&parsed).await?;
    Ok(r)
}

// ---------------------------------------------------------------------------
// B6-1：纯函数补充用例（headers.rs / filename.rs 内还有各自完整的单元测试）
// ---------------------------------------------------------------------------

#[test]
fn pure_functions_extra_cases() -> TestResult {
    let cr = parse_content_range("bytes 100-199/*")?;
    assert_eq!(cr.start, 100);
    assert_eq!(cr.end, 199);
    assert_eq!(cr.total, None);
    assert!(parse_content_range("bytes */1234").is_err());
    assert_eq!(sanitize_windows_filename("a\u{1}b|c"), "a_b_c");
    assert_eq!(sanitize_windows_filename("NUL.txt"), "_NUL.txt");
    let long = format!("{}.txt", "x".repeat(300));
    let out = sanitize_windows_filename(&long);
    assert!(out.len() <= 200);
    assert!(out.ends_with(".txt"), "extension must be preserved: {out}");
    assert_eq!(
        parse_content_disposition("attachment; filename*=UTF-8''%E4%B8%AD%E6%96%87.txt").as_deref(),
        Some("中文.txt")
    );
    Ok(())
}

#[test]
fn resolve_conflict_first_free_number() -> TestResult {
    let tmp = tempdir()?;
    let _ = std::fs::File::create(tmp.path().join("f.bin"))?;
    assert_eq!(
        resolve_conflict(tmp.path(), "f.bin"),
        tmp.path().join("f (1).bin")
    );
    let _ = std::fs::File::create(tmp.path().join("f (1).bin"))?;
    assert_eq!(
        resolve_conflict(tmp.path(), "f.bin"),
        tmp.path().join("f (2).bin")
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// B6-2：Probe 在每种服务器行为下的识别结果
// ---------------------------------------------------------------------------

#[tokio::test]
async fn probe_range_supported() -> TestResult {
    let srv = start(ServerConfig {
        size: 4 * MIB,
        ..ServerConfig::default()
    })
    .await?;
    let r = probe_once(&srv.url("/file.bin"), &DownloadConfig::default()).await?;
    assert_eq!(r.total_size, Some(4 * MIB));
    assert!(r.supports_range);
    assert_eq!(r.filename, "file.bin");
    assert_eq!(r.content_type.as_deref(), Some("application/octet-stream"));
    srv.shutdown();
    Ok(())
}

#[tokio::test]
async fn probe_no_range_despite_accept_ranges_header() -> TestResult {
    // 服务器刻意返回 Accept-Ranges: bytes，但状态是 200 → 不得据此判定支持 Range。
    let srv = start(ServerConfig {
        size: 64 * KIB,
        range: RangeMode::NoRange,
        ..ServerConfig::default()
    })
    .await?;
    let r = probe_once(&srv.url("/file.bin"), &DownloadConfig::default()).await?;
    assert!(!r.supports_range);
    assert_eq!(r.total_size, Some(64 * KIB));
    srv.shutdown();
    Ok(())
}

#[tokio::test]
async fn probe_unknown_length() -> TestResult {
    let srv = start(ServerConfig {
        size: 32 * KIB,
        range: RangeMode::UnknownLength,
        ..ServerConfig::default()
    })
    .await?;
    let r = probe_once(&srv.url("/file.bin"), &DownloadConfig::default()).await?;
    assert_eq!(r.total_size, None);
    assert!(!r.supports_range);
    srv.shutdown();
    Ok(())
}

#[tokio::test]
async fn probe_star_total() -> TestResult {
    let srv = start(ServerConfig {
        size: 32 * KIB,
        range: RangeMode::StarTotal,
        ..ServerConfig::default()
    })
    .await?;
    let r = probe_once(&srv.url("/file.bin"), &DownloadConfig::default()).await?;
    assert_eq!(r.total_size, None);
    assert!(
        r.supports_range,
        "206 with total=* still proves Range support"
    );
    srv.shutdown();
    Ok(())
}

#[tokio::test]
async fn probe_redirect_final_url() -> TestResult {
    let srv = start(ServerConfig {
        size: 16 * KIB,
        redirect: true,
        ..ServerConfig::default()
    })
    .await?;
    let r = probe_once(&srv.url("/start"), &DownloadConfig::default()).await?;
    assert!(r.final_url.as_str().ends_with("/target"));
    assert_eq!(r.total_size, Some(16 * KIB));
    srv.shutdown();
    Ok(())
}

#[tokio::test]
async fn probe_chinese_content_disposition() -> TestResult {
    let srv = start(ServerConfig {
        size: 8 * KIB,
        content_disposition: Some(
            "attachment; filename=fallback.bin; filename*=UTF-8''%E4%B8%AD%E6%96%87.txt"
                .to_string(),
        ),
        ..ServerConfig::default()
    })
    .await?;
    let r = probe_once(&srv.url("/x"), &DownloadConfig::default()).await?;
    assert_eq!(r.filename, "中文.txt");
    srv.shutdown();
    Ok(())
}

#[tokio::test]
async fn probe_weak_etag_flag() -> TestResult {
    let srv = start(ServerConfig {
        size: 8 * KIB,
        etag: Some("\"v1\"".to_string()),
        weak_etag: true,
        ..ServerConfig::default()
    })
    .await?;
    let r = probe_once(&srv.url("/x"), &DownloadConfig::default()).await?;
    match &r.etag {
        Some((tag, weak)) => {
            assert!(weak, "W/ prefix must be detected as weak ETag");
            assert_eq!(tag, "W/\"v1\"");
        }
        None => panic!("expected an ETag"),
    }
    srv.shutdown();
    Ok(())
}

#[tokio::test]
async fn probe_gzip_rejected() -> TestResult {
    let srv = start(ServerConfig {
        size: 8 * KIB,
        content_encoding: Some("gzip".to_string()),
        ..ServerConfig::default()
    })
    .await?;
    let cfg = DownloadConfig::default();
    let client = cfg.build_client()?;
    let url = Url::parse(&srv.url("/x"))?;
    let r = Probe::new(&client, &cfg).probe(&url).await;
    assert!(matches!(r, Err(Error::UnsupportedContentEncoding { .. })));
    srv.shutdown();
    Ok(())
}

// ---------------------------------------------------------------------------
// B6-3：下载模式
// ---------------------------------------------------------------------------

#[tokio::test]
async fn download_eight_chunks_byte_exact() -> TestResult {
    let size = 8 * 256 * KIB;
    let srv = start(ServerConfig {
        size,
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempdir()?;
    let pool = Arc::new(DiskWriterPool::new(8, 16)?);
    let mut opts = options(tmp.path(), 8, 256 * KIB, "eight.bin");
    let (tx, rx) = watch::channel(ProgressSnapshot::default());
    opts.progress_tx = Some(tx);
    let (outcome, _rx) = download(&srv.url("/file.bin"), pool.clone(), &opts, None).await?;
    assert_eq!(outcome, DownloadOutcome::Completed);
    assert_eq!(rx.borrow().chunks.len(), 8, "must use exactly 8 chunks");
    assert!(
        rx.borrow().resumable,
        "static chunked download must be resumable"
    );
    verify_file(&tmp.path().join("eight.bin"), size)?;
    shutdown_pool(pool).await?;
    srv.shutdown();
    Ok(())
}

#[tokio::test]
async fn download_smaller_than_min_chunk_uses_one_chunk() -> TestResult {
    let size = 100 * KIB;
    let srv = start(ServerConfig {
        size,
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempdir()?;
    let pool = Arc::new(DiskWriterPool::new(4, 8)?);
    let mut opts = options(tmp.path(), 8, MIB, "small.bin");
    let (tx, rx) = watch::channel(ProgressSnapshot::default());
    opts.progress_tx = Some(tx);
    let (outcome, _rx) = download(&srv.url("/file.bin"), pool.clone(), &opts, None).await?;
    assert_eq!(outcome, DownloadOutcome::Completed);
    assert_eq!(rx.borrow().chunks.len(), 1, "small file must use 1 chunk");
    verify_file(&tmp.path().join("small.bin"), size)?;
    shutdown_pool(pool).await?;
    srv.shutdown();
    Ok(())
}

#[tokio::test]
async fn download_no_range_single_connection() -> TestResult {
    let size = 300 * KIB;
    let srv = start(ServerConfig {
        size,
        range: RangeMode::NoRange,
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempdir()?;
    let pool = Arc::new(DiskWriterPool::new(4, 8)?);
    let opts = options(tmp.path(), 8, 64 * KIB, "norange.bin");
    let (outcome, _rx) = download(&srv.url("/file.bin"), pool.clone(), &opts, None).await?;
    assert_eq!(outcome, DownloadOutcome::Completed);
    verify_file(&tmp.path().join("norange.bin"), size)?;
    shutdown_pool(pool).await?;
    srv.shutdown();
    Ok(())
}

#[tokio::test]
async fn download_unknown_length_sequential() -> TestResult {
    let size = 200 * KIB;
    let srv = start(ServerConfig {
        size,
        range: RangeMode::UnknownLength,
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempdir()?;
    let pool = Arc::new(DiskWriterPool::new(2, 8)?);
    let opts = options(tmp.path(), 8, 64 * KIB, "unknown.bin");
    let (outcome, rx) = download(&srv.url("/file.bin"), pool.clone(), &opts, None).await?;
    assert_eq!(outcome, DownloadOutcome::Completed);
    assert!(
        !rx.borrow().resumable,
        "streaming download must be marked non-resumable"
    );
    verify_file(&tmp.path().join("unknown.bin"), size)?;
    shutdown_pool(pool).await?;
    srv.shutdown();
    Ok(())
}

#[tokio::test]
async fn download_star_total_sequential() -> TestResult {
    let size = 200 * KIB;
    let srv = start(ServerConfig {
        size,
        range: RangeMode::StarTotal,
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempdir()?;
    let pool = Arc::new(DiskWriterPool::new(2, 8)?);
    let opts = options(tmp.path(), 8, 64 * KIB, "star.bin");
    let (outcome, _rx) = download(&srv.url("/file.bin"), pool.clone(), &opts, None).await?;
    assert_eq!(outcome, DownloadOutcome::Completed);
    verify_file(&tmp.path().join("star.bin"), size)?;
    shutdown_pool(pool).await?;
    srv.shutdown();
    Ok(())
}

// ---------------------------------------------------------------------------
// B6-5：完成后重命名（同时证明文件句柄已全部释放）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rename_part_file_on_completion() -> TestResult {
    let size = 512 * KIB;
    let srv = start(ServerConfig {
        size,
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempdir()?;
    let pool = Arc::new(DiskWriterPool::new(4, 8)?);
    let opts = options(tmp.path(), 4, 128 * KIB, "renamed.bin");
    let (outcome, _rx) = download(&srv.url("/file.bin"), pool.clone(), &opts, None).await?;
    assert_eq!(outcome, DownloadOutcome::Completed);
    assert!(!part_path(tmp.path(), "renamed.bin").exists());
    assert!(tmp.path().join("renamed.bin").exists());
    verify_file(&tmp.path().join("renamed.bin"), size)?;
    shutdown_pool(pool).await?;
    srv.shutdown();
    Ok(())
}

// ---------------------------------------------------------------------------
// B6-4：五种异常
// ---------------------------------------------------------------------------

async fn expect_download_error(
    server_cfg: ServerConfig,
    out: &str,
    connections: usize,
) -> TestResult<Error> {
    let srv = start(server_cfg).await?;
    let tmp = tempdir()?;
    let pool = Arc::new(DiskWriterPool::new(4, 8)?);
    let opts = options(tmp.path(), connections, 64 * KIB, out);
    let url = srv.url("/file.bin");
    let r = tokio::time::timeout(
        Duration::from_secs(20),
        download(&url, pool.clone(), &opts, None),
    )
    .await?;
    let e = match r {
        Ok(_) => panic!("expected download failure"),
        Err(e) => e,
    };
    let part = part_path(tmp.path(), out);
    assert!(part.exists(), "{out}.idm-part must be retained on failure");
    // 程序不能 panic：能走到这里、并且线程池能正常 shutdown 即为证据。
    shutdown_pool(pool).await?;
    srv.shutdown();
    Ok(e)
}

#[tokio::test]
async fn error_range_returned_200() -> TestResult {
    let e = expect_download_error(
        ServerConfig {
            size: 256 * KIB,
            range: RangeMode::RangeReturn200,
            ..ServerConfig::default()
        },
        "e200.bin",
        4,
    )
    .await?;
    assert!(matches!(e, Error::RangeNotHonored), "got {e:?}");
    Ok(())
}

#[tokio::test]
async fn error_bad_content_range() -> TestResult {
    let e = expect_download_error(
        ServerConfig {
            size: 256 * KIB,
            range: RangeMode::BadContentRange,
            ..ServerConfig::default()
        },
        "ecr.bin",
        4,
    )
    .await?;
    assert!(matches!(e, Error::InvalidContentRange { .. }), "got {e:?}");
    Ok(())
}

#[tokio::test]
async fn error_connection_broken() -> TestResult {
    let e = expect_download_error(
        ServerConfig {
            size: 256 * KIB,
            break_at: Some(70 * KIB),
            ..ServerConfig::default()
        },
        "eeof.bin",
        4,
    )
    .await?;
    assert!(matches!(e, Error::UnexpectedEof { .. }), "got {e:?}");
    Ok(())
}

#[tokio::test]
async fn error_etag_changed() -> TestResult {
    let e = expect_download_error(
        ServerConfig {
            size: 256 * KIB,
            etag: Some("\"v1\"".to_string()),
            change_etag_after: Some(0),
            ..ServerConfig::default()
        },
        "etag.bin",
        4,
    )
    .await?;
    assert!(matches!(e, Error::RemoteChanged), "got {e:?}");
    Ok(())
}

#[tokio::test]
async fn error_weak_etag_changed_on_206() -> TestResult {
    // 弱 ETag：不发 If-Range，改为校验每个 206 响应的 ETag 与 Probe 时一致。
    let e = expect_download_error(
        ServerConfig {
            size: 256 * KIB,
            etag: Some("\"v1\"".to_string()),
            weak_etag: true,
            change_etag_after: Some(0),
            ..ServerConfig::default()
        },
        "wetag.bin",
        4,
    )
    .await?;
    assert!(matches!(e, Error::RemoteChanged), "got {e:?}");
    Ok(())
}

#[tokio::test]
async fn error_gzip() -> TestResult {
    let e = expect_download_error(
        ServerConfig {
            size: 128 * KIB,
            content_encoding: Some("gzip".to_string()),
            ..ServerConfig::default()
        },
        "gzip.bin",
        4,
    )
    .await?;
    assert!(
        matches!(e, Error::UnsupportedContentEncoding { .. }),
        "got {e:?}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// B6-6 + 用户要求：三种时机取消
// ---------------------------------------------------------------------------

async fn run_cancel_case(
    server_cfg: ServerConfig,
    out: &'static str,
    connections: usize,
    pre_cancel: bool,
    cancel_after: Option<Duration>,
) -> TestResult {
    let srv = start(server_cfg).await?;
    let tmp = tempdir()?;
    let pool = Arc::new(DiskWriterPool::new(4, 16)?);
    let opts = options(tmp.path(), connections, 64 * KIB, out);
    let url = srv.url("/file.bin");
    let sig = Arc::new(CancelSignal::new());
    if pre_cancel {
        sig.cancel();
    }
    let cancel_task = cancel_after.map(|d| {
        let s = sig.clone();
        tokio::spawn(async move {
            tokio::time::sleep(d).await;
            s.cancel();
        })
    });

    let r = tokio::time::timeout(
        Duration::from_secs(10),
        download(&url, pool.clone(), &opts, Some(sig.as_ref())),
    )
    .await?;
    let (outcome, _rx) = r?;
    assert_eq!(
        outcome,
        DownloadOutcome::Cancelled,
        "{out} must be Cancelled"
    );
    assert!(
        part_path(tmp.path(), out).exists(),
        "{out}.idm-part must remain"
    );
    if let Some(t) = cancel_task {
        t.await?;
    }
    shutdown_pool(pool).await?;
    srv.shutdown();
    Ok(())
}

/// 取消时机 1：Probe 完成之前（且发生在 download 开始等待之前）。
#[tokio::test]
async fn cancel_before_probe() -> TestResult {
    run_cancel_case(
        ServerConfig {
            size: 2 * MIB,
            probe_delay: Some(Duration::from_millis(300)),
            ..ServerConfig::default()
        },
        "cancel_probe.bin",
        8,
        true,
        None,
    )
    .await
}

/// 取消时机 2：Probe 进行中（watch 状态语义保证不会漏掉）。
#[tokio::test]
async fn cancel_during_probe() -> TestResult {
    run_cancel_case(
        ServerConfig {
            size: 2 * MIB,
            probe_delay: Some(Duration::from_millis(800)),
            ..ServerConfig::default()
        },
        "cancel_during_probe.bin",
        8,
        false,
        Some(Duration::from_millis(100)),
    )
    .await
}

/// 取消时机 3：Probe 已完成、分块任务尚未全部启动。
#[tokio::test]
async fn cancel_before_all_chunks_spawned() -> TestResult {
    run_cancel_case(
        ServerConfig {
            size: 4 * MIB,
            probe_delay: Some(Duration::from_millis(150)),
            chunk_delay: Some(Duration::from_millis(5)),
            ..ServerConfig::default()
        },
        "cancel_spawn.bin",
        32,
        false,
        Some(Duration::from_millis(160)),
    )
    .await
}

/// 取消时机 4：下载进行中（数据正在到达）。
#[tokio::test]
async fn cancel_during_download() -> TestResult {
    run_cancel_case(
        ServerConfig {
            size: 8 * MIB,
            chunk_delay: Some(Duration::from_millis(20)),
            ..ServerConfig::default()
        },
        "cancel_mid.bin",
        8,
        false,
        Some(Duration::from_millis(200)),
    )
    .await
}

// ---------------------------------------------------------------------------
// B6-7：进度两种情况
// ---------------------------------------------------------------------------

#[tokio::test]
async fn progress_final_snapshot_matches_total() -> TestResult {
    let size = MIB;
    let srv = start(ServerConfig {
        size,
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempdir()?;
    let pool = Arc::new(DiskWriterPool::new(4, 8)?);
    let mut opts = options(tmp.path(), 4, 256 * KIB, "progress.bin");
    let (tx, mut rx) = watch::channel(ProgressSnapshot::default());
    opts.progress_tx = Some(tx);
    // 模拟真实读者：整个下载过程中持续消费快照。
    let reader = tokio::spawn(async move {
        let mut updates = 0u32;
        loop {
            if rx.changed().await.is_err() {
                break;
            }
            let _ = rx.borrow().downloaded;
            updates += 1;
        }
        updates
    });
    let (outcome, final_rx) = download(&srv.url("/file.bin"), pool.clone(), &opts, None).await?;
    drop(opts);
    assert_eq!(outcome, DownloadOutcome::Completed);
    let snap = final_rx.borrow();
    assert_eq!(
        snap.downloaded, size,
        "final snapshot must equal total size"
    );
    assert_eq!(snap.total_size, Some(size));
    assert_eq!(snap.chunks.len(), 4);
    drop(snap);
    // 外部读者在下载过程中持续消费，不得阻塞下载。
    let updates = reader.await?;
    assert!(updates > 0, "progress reader should observe updates");
    shutdown_pool(pool).await?;
    srv.shutdown();
    Ok(())
}

#[tokio::test]
async fn progress_without_any_reader_completes() -> TestResult {
    let size = 512 * KIB;
    let srv = start(ServerConfig {
        size,
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempdir()?;
    let pool = Arc::new(DiskWriterPool::new(2, 8)?);
    let opts = options(tmp.path(), 4, 128 * KIB, "noreader.bin");
    // 不注入通道、不启动读者、不读取返回的接收端：下载必须照常完成。
    let (outcome, rx) = download(&srv.url("/file.bin"), pool.clone(), &opts, None).await?;
    assert_eq!(outcome, DownloadOutcome::Completed);
    assert_eq!(
        rx.borrow().downloaded,
        size,
        "final snapshot must equal total"
    );
    assert_eq!(rx.borrow().total_size, Some(size));
    verify_file(&tmp.path().join("noreader.bin"), size)?;
    shutdown_pool(pool).await?;
    srv.shutdown();
    Ok(())
}

// ---------------------------------------------------------------------------
// B6-8：20 个下载任务同时进行、共用一个线程池，每任务 8 分块
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn twenty_concurrent_downloads_one_pool() -> TestResult {
    let size = 8 * 64 * KIB;
    let srv = start(ServerConfig {
        size,
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempdir()?;
    let pool = Arc::new(DiskWriterPool::new(8, 32)?);
    let dir = tmp.path().to_path_buf();
    let url = srv.url("/file.bin");
    let mut tasks: JoinSet<(usize, IdmResult<DownloadOutcome>)> = JoinSet::new();
    for i in 0..20usize {
        let p = pool.clone();
        let d = dir.clone();
        let u = url.clone();
        tasks.spawn(async move {
            let out = format!("task{i:02}.bin");
            let opts = options(&d, 8, 64 * KIB, &out);
            let r = download(&u, p, &opts, None).await;
            match r {
                Ok((o, _rx)) => (i, Ok(o)),
                Err(e) => (i, Err(e)),
            }
        });
    }
    let mut done = 0usize;
    while let Some(joined) = tasks.join_next().await {
        let (i, r) = joined?;
        assert_eq!(r?, DownloadOutcome::Completed, "task {i}");
        verify_bytes(&dir.join(format!("task{i:02}.bin")), size)?;
        done += 1;
    }
    assert_eq!(done, 20);
    shutdown_pool(pool).await?;
    srv.shutdown();
    Ok(())
}

// ---------------------------------------------------------------------------
// B6-9：1GB / 16 连接压力测试（#[ignore]，用 --release -- --ignored 运行）
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "1GB stress; run via: cargo test --release -p idm-core --test phase1b -- --ignored --nocapture"]
async fn stress_1gb_16_connections() -> TestResult {
    let size: u64 = 1024 * MIB;
    let srv = start(ServerConfig {
        size,
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempdir()?;
    let pool = Arc::new(DiskWriterPool::new(8, 64)?);
    let opts = options(tmp.path(), 16, MIB, "stress.bin");
    let t0 = Instant::now();
    let (outcome, _rx) = download(&srv.url("/file.bin"), pool.clone(), &opts, None).await?;
    let download_elapsed = t0.elapsed();
    assert_eq!(outcome, DownloadOutcome::Completed);
    let t1 = Instant::now();
    verify_file(&tmp.path().join("stress.bin"), size)?;
    let verify_elapsed = t1.elapsed();
    println!(
        "STRESS_1GB size={size} connections=16 download_elapsed={:.2}s verify_elapsed={:.2}s total_elapsed={:.2}s",
        download_elapsed.as_secs_f64(),
        verify_elapsed.as_secs_f64(),
        t0.elapsed().as_secs_f64()
    );
    shutdown_pool(pool).await?;
    srv.shutdown();
    Ok(())
}
