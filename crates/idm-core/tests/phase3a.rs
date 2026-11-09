//! Phase 3a：动态/全局限速、按 Host 连接限制、接管零重叠、代理支持。
//! 全部使用 127.0.0.1 上的本地测试服务器 / 代理，不访问外网。

#![cfg(windows)]

mod support;

#[path = "support/conn_probe.rs"]
mod conn_probe;
#[path = "support/proxy.rs"]
mod proxy;
#[path = "support/verify.rs"]
mod verify;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use idm_core::disk::{AllocationStrategy, DiskWriterPool};
use idm_core::error::Error;
use idm_core::net::client::{DownloadConfig, ProxyConfig};
use idm_core::net::{HostConnectionLimiter, RateLimiter, Url};
use idm_core::task::{DownloadOptions, DownloadOutcome, ProgressSnapshot, download};
use proxy::{start_http_proxy, start_socks5_proxy};
use support::{ServerConfig, SpeedPolicy, start};
use tempfile::tempdir;
use tokio::sync::watch;
use verify::verify_file;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;

fn options(dir: &Path, out: &str, config: DownloadConfig) -> DownloadOptions {
    DownloadOptions {
        config,
        strategy: AllocationStrategy::Sparse,
        dir: dir.to_path_buf(),
        out_name: Some(out.to_string()),
        progress_tx: None,
        max_speed: None,
        global_limiter: None,
        host_limiter: None,
    }
}

fn default_config(connections: usize) -> DownloadConfig {
    DownloadConfig {
        connections,
        ..DownloadConfig::default()
    }
}

fn pool() -> TestResult<Arc<DiskWriterPool>> {
    Ok(Arc::new(DiskWriterPool::new(4, 16)?))
}

// ---------------------------------------------------------------------------
// 五.1 动态调速
// ---------------------------------------------------------------------------

