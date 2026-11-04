//! Phase 1b B6 本地测试服务器：仅用 `tokio::net::TcpListener` 手写的最小 HTTP/1.1
//! 服务器，不引入 hyper 等依赖。
//!
//! 内容按偏移量确定性生成（不占内存，可模拟 GB 级文件）。通过 [`ServerConfig`]
//! 可模拟 B6 列出的全部服务器行为。

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// 每块生成大小。
const BLOCK: usize = 64 * 1024;

/// 服务器对 Range 的处理方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeMode {
    /// 正常支持 Range：分块请求返回 206。
    Range,
    /// 完全不支持 Range：总是返回 200。
    NoRange,
    /// 总长未知：不发送 Content-Length，直接关闭连接表示结束。
    UnknownLength,
    /// 206 但总长为 `*`（支持 Range 但总长未知，按不支持分块处理）。
    StarTotal,
    /// 对分块 Range 请求返回 200（Probe 仍返回 206）。
    RangeReturn200,
    /// 206 但 Content-Range 与请求区间不一致。
    BadContentRange,
}

/// 服务器配置；所有字段都在请求处理中被读取。
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// 逻辑文件总长。
    pub size: u64,
    /// Range 行为。
    pub range: RangeMode,
    /// 可选的 Content-Disposition（含 `filename*=UTF-8''` 中文文件名）。
    pub content_disposition: Option<String>,
    /// 可选的 Content-Encoding（如 gzip）。
    pub content_encoding: Option<String>,
    /// 强 ETag 原始值（含引号），如 `"v1"`。
    pub etag: Option<String>,
    /// 是否把 ETag 标为弱 ETag `W/...`。
    pub weak_etag: bool,
    /// 第 N 个分块请求之后更换 ETag；之后的 If-Range 不匹配则返回 200。
    pub change_etag_after: Option<u64>,
    /// `/start` 是否返回 302 到 `/target`。
    pub redirect: bool,
    /// 写到该绝对字节位置后断开连接。
    pub break_at: Option<u64>,
    /// 每发送一块数据后的延迟（慢速但持续发送，用于验证空闲超时不会误杀）。
    pub chunk_delay: Option<Duration>,
    /// 写到该绝对字节位置后，静默停顿 `freeze_duration` 再关闭连接（用于触发读取空闲超时）。
    pub freeze_after: Option<u64>,
    /// 触发 `freeze_after` 后的静默停顿时长（应大于客户端的 `read_timeout`）。
    pub freeze_duration: Option<Duration>,
    /// 可选的 Last-Modified 响应头（用于弱 ETag + Last-Modified 作为 If-Range 的测试）。
    pub last_modified: Option<String>,
    /// Probe 请求（Range: bytes=0-0）响应前的延迟。
    pub probe_delay: Option<Duration>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            size: 1024,
            range: RangeMode::Range,
            content_disposition: None,
            content_encoding: None,
            etag: None,
            weak_etag: false,
            change_etag_after: None,
            redirect: false,
            break_at: None,
            chunk_delay: None,
            freeze_after: None,
            freeze_duration: None,
            last_modified: None,
            probe_delay: None,
        }
    }
}

/// 内容按偏移量确定性生成，可模拟 GB 级文件而不占内存。
pub fn byte_at(offset: u64) -> u8 {
    let x = offset ^ (offset >> 8) ^ (offset >> 16) ^ (offset >> 24) ^ (offset >> 32);
    (x & 0xff) as u8
}

fn fill_block(buf: &mut [u8], base: u64) {
    for (k, b) in buf.iter_mut().enumerate() {
        *b = byte_at(base + k as u64);
    }
}

/// 逐字节校验文件内容与服务器生成规则一致。
pub fn verify_file(path: &Path, size: u64) -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read(path)?;
    if data.len() as u64 != size {
        return Err(format!("file length mismatch: expected {size}, got {}", data.len()).into());
    }
    for (i, &b) in data.iter().enumerate() {
        let expected = byte_at(i as u64);
        if b != expected {
            return Err(format!("byte {i}: expected {expected}, got {b}").into());
        }
    }
    Ok(())
}

