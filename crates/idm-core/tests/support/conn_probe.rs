//! Phase 3a 接管零重叠测试专用服务器：精确统计活动 TCP 连接数。
//!
//! 与 `support/mod.rs` 的通用服务器不同，这里在 accept 时立刻 +1、连接结束
//! （读到对端 EOF / 发送完毕）时立刻 -1，不做任何"事后剔除"，也不使用
//! `peek` 在限速 sleep 里提前注销。这样 `max_active` 就是真实的并发连接峰值。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::support::byte_at;

/// 服务器对外暴露的观测状态。
#[derive(Clone)]
pub struct ConnStats {
    /// 当前活动连接数。
    pub active: Arc<AtomicU64>,
    /// 全程活动连接峰值。
    pub max_active: Arc<AtomicU64>,
    /// 被停住的连接是否在 20 秒到期前被客户端关闭。
    pub closed_early: Arc<AtomicBool>,
    /// 从开始停住到检测到关闭的毫秒数。
    pub close_after_ms: Arc<AtomicU64>,
    /// 是否有新请求在旧连接关闭之后才到达。
    pub new_request_after_close: Arc<AtomicBool>,
    /// 是否已经见过被停住的那条连接（内部判重用）。
    stalled_seen: Arc<AtomicBool>,
}

impl Default for ConnStats {
    fn default() -> Self {
        ConnStats {
            active: Arc::new(AtomicU64::new(0)),
            max_active: Arc::new(AtomicU64::new(0)),
            closed_early: Arc::new(AtomicBool::new(false)),
            close_after_ms: Arc::new(AtomicU64::new(u64::MAX)),
            new_request_after_close: Arc::new(AtomicBool::new(false)),
            stalled_seen: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl ConnStats {
    pub fn peak(&self) -> u64 {
        self.max_active.load(Ordering::SeqCst)
    }
    pub fn closed(&self) -> bool {
        self.closed_early.load(Ordering::SeqCst)
    }
}

pub struct ConnProbeServer {
    pub url: String,
    pub stats: ConnStats,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for ConnProbeServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

struct ActiveGuard(Arc<AtomicU64>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn fill(buf: &mut [u8], base: u64) {
    for (i, b) in buf.iter_mut().enumerate() {
        *b = byte_at(base + i as u64);
    }
}

async fn read_headers(sock: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0u8; 4096];
    let mut all = Vec::new();
    loop {
        let n = sock.read(&mut buf).await?;
        if n == 0 {
            return Ok(all);
        }
        all.extend_from_slice(&buf[..n]);
        if all.windows(4).any(|w| w == b"\r\n\r\n") || all.len() > 65536 {
            break;
        }
    }
    Ok(all)
}

fn parse_range(text: &str, size: u64) -> Option<(u64, u64)> {
    let mut range = None;
    for line in text.split("\r\n").skip(1) {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        if k.trim().eq_ignore_ascii_case("range") {
            let v = v.trim();
            let body = v.strip_prefix("bytes=").unwrap_or(v);
            let (a, b) = body.split_once('-')?;
            let start = a.trim().parse::<u64>().ok()?;
            let end = if b.trim().is_empty() {
                size - 1
            } else {
                b.trim().parse::<u64>().ok()?
            };
            range = Some((start, end.min(size - 1)));
        }
    }
    range
}

async fn handle(
    mut sock: TcpStream,
    size: u64,
    stall_bytes: u64,
    stall_duration: Duration,
    stats: ConnStats,
) -> std::io::Result<()> {
    let now = stats.active.fetch_add(1, Ordering::SeqCst) + 1;
    stats.max_active.fetch_max(now, Ordering::SeqCst);
    let _guard = ActiveGuard(Arc::clone(&stats.active));

    let raw = read_headers(&mut sock).await?;
    if raw.is_empty() {
        return Ok(());
    }
    let text = String::from_utf8_lossy(&raw).to_string();
    let (start, end) = parse_range(&text, size).unwrap_or((0, size - 1));
    let is_probe = start == 0 && end == 0;
    let length = end - start + 1;

    // 非停住的连接：若旧连接已被客户端关闭，标记"新请求在关闭之后到达"。
    if stats.stalled_seen.load(Ordering::SeqCst) && stats.closed_early.load(Ordering::SeqCst) {
        stats.new_request_after_close.store(true, Ordering::SeqCst);
    }

    let head = if is_probe {
        format!(
            "HTTP/1.1 206 Partial Content\r\nAccept-Ranges: bytes\r\nContent-Range: bytes 0-0/{size}\r\nContent-Length: 1\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n"
        )
    } else {
        format!(
            "HTTP/1.1 206 Partial Content\r\nAccept-Ranges: bytes\r\nContent-Range: bytes {start}-{end}/{size}\r\nContent-Length: {length}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n"
        )
    };
    sock.write_all(head.as_bytes()).await?;

    let stall_this = !is_probe && start == 0 && !stats.stalled_seen.swap(true, Ordering::SeqCst);
    let mut written = 0u64;
    let mut stalled = false;
    while written < length {
        if stall_this && !stalled && written >= stall_bytes {
            stalled = true;
            let stall_started = Instant::now();
            let mut probe = [0u8; 64];
            let closed = tokio::time::timeout(stall_duration, async {
                loop {
                    match sock.read(&mut probe).await {
                        Ok(0) | Err(_) => return true,
                        Ok(_) => continue,
                    }
                }
            })
            .await;
            if matches!(closed, Ok(true)) {
                stats.closed_early.store(true, Ordering::SeqCst);
                stats
                    .close_after_ms
                    .store(stall_started.elapsed().as_millis() as u64, Ordering::SeqCst);
                return Ok(());
            }
        }
        let mut n = ((length - written) as usize).min(64 * 1024);
        if stall_this {
            // 1~16 KiB 大小不一的块，保证 reserved 不对齐 64 KiB。
            let h = (start ^ written).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 33;
            n = n.min(1024 + (h % (16 * 1024 - 1024 + 1)) as usize);
            if written < stall_bytes {
                n = n.min((stall_bytes - written) as usize);
            }
        }
        let mut buf = vec![0u8; n];
        fill(&mut buf, start + written);
        sock.write_all(&buf).await?;
        written += n as u64;
    }
    sock.flush().await?;
    Ok(())
}

/// 启动服务器。第 0 号分块按大小不一的块写出 `stall_bytes` 后停住 `stall_duration`。
pub async fn start(
    size: u64,
    stall_bytes: u64,
    stall_duration: Duration,
) -> std::io::Result<ConnProbeServer> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let stats = ConnStats::default();
    let s = stats.clone();
    let handle = tokio::spawn(async move {
        loop {
            let (sock, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => break,
            };
            let s = s.clone();
            tokio::spawn(async move {
                let _ = handle(sock, size, stall_bytes, stall_duration, s).await;
            });
        }
    });
    Ok(ConnProbeServer {
        url: format!("http://127.0.0.1:{port}/file.bin"),
        stats,
        handle,
    })
}
