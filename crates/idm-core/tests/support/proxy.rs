//! Phase 3a 本地测试用代理：HTTP 转发代理与 SOCKS5（RFC 1928 CONNECT）代理。
//!
//! 只依赖 `tokio::net::TcpListener`，记录经过代理的请求数；不访问外网。
//! 目标地址来自客户端发来的绝对 URI / SOCKS5 CONNECT 请求，因此只用于
//! 127.0.0.1 上的本地测试服务器。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// 已启动的测试代理。
pub struct TestProxy {
    scheme: String,
    port: u16,
    count: Arc<AtomicU64>,
    handle: tokio::task::JoinHandle<()>,
}

impl TestProxy {
    /// 传给 `--proxy` / `ProxyConfig::Custom` 的 URL。
    pub fn url(&self) -> String {
        format!("{}://127.0.0.1:{}", self.scheme, self.port)
    }

    /// 经过代理转发的请求数。
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::SeqCst)
    }
}

impl Drop for TestProxy {
    fn drop(&mut self) {
        self.handle.abort();
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

/// 双向转发，直到任一端结束（本地测试服务器每条响应都带 `Connection: close`）。
async fn relay(mut client: TcpStream, mut upstream: TcpStream) -> std::io::Result<()> {
    let (mut cr, mut cw) = client.split();
    let (mut ur, mut uw) = upstream.split();
    tokio::select! {
        r = tokio::io::copy(&mut cr, &mut uw) => { r.map(|_| ()) }
        r = tokio::io::copy(&mut ur, &mut cw) => { r.map(|_| ()) }
    }
}

async fn http_proxy_conn(mut client: TcpStream, count: Arc<AtomicU64>) -> std::io::Result<()> {
    let raw = read_headers(&mut client).await?;
    if raw.is_empty() {
        return Ok(());
    }
    let text = String::from_utf8_lossy(&raw).to_string();
    let mut lines = text.split("\r\n");
    let first = lines.next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let absolute = parts.next().unwrap_or("").to_string();
    let version = parts.next().unwrap_or("HTTP/1.1").to_string();
    // 绝对 URI（http://host:port/path）；CONNECT 形式不需要支持（目标均为 http）。
    let uri = reqwest::Url::parse(&absolute).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("bad absolute URI: {e}"),
        )
    })?;
    let host = uri
        .host_str()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "no host"))?
        .to_string();
    let port = uri.port_or_known_default().unwrap_or(80);
    let path = match uri.query() {
        Some(q) => format!("{}?{q}", uri.path()),
        None => uri.path().to_string(),
    };
    let mut forwarded = format!("{method} {path} {version}\r\n").into_bytes();
    for line in lines {
        if line.is_empty() {
            break;
        }
        forwarded.extend_from_slice(line.as_bytes());
        forwarded.extend_from_slice(b"\r\n");
    }
    forwarded.extend_from_slice(b"\r\n");

    let upstream = TcpStream::connect((host.as_str(), port)).await?;
    count.fetch_add(1, Ordering::SeqCst);
    let mut upstream = upstream;
    upstream.write_all(&forwarded).await?;
    relay(client, upstream).await
}

async fn socks5_conn(mut client: TcpStream, count: Arc<AtomicU64>) -> std::io::Result<()> {
    // 握手：VER NMETHODS METHODS → 选择无认证。
    let ver = client.read_u8().await?;
    if ver != 0x05 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "not socks5",
        ));
    }
    let n = client.read_u8().await? as usize;
    let mut methods = vec![0u8; n];
    client.read_exact(&mut methods).await?;
    client.write_all(&[0x05, 0x00]).await?;

    // CONNECT 请求：VER CMD RSV ATYP ADDR PORT。
    let mut hdr = [0u8; 4];
    client.read_exact(&mut hdr).await?;
    if hdr[0] != 0x05 || hdr[1] != 0x01 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unsupported socks command",
        ));
    }
    let host = match hdr[3] {
        0x01 => {
            let mut b = [0u8; 4];
            client.read_exact(&mut b).await?;
            format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3])
        }
        0x03 => {
            let len = client.read_u8().await? as usize;
            let mut b = vec![0u8; len];
            client.read_exact(&mut b).await?;
            String::from_utf8_lossy(&b).to_string()
        }
        0x04 => {
            let mut b = [0u8; 16];
            client.read_exact(&mut b).await?;
            let mut parts = Vec::new();
            for i in 0..8 {
                parts.push(format!("{:02x}{:02x}", b[2 * i], b[2 * i + 1]));
            }
            parts.join(":")
        }
        other => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unsupported ATYP {other}"),
            ));
        }
    };
    let mut pb = [0u8; 2];
    client.read_exact(&mut pb).await?;
    let port = u16::from_be_bytes(pb);

    let upstream = TcpStream::connect((host.as_str(), port)).await?;
    // 成功：VER REP RSV ATYP BND.ADDR BND.PORT。
    client
        .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
        .await?;
    count.fetch_add(1, Ordering::SeqCst);
    relay(client, upstream).await
}

async fn start(scheme: &str) -> std::io::Result<TestProxy> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let count = Arc::new(AtomicU64::new(0));
    let c = Arc::clone(&count);
    let is_socks = scheme == "socks5";
    let handle = tokio::spawn(async move {
        loop {
            let (sock, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => break,
            };
            let c = Arc::clone(&c);
            tokio::spawn(async move {
                let r = if is_socks {
                    socks5_conn(sock, c).await
                } else {
                    http_proxy_conn(sock, c).await
                };
                let _ = r;
            });
        }
    });
    Ok(TestProxy {
        scheme: scheme.to_string(),
        port,
        count,
        handle,
    })
}

pub async fn start_http_proxy() -> std::io::Result<TestProxy> {
    start("http").await
}

pub async fn start_socks5_proxy() -> std::io::Result<TestProxy> {
    start("socks5").await
}
