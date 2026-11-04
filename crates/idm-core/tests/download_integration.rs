//! 下载编排集成测试：用本地最小 HTTP/1.1 服务器驱动 `idm_core::task::download`，
//! 覆盖静态分块并发、单连接流式、以及取消三种路径。
//!
//! 测试刻意不使用 `unwrap`/`expect`（workspace lint 已将其设为 `warn`，
//! 验收脚本以 `-D warnings` 运行 clippy），错误一律用 `?` 传播。

#![cfg(windows)]

use std::error::Error;
use std::sync::Arc;

use idm_core::disk::{AllocationStrategy, DiskWriterPool};
use idm_core::error::{Error as IdmError, Result as IdmResult};
use idm_core::net::client::DownloadConfig;
use idm_core::task::{DownloadOptions, DownloadOutcome, download};
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

/// 服务器行为模式。
#[derive(Clone, Copy)]
enum Mode {
    /// 支持 Range：对带 Range 的请求返回 206。
    Range,
    /// 不支持 Range：总是返回 200 全量（Content-Length 已知）。
    NoRange,
    /// 支持 Range，但发送响应体前先睡眠 `ms`，供取消测试使用。
    Slow(u64),
}

/// 启动一个监听 `127.0.0.1:0` 的最小 HTTP 服务器，返回基址与可中止的句柄。
async fn serve(body: Arc<Vec<u8>>, mode: Mode) -> IdmResult<(String, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| IdmError::Io {
            operation: "bind test server",
            path: std::path::PathBuf::new(),
            offset: 0,
            source: e,
        })?;
    let addr = listener.local_addr().map_err(|e| IdmError::Io {
        operation: "local_addr",
        path: std::path::PathBuf::new(),
        offset: 0,
        source: e,
    })?;
    let handle = tokio::spawn(async move {
        loop {
            let (sock, _) = match listener.accept().await {
                Ok(x) => x,
                Err(_) => break,
            };
            let body = body.clone();
            tokio::spawn(async move {
                let _ = handle_conn(sock, body, mode).await;
            });
        }
    });
    Ok((format!("http://{addr}"), handle))
}

/// 处理单条连接：读取请求头，按 `Mode` 返回 200/206。
async fn handle_conn(mut sock: TcpStream, body: Arc<Vec<u8>>, mode: Mode) -> IdmResult<()> {
    let mut buf = vec![0u8; 4096];
    let mut req = Vec::new();
    loop {
        let n = sock.read(&mut buf).await.map_err(|e| IdmError::Io {
            operation: "read request",
            path: std::path::PathBuf::new(),
            offset: 0,
            source: e,
        })?;
        if n == 0 {
            return Ok(());
        }
        req.extend_from_slice(&buf[..n]);
        if req.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if req.len() > 65536 {
            break;
        }
    }

    let header_end = req
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(req.len());
    let header_text = String::from_utf8_lossy(&req[..header_end]);
    let mut lines = header_text.split("\r\n");
    let _request_line = lines.next();
    let mut range: Option<(u64, u64)> = None;
    for line in lines {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        if k.trim().eq_ignore_ascii_case("Range") {
            range = parse_range(v.trim(), body.len() as u64);
        }
    }

    let total = body.len() as u64;
    let (status, headers, payload) = match mode {
        Mode::NoRange => {
            let mut h = String::new();
            h.push_str(&format!("Content-Length: {total}\r\n"));
            (200u16, h, body.to_vec())
        }
        Mode::Range | Mode::Slow(_) => {
            if let Some((s, e)) = range {
                let e = e.min(total.saturating_sub(1));
                let s = s.min(e);
                let slice = body[s as usize..=e as usize].to_vec();
                let mut h = String::new();
                h.push_str(&format!("Content-Range: bytes {s}-{e}/{total}\r\n"));
                h.push_str(&format!("Content-Length: {}\r\n", slice.len()));
                h.push_str("ETag: \"static-etag\"\r\n");
                (206u16, h, slice)
            } else {
                let mut h = String::new();
                h.push_str(&format!("Content-Length: {total}\r\n"));
                (200u16, h, body.to_vec())
            }
        }
    };

    let body_delay = match mode {
        Mode::Slow(ms) => ms,
        _ => 0,
    };

    let resp = format!(
        "HTTP/1.1 {status} {}\r\nConnection: close\r\n{headers}\r\n",
        status_text(status)
    );
    sock.write_all(resp.as_bytes())
        .await
        .map_err(|e| IdmError::Io {
            operation: "write response head",
            path: std::path::PathBuf::new(),
            offset: 0,
            source: e,
        })?;
    if body_delay > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(body_delay)).await;
    }
    sock.write_all(&payload).await.map_err(|e| IdmError::Io {
        operation: "write response body",
        path: std::path::PathBuf::new(),
        offset: 0,
        source: e,
    })?;
    sock.flush().await.map_err(|e| IdmError::Io {
        operation: "flush response",
        path: std::path::PathBuf::new(),
        offset: 0,
        source: e,
    })?;
    Ok(())
}

/// 解析 `bytes=START-END`（END 可为空表示到末尾）。
fn parse_range(v: &str, total: u64) -> Option<(u64, u64)> {
    let v = v.trim();
    if !v.to_ascii_lowercase().starts_with("bytes=") {
        return None;
    }
    let r = &v["bytes=".len()..];
    let (s, e) = r.split_once('-')?;
    let start = s.trim().parse::<u64>().ok()?;
    let end = match e.trim() {
        "" => total.saturating_sub(1),
        other => other.parse::<u64>().ok()?,
    };
    Some((start, end))
}

fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        206 => "Partial Content",
        _ => "OK",
    }
}

/// 生成确定性内容：第 i 字节 = i as u8。
fn make_body(total: u64) -> Vec<u8> {
    (0..total).map(|i| i as u8).collect()
}

/// 逐字节校验文件内容是否符合 `make_body` 的期望值。
fn verify_file(path: &std::path::Path, total: u64) -> IdmResult<()> {
    let data = std::fs::read(path).map_err(|e| IdmError::Io {
        operation: "read-back",
        path: path.to_path_buf(),
        offset: 0,
        source: e,
    })?;
    if (data.len() as u64) != total {
        return Err(IdmError::UnexpectedEof {
            chunk_id: 0,
            expected: total,
            received: data.len() as u64,
        });
    }
    for (i, &b) in data.iter().enumerate() {
        let expected = i as u8;
        if b != expected {
            return Err(IdmError::Io {
                operation: "content mismatch",
                path: path.to_path_buf(),
                offset: i as u64,
                source: std::io::Error::other(format!("byte {i} expected {expected} got {b}")),
            });
        }
    }
    Ok(())
}

#[tokio::test]
async fn static_chunked_download() -> std::result::Result<(), Box<dyn Error>> {
    let total: u64 = 4 * 1024 * 1024; // 4 MiB → 4 × 1 MiB 分块
    let body = Arc::new(make_body(total));
    let (url, handle) = serve(body, Mode::Range).await?;

    let tmp = tempdir()?;
    let pool = Arc::new(DiskWriterPool::new(4, 16)?);
    let config = DownloadConfig {
        connections: 4,
        min_chunk_size: 1024 * 1024,
        ..Default::default()
    };
    let opts = DownloadOptions {
        config,
        strategy: AllocationStrategy::Sparse,
        dir: tmp.path().to_path_buf(),
        out_name: Some("out.bin".to_string()),
    };

    let (outcome, _rx) = download(&url, pool.clone(), &opts, None).await?;
    assert_eq!(outcome, DownloadOutcome::Completed);

    verify_file(&tmp.path().join("out.bin"), total)?;

    let pool =
        Arc::try_unwrap(pool).map_err(|_| -> Box<dyn Error> { "pool still referenced".into() })?;
    pool.shutdown().await?;
    handle.abort();
    Ok(())
}

#[tokio::test]
async fn streaming_download_no_range() -> std::result::Result<(), Box<dyn Error>> {
    let total: u64 = 500 * 1024; // 500 KiB，不支持 Range → 单连接流式
    let body = Arc::new(make_body(total));
    let (url, handle) = serve(body, Mode::NoRange).await?;

    let tmp = tempdir()?;
    let pool = Arc::new(DiskWriterPool::new(2, 8)?);
    let config = DownloadConfig::default();
    let opts = DownloadOptions {
        config,
        strategy: AllocationStrategy::Sparse,
        dir: tmp.path().to_path_buf(),
        out_name: Some("stream.bin".to_string()),
    };

    let (outcome, _rx) = download(&url, pool.clone(), &opts, None).await?;
    assert_eq!(outcome, DownloadOutcome::Completed);

    verify_file(&tmp.path().join("stream.bin"), total)?;

    let pool =
        Arc::try_unwrap(pool).map_err(|_| -> Box<dyn Error> { "pool still referenced".into() })?;
    pool.shutdown().await?;
    handle.abort();
    Ok(())
}

#[tokio::test]
async fn cancel_mid_download() -> std::result::Result<(), Box<dyn Error>> {
    let total: u64 = 8 * 1024 * 1024; // 8 MiB，配合 Slow 服务器用于中途取消
    let body = Arc::new(make_body(total));
    let (url, handle) = serve(body, Mode::Slow(3000)).await?;

    let tmp = tempdir()?;
    let pool = Arc::new(DiskWriterPool::new(4, 16)?);
    let config = DownloadConfig {
        connections: 4,
        min_chunk_size: 1024 * 1024,
        ..Default::default()
    };
    let opts = DownloadOptions {
        config,
        strategy: AllocationStrategy::Sparse,
        dir: tmp.path().to_path_buf(),
        out_name: Some("cancel.bin".to_string()),
    };

    let notify = Arc::new(Notify::new());
    let notify_task = notify.clone();
    let pool_task = pool.clone();
    let dl =
        tokio::spawn(
            async move { download(&url, pool_task, &opts, Some(notify_task.as_ref())).await },
        );

    // 等 probe 完成、分块任务进入等待响应体的阶段再取消。
    tokio::time::sleep(std::time::Duration::from_millis(900)).await;
    notify.notify_one();

    let result = dl.await?;
    let (outcome, _rx) = result?;
    assert_eq!(outcome, DownloadOutcome::Cancelled);

    // 取消后保留 .idm-part，不重命名。
    let tmp_part = tmp.path().join("cancel.bin.idm-part");
    assert!(tmp_part.exists(), "partial file must be retained on cancel");

    let pool =
        Arc::try_unwrap(pool).map_err(|_| -> Box<dyn Error> { "pool still referenced".into() })?;
    pool.shutdown().await?;
    handle.abort();
    Ok(())
}
