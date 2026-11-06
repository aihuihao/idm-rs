//! Phase 1d：分块重试 / 退避 / Retry-After（全部使用本地测试服务器，不访问外网）。

#![cfg(windows)]

#[allow(dead_code)]
mod support;

use std::path::Path;
use std::sync::Arc;

use idm_core::disk::{AllocationStrategy, DiskWriterPool};
use idm_core::error::Error;
use idm_core::net::client::DownloadConfig;
use idm_core::task::{DownloadOptions, download};
use support::{RangeMode, ServerConfig, start};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;

fn options(dir: &Path, cfg: DownloadConfig, out: &str) -> DownloadOptions {
    DownloadOptions {
        config: cfg,
        strategy: AllocationStrategy::Sparse,
        dir: dir.to_path_buf(),
        out_name: Some(out.to_string()),
        progress_tx: None,
        max_speed: None,
    }
}

async fn shutdown_pool(pool: Arc<DiskWriterPool>) -> TestResult {
    let pool = Arc::try_unwrap(pool)
        .map_err(|_| -> Box<dyn std::error::Error> { "pool still referenced".into() })?;
    pool.shutdown().await?;
    Ok(())
}

/// 修复 1：`max_retries = 0` 时首次错误必须原样返回，不得包装成 `RetryExhausted`。
///
/// 服务器每次响应写到 64 KiB 就断开，必然产生可重试的 `UnexpectedEof`；由于禁用
/// 重试，该错误应作为原始错误直接冒泡，并保留其具体变体（可续传）。
#[tokio::test]
async fn zero_retries_returns_original_error() -> TestResult {
    let size = 2 * MIB;
    let srv = start(ServerConfig {
        size,
        break_after_bytes: Some(64 * KIB),
        ..Default::default()
    })
    .await?;
    let tmp = tempfile::tempdir()?;
    let pool = Arc::new(DiskWriterPool::new(2, 8)?);
    let opts = options(
        tmp.path(),
        DownloadConfig {
            connections: 1,
            max_retries: 0,
            ..DownloadConfig::default()
        },
        "zero.bin",
    );

    let err = download(&srv.url("/file.bin"), pool.clone(), &opts, None)
        .await
        .expect_err("download must fail since retries are disabled");
    assert!(
        !matches!(err, Error::RetryExhausted { .. }),
        "max_retries=0 must return the original error, not RetryExhausted: {err:?}"
    );
    assert!(
        matches!(err, Error::UnexpectedEof { .. }),
        "expected the original UnexpectedEof error, got {err:?}"
    );

    shutdown_pool(pool).await?;
    srv.shutdown();
    Ok(())
}

/// 修复 1（流式）：服务器不支持 Range（`NoRange` → 走单连接 streaming）时，首个
/// 分块请求直接 500。流式模式强制 `max_retries = 0`，因此必须返回原始 `Http`
/// 错误而不得包装成 `RetryExhausted`。
#[tokio::test]
async fn streaming_first_failure_returns_original_error() -> TestResult {
    let size = MIB;
    // NoRange：Probe 得到 200 → supports_range=false → streaming；
    // error_status=500：Probe 被豁免，但真正的下载请求立即 500。
    let srv = start(ServerConfig {
        size,
        range: RangeMode::NoRange,
        error_status: Some(500),
        ..Default::default()
    })
    .await?;
    let tmp = tempfile::tempdir()?;
    let pool = Arc::new(DiskWriterPool::new(2, 8)?);
    let opts = options(
        tmp.path(),
        DownloadConfig {
            connections: 1,
            // 即便调用方给了重试，流式也必须把它压成 0。
            max_retries: 5,
            ..DownloadConfig::default()
        },
        "stream.bin",
    );

    let err = download(&srv.url("/file.bin"), pool.clone(), &opts, None)
        .await
        .expect_err("streaming download must fail on the first error");
    assert!(
        !matches!(err, Error::RetryExhausted { .. }),
        "streaming mode must return the original error, not RetryExhausted: {err:?}"
    );
    assert!(
        matches!(err, Error::Http { .. }),
        "expected the original Http error from status 500, got {err:?}"
    );

    shutdown_pool(pool).await?;
    srv.shutdown();
    Ok(())
}