/// 已启动的测试服务器。
pub struct TestServer {
    base: String,
    handle: tokio::task::JoinHandle<()>,
}

impl TestServer {
    /// 拼接 URL（如 `/start`、`/target`）。
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// 停止 accept 循环（已建立的短连接会自行结束）。
    pub fn shutdown(&self) {
        self.handle.abort();
    }
}

/// 启动服务器，返回可构造 URL 的句柄。
pub async fn start(config: ServerConfig) -> std::io::Result<TestServer> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let base = format!("http://127.0.0.1:{port}");
    let cfg = Arc::new(config);
    let counter = Arc::new(AtomicU64::new(0));
    let handle = tokio::spawn(async move {
        loop {
            let (sock, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => break,
            };
            let c = cfg.clone();
            let n = counter.clone();
            tokio::spawn(async move {
                let _ = handle_conn(sock, c, n, port).await;
            });
        }
    });
    Ok(TestServer { base, handle })
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

/// 解析 `bytes=A-B`；B 省略表示到末尾。
fn parse_range(v: &str, size: u64) -> Option<(u64, u64)> {
    let v = v.trim();
    if !v.to_ascii_lowercase().starts_with("bytes=") {
        return None;
    }
    let (a, b) = v[6..].split_once('-')?;
    let start = a.trim().parse::<u64>().ok()?;
    let end = if b.trim().is_empty() {
        size.saturating_sub(1)
    } else {
        b.trim().parse::<u64>().ok()?
    };
    Some((start, end.min(size.saturating_sub(1))))
}