/// 16 MiB / 初始 1 MiB/s；1.5 秒后改为不限速，必须立即唤醒等待中的 acquire，
/// 总耗时 < 3.5 秒（未生效则需 ~16 秒）。
#[tokio::test]
async fn dynamic_rate_increase_and_unlimited() -> TestResult {
    let size = 16 * MIB;
    let srv = start(ServerConfig {
        size,
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempdir()?;
    let rl = Arc::new(RateLimiter::new(Some(MIB)));
    let mut opts = options(tmp.path(), "dyn-up.bin", default_config(4));
    opts.global_limiter = Some(Arc::clone(&rl));
    let url = srv.url("/file.bin");
    let p = pool()?;

    let started = Instant::now();
    let handle = tokio::spawn(async move { download(&url, p, &opts, None).await });
    tokio::time::sleep(Duration::from_millis(1500)).await;
    rl.set_rate(None);
    assert_eq!(rl.current_rate(), None);
    let (outcome, _rx) = handle.await??;
    let elapsed = started.elapsed();

    assert_eq!(outcome, DownloadOutcome::Completed);
    assert!(
        elapsed < Duration::from_millis(3500),
        "改为不限速后应在 3.5 秒内完成，实际 {elapsed:?}"
    );
    verify_file(&tmp.path().join("dyn-up.bin"), size)?;
    println!("DYNAMIC_INCREASE elapsed={elapsed:?} size={size}");
    srv.shutdown();
    Ok(())
}

/// 初始 50 MiB/s 运行 500ms 后降到 1 MiB/s；随后 2 秒内客户端**落盘进度增长量**
/// 不得超过 2.4 MiB（旧桶令牌必须清零，4 个连接也不能群聚突发）。
///
/// Phase 3b A2：4 个连接、取消 300ms 稳定等待；同时记录服务器发送量作为对照。
#[tokio::test]
async fn dynamic_rate_decrease_clamps_burst() -> TestResult {
    let size = 64 * MIB;
    let sent = Arc::new(AtomicU64::new(0));
    let srv = start(ServerConfig {
        size,
        bytes_sent: Some(Arc::clone(&sent)),
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempdir()?;
    let rl = Arc::new(RateLimiter::new(Some(50 * MIB)));
    let (tx, mut rx) = watch::channel(ProgressSnapshot::default());
    let mut opts = options(tmp.path(), "dyn-down.bin", default_config(4));
    opts.progress_tx = Some(tx);
    opts.global_limiter = Some(Arc::clone(&rl));
    let url = srv.url("/file.bin");
    let p = pool()?;

    let started = Instant::now();
    let handle = tokio::spawn(async move { download(&url, p, &opts, None).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    rl.set_rate(Some(MIB));
    assert_eq!(rl.current_rate(), Some(MIB));
    assert_eq!(rl.capacity(), MIB);

    // 进度快照按 200ms 节流发布，直接读当前值可能还是 50 MiB/s 阶段的旧值。
    // 先把当前值标记为已读，再等变更后的**第一次新发布**作为基准：这是与进度
    // 通道同步，不是固定的稳定等待（通常 <= 200ms）。
    let _ = rx.borrow_and_update().downloaded;
    let _ = tokio::time::timeout(Duration::from_millis(500), rx.changed()).await;
    let before_progress = rx.borrow_and_update().downloaded;
    let before_sent = sent.load(Ordering::SeqCst);

    tokio::time::sleep(Duration::from_millis(2000)).await;
    let after_progress = rx.borrow_and_update().downloaded;
    let after_sent = sent.load(Ordering::SeqCst);
    let client_growth = after_progress.saturating_sub(before_progress);
    let server_growth = after_sent.saturating_sub(before_sent);
    // 2.4 MiB = 2 MiB (2 秒 × 1 MiB/s) + 0.4 MiB 余量。
    assert!(
        client_growth <= 2 * MIB + 409 * KIB,
        "降速后 2 秒内客户端落盘增长不得超过 2.4 MiB，实际 {client_growth} 字节"
    );

    // 恢复不限速让下载尽快结束，顺便校验文件完整性。
    rl.set_rate(None);
    let (outcome, _rx2) = handle.await??;
    let elapsed = started.elapsed();
    assert_eq!(outcome, DownloadOutcome::Completed);
    verify_file(&tmp.path().join("dyn-down.bin"), size)?;
    srv.shutdown();
    println!(
        "DYNAMIC_DECREASE client_2s_bytes={client_growth} server_2s_bytes={server_growth} total_elapsed={elapsed:?} size={size}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// 五.2 全局 + 单任务双层限速
// ---------------------------------------------------------------------------

/// 2 个下载任务（各 4 连接，各 6 MiB）共享一个 2 MiB/s 全局限速器：
/// 合计 12 MiB，总耗时应在 5.0~7.2 秒之间。
#[tokio::test]
async fn global_rate_limiter_shared_across_tasks() -> TestResult {
    let size = 6 * MIB;
    let srv = start(ServerConfig {
        size,
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempdir()?;
    let global = Arc::new(RateLimiter::new(Some(2 * MIB)));
    let mut a = options(tmp.path(), "global-a.bin", default_config(4));
    let mut b = options(tmp.path(), "global-b.bin", default_config(4));
    a.global_limiter = Some(Arc::clone(&global));
    b.global_limiter = Some(Arc::clone(&global));
    let url_a = srv.url("/a.bin");
    let url_b = srv.url("/b.bin");
    let pa = pool()?;
    let pb = pool()?;

    let started = Instant::now();
    let ha = tokio::spawn(async move { download(&url_a, pa, &a, None).await });
    let hb = tokio::spawn(async move { download(&url_b, pb, &b, None).await });
    let (ra, rb) = tokio::join!(ha, hb);
    let (outcome_a, _) = ra??;
    let (outcome_b, _) = rb??;
    let elapsed = started.elapsed();

    assert_eq!(outcome_a, DownloadOutcome::Completed);
    assert_eq!(outcome_b, DownloadOutcome::Completed);
    assert!(
        elapsed >= Duration::from_millis(5000) && elapsed <= Duration::from_millis(7200),
        "共享 2 MiB/s 全局限速时 12 MiB 应耗时 5.0~7.2 秒，实际 {elapsed:?}"
    );
    verify_file(&tmp.path().join("global-a.bin"), size)?;
    verify_file(&tmp.path().join("global-b.bin"), size)?;
    println!("GLOBAL_SHARED elapsed={elapsed:?} bytes={}", 2 * size);
    Ok(())
}

// ---------------------------------------------------------------------------
// 五.3 接管零重叠
// ---------------------------------------------------------------------------

/// 4 个连接，第 0 号分块按大小不一的块写出 100000 字节（不对齐）后停住 20 秒。
/// 服务器端在 accept 时 +1、连接关闭时 -1（不用 peek 事后剔除），断言：
/// 全程并发峰值严格 <= 4；被停住的旧连接在新分块请求到达前已被客户端关闭。
#[tokio::test]
async fn takeover_never_exceeds_connection_limit() -> TestResult {
    let size = 8 * MIB;
    let stall = Duration::from_secs(20);
    let probe = conn_probe::start(size, 100_000, stall).await?;
    let tmp = tempdir()?;
    let opts = options(tmp.path(), "no-overlap.bin", default_config(4));
    let url = probe.url.clone();
    let p = pool()?;

    let started = Instant::now();
    let (outcome, rx) = download(&url, p, &opts, None).await?;
    let elapsed = started.elapsed();
    let snap = rx.borrow().clone();
    let stats = probe.stats.clone();

    assert_eq!(outcome, DownloadOutcome::Completed);
    assert!(snap.takeover_count >= 1, "必须发生接管: {:?}", snap.chunks);
    let peak = stats.peak();
    assert!(peak <= 4, "接管期间活动连接峰值必须严格 <= 4，实际 {peak}");
    assert!(stats.closed(), "被停住的旧连接必须被客户端提前关闭");
    assert!(
        stats.new_request_after_close.load(Ordering::SeqCst),
        "新分块请求必须在旧连接关闭之后才到达"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "应在 5 秒内完成: {elapsed:?}"
    );
    verify_file(&tmp.path().join("no-overlap.bin"), size)?;
    println!(
        "TAKEOVER elapsed={elapsed:?} peak_active={peak} closed_after_ms={} takeovers={}",
        stats.close_after_ms.load(Ordering::SeqCst),
        snap.takeover_count
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// 五.4 / 五.5 按 Host 限制连接数
// ---------------------------------------------------------------------------

/// 3 个任务同时从同一台服务器下载，各 4 连接（共 12 个请求），共用一个
/// `max_per_host = 5` 的限速器。服务器端统计峰值必须严格等于 5。
#[tokio::test]
async fn host_limiter_caps_connections_across_tasks() -> TestResult {
    let size = 4 * MIB;
    let active = Arc::new(AtomicU64::new(0));
    let max_active = Arc::new(AtomicU64::new(0));
    let srv = start(ServerConfig {
        size,
        // 每条响应限速，保证 5 个名额会被同时占满且可被服务器观察到。
        speed: Some(SpeedPolicy(Arc::new(move |_, _| Some(512 * KIB)))),
        active: Some(Arc::clone(&active)),
        max_active: Some(Arc::clone(&max_active)),
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempdir()?;
    let limiter = Arc::new(HostConnectionLimiter::new(5)?);
    let mut handles = Vec::new();
    for i in 0..3 {
        let mut opts = options(tmp.path(), &format!("host-{i}.bin"), default_config(4));
        opts.host_limiter = Some(Arc::clone(&limiter));
        let url = srv.url(&format!("/host-{i}.bin"));
        let p = pool()?;
        handles.push(tokio::spawn(
            async move { download(&url, p, &opts, None).await },
        ));
    }
    for h in handles {
        let (outcome, _) = h.await??;
        assert_eq!(outcome, DownloadOutcome::Completed);
    }
    let peak = max_active.load(Ordering::SeqCst);
    assert_eq!(peak, 5, "跨任务按 Host 并发峰值必须严格等于 5，实际 {peak}");
    for i in 0..3 {
        verify_file(&tmp.path().join(format!("host-{i}.bin")), size)?;
    }
    println!("HOST_CAP peak_active={peak} tasks=3 connections_per_task=4");
    Ok(())
}

/// `max_per_host = 1`、退避基数 5 秒。第 0 号分块首次请求被断开后进入退避，
/// Host 名额必须在退避前释放：第 1 号分块应在 1 秒内完成。
#[tokio::test]
async fn host_permit_released_during_retry_backoff() -> TestResult {
    let size = 8 * MIB;
    let srv = start(ServerConfig {
        size,
        // 只让起点 0 的那个请求写出 64 KiB 后断开。
        break_at_start: Some((0, 64 * KIB)),
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempdir()?;
    let limiter = Arc::new(HostConnectionLimiter::new(1)?);
    let mut opts = options(
        tmp.path(),
        "backoff-host.bin",
        DownloadConfig {
            connections: 2,
            retry_base_delay: Duration::from_secs(5),
            retry_max_delay: Duration::from_secs(5),
            dynamic_split: false,
            ..DownloadConfig::default()
        },
    );
    opts.host_limiter = Some(limiter);
    let url = srv.url("/file.bin");
    let p = pool()?;

    let (outcome, rx) = download(&url, p, &opts, None).await?;
    assert_eq!(outcome, DownloadOutcome::Completed);
    let snap = rx.borrow().clone();
    let chunk1 = snap.chunks.get(1).ok_or("missing chunk 1")?;
    assert!(
        chunk1.elapsed_secs < 1.0,
        "第 1 号分块必须立即拿到 Host 名额并在 1 秒内完成，实际 {} 秒: {:?}",
        chunk1.elapsed_secs,
        snap.chunks
    );
    verify_file(&tmp.path().join("backoff-host.bin"), size)?;
    println!(
        "HOST_BACKOFF chunk1_elapsed={:.3}s retries={}",
        chunk1.elapsed_secs, chunk1.retries
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// 五.6 代理
// ---------------------------------------------------------------------------

async fn run_proxy_download(scheme: &str) -> TestResult<(u64, Duration)> {
    let proxy = if scheme == "socks5" {
        start_socks5_proxy().await?
    } else {
        start_http_proxy().await?
    };
    let srv = start(ServerConfig {
        size: 8 * MIB,
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempdir()?;
    // 用目标服务器的实际 host:port 构造经过代理的 URL；代理从绝对 URI /
    // SOCKS5 CONNECT 中解析出这个地址。
    let target = Url::parse(&srv.url("/file.bin"))?;
    let port = target.port_or_known_default().ok_or("no port")?;
    let url = format!("http://127.0.0.1:{port}/file.bin");
    let opts = options(
        tmp.path(),
        "proxied.bin",
        DownloadConfig {
            connections: 8,
            proxy: ProxyConfig::Custom(proxy.url()),
            ..DownloadConfig::default()
        },
    );
    let p = pool()?;
    let started = Instant::now();
    let (outcome, _rx) = download(&url, p, &opts, None).await?;
    let elapsed = started.elapsed();
    assert_eq!(outcome, DownloadOutcome::Completed);
    verify_file(&tmp.path().join("proxied.bin"), 8 * MIB)?;
    let count = proxy.count();
    // 1 个 Probe + 8 个分块 Range 请求，都必须经过代理转发。
    assert!(
        count >= 9,
        "所有 Probe 与分块请求都必须经过代理，实际转发 {count} 个"
    );
    println!("PROXY scheme={scheme} forwarded={count} elapsed={elapsed:?}");
    Ok((count, elapsed))
}

#[tokio::test]
async fn download_via_http_proxy() -> TestResult {
    let (count, _) = run_proxy_download("http").await?;
    assert!(count >= 9);
    Ok(())
}

#[tokio::test]
async fn download_via_socks5_proxy() -> TestResult {
    let (count, _) = run_proxy_download("socks5").await?;
    assert!(count >= 9);
    Ok(())
}

#[tokio::test]
async fn invalid_proxy_url_rejected() -> TestResult {
    for bad in ["ftp://127.0.0.1:1080", "socks4://127.0.0.1:1080", "乱码"] {
        let opts = options(
            std::path::Path::new("."),
            "invalid.bin",
            DownloadConfig {
                proxy: ProxyConfig::Custom(bad.to_string()),
                ..DownloadConfig::default()
            },
        );
        let p = pool()?;
        let res = download("http://127.0.0.1:1/file.bin", p, &opts, None).await;
        match res {
            Err(Error::InvalidProxy { url, .. }) => assert_eq!(url, bad),
            Err(e) => return Err(format!("expected InvalidProxy, got {e}").into()),
            Ok(_) => return Err(format!("expected InvalidProxy for {bad:?}").into()),
        }
    }
    println!("INVALID_PROXY 3/3 rejected with Error::InvalidProxy");
    Ok(())
}