async fn handle_conn(
    mut sock: TcpStream,
    cfg: Arc<ServerConfig>,
    counter: Arc<AtomicU64>,
    port: u16,
) -> std::io::Result<()> {
    let raw = read_headers(&mut sock).await?;
    let text = String::from_utf8_lossy(&raw);
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();
    let mut range_header: Option<String> = None;
    let mut if_range: Option<String> = None;
    for line in lines {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        match k.trim().to_ascii_lowercase().as_str() {
            "range" => range_header = Some(v.trim().to_string()),
            "if-range" => if_range = Some(v.trim().to_string()),
            _ => {}
        }
    }

    if cfg.redirect && path.starts_with("/start") {
        let head = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/target\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        sock.write_all(head.as_bytes()).await?;
        return Ok(());
    }

    let is_probe = range_header.as_deref() == Some("bytes=0-0");
    let request_no = if is_probe {
        0
    } else {
        counter.fetch_add(1, Ordering::Relaxed) + 1
    };
    let changed = cfg
        .change_etag_after
        .is_some_and(|threshold| request_no > threshold);

    let prefix = if cfg.weak_etag { "W/" } else { "" };
    let base_etag = cfg.etag.clone().unwrap_or_else(|| "\"v1\"".to_string());
    let current_etag = if changed {
        format!("{prefix}\"v2\"")
    } else {
        format!("{prefix}{base_etag}")
    };
    let base_lm = cfg
        .last_modified
        .clone()
        .unwrap_or_else(|| "Wed, 01 Jan 2025 00:00:00 GMT".to_string());
    // 变更后同时更换 ETag 与 Last-Modified，使任意 If-Range（ETag 或 Last-Modified）都失配。
    let current_lm = if changed {
        "Thu, 02 Jan 2026 00:00:00 GMT".to_string()
    } else {
        base_lm
    };
    let if_range_mismatch = changed && if_range.is_some();

    if let Some(d) = cfg.probe_delay
        && is_probe
    {
        tokio::time::sleep(d).await;
    }

    let size = cfg.size;
    // Probe 请求在 Range / StarTotal / BadContentRange / RangeReturn200 下都返回
    // 206，这样 download 才会走静态分块；异常只作用于后续分块请求。
    let support_206 = matches!(
        cfg.range,
        RangeMode::Range
            | RangeMode::StarTotal
            | RangeMode::BadContentRange
            | RangeMode::RangeReturn200
    );
    let force_200 = if_range_mismatch
        || cfg.range == RangeMode::NoRange
        || cfg.range == RangeMode::UnknownLength
        || (!is_probe && cfg.range == RangeMode::RangeReturn200);
    let parsed_range = range_header.as_deref().and_then(|v| parse_range(v, size));

    let (status, start, length, content_range, content_length) = if let Some((s, e)) = parsed_range
    {
        if support_206 && !force_200 {
            let cr = if cfg.range == RangeMode::BadContentRange && !is_probe {
                format!("bytes 0-0/{size}")
            } else if cfg.range == RangeMode::StarTotal {
                format!("bytes {s}-{e}/*")
            } else {
                format!("bytes {s}-{e}/{size}")
            };
            (206u16, s, e - s + 1, Some(cr), true)
        } else {
            let cl = cfg.range != RangeMode::UnknownLength;
            (200u16, 0u64, size, None, cl)
        }
    } else {
        let cl = cfg.range != RangeMode::UnknownLength;
        (200u16, 0u64, size, None, cl)
    };

    let mut head = format!(
        "HTTP/1.1 {} {}\r\n",
        status,
        if status == 206 {
            "Partial Content"
        } else {
            "OK"
        }
    );
    // 刻意总是发送 Accept-Ranges: bytes：Probe 不能只看这个头判断支持 Range。
    head.push_str("Accept-Ranges: bytes\r\n");
    if let Some(cr) = &content_range {
        head.push_str(&format!("Content-Range: {cr}\r\n"));
    }
    if content_length {
        head.push_str(&format!("Content-Length: {length}\r\n"));
    }
    if let Some(cd) = &cfg.content_disposition {
        head.push_str(&format!("Content-Disposition: {cd}\r\n"));
    }
    if let Some(ce) = &cfg.content_encoding {
        head.push_str(&format!("Content-Encoding: {ce}\r\n"));
    }
    if cfg.etag.is_some() || changed {
        head.push_str(&format!("ETag: {current_etag}\r\n"));
    }
    if cfg.last_modified.is_some() {
        head.push_str(&format!("Last-Modified: {current_lm}\r\n"));
    }
    head.push_str("Content-Type: application/octet-stream\r\n");
    head.push_str("Connection: close\r\n\r\n");
    sock.write_all(head.as_bytes()).await?;

    // 提前断开模式：Content-Length 仍声明完整长度，但写到 break_at 后直接关闭
    // 连接，让客户端观察到响应体被截断。
    let mut written = 0u64;
    while written < length {
        let n = ((length - written) as usize).min(BLOCK);
        let mut buf = vec![0u8; n];
        fill_block(&mut buf, start + written);
        // 冻结模式：已写到 freeze_after 之后，静默停顿 freeze_duration 再关闭连接。
        // 停顿期间客户端收不到任何字节，其 read_timeout 会先于本函数返回而触发超时错误。
        if let Some(fa) = cfg.freeze_after {
            let abs = start + written;
            if abs >= fa {
                if let Some(d) = cfg.freeze_duration {
                    tokio::time::sleep(d).await;
                }
                return Ok(());
            }
        }
        // 提前断开：只写到 break_at 为止就关闭连接，保证 Content-Length 声明的
        // 长度无法被满足，客户端必须观察到响应体被截断。
        if let Some(b) = cfg.break_at {
            let abs = start + written;
            if abs >= b {
                return Ok(());
            }
            if abs + n as u64 >= b {
                let partial = (b - abs) as usize;
                sock.write_all(&buf[..partial]).await?;
                return Ok(());
            }
        }
        sock.write_all(&buf).await?;
        written += n as u64;
        if let Some(d) = cfg.chunk_delay {
            tokio::time::sleep(d).await;
        }
    }
    sock.flush().await?;
    Ok(())
}
