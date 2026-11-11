//! Phase 1b B6 本地测试服务器：仅用 `tokio::net::TcpListener` 手写的最小 HTTP/1.1
//! 服务器，不引入 hyper 等依赖。
//!
//! 内容按偏移量确定性生成（不占内存，可模拟 GB 级文件）。通过 [`ServerConfig`]
//! 可模拟 B6 列出的全部服务器行为。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// 速度策略：(区间起点, 请求序号) → 字节/秒；返回 `None` 表示不限速。
///
/// Phase 2 用它构造"第 0 号分块很慢、其余很快"以及"每个请求速度随机"的服务器。
pub type SpeedFn = Arc<dyn Fn(u64, u64) -> Option<u64> + Send + Sync>;

/// 速度策略的 Debug 包装（函数指针无法 Debug）。
#[derive(Clone)]
pub struct SpeedPolicy(pub SpeedFn);

impl std::fmt::Debug for SpeedPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<speed-policy>")
    }
}

/// Phase 2.2.1：让起点为 0 的第一个响应"完全停住"（保持连接、不发送、不关闭）。
///
/// 真实服务器不会在新请求到达时放开连接：停住期间除了**读**对端是否关闭，
/// 什么都不做；只有停满 `duration`（小于客户端 `read_timeout`）才继续发送。
/// 客户端若因接管而提前关闭连接，服务器把这次关闭记录下来。
#[derive(Debug, Clone)]
pub struct StallConfig {
    /// 相对响应起点发送这么多字节之后停住；`0` 表示连响应头都不发
    /// （零进度停顿：客户端卡在 `req.send()`）。
    pub after_bytes: u64,
    /// 停住时长（必须小于客户端 `read_timeout`，例如 20s < 30s）。
    pub duration: Duration,
    /// 只停住第一个起点为 0 的非 Probe 请求。零进度接管产生的新分块起点
    /// 仍是 0，不能被再次停住；由服务器用这个标记保证。
    pub stall_used: Arc<AtomicBool>,
    /// 按 1~16 KiB 大小不一的块发送 `after_bytes`（使 reserved 不对齐 64 KiB）。
    pub variable_blocks: bool,
    /// 服务器是否观察到客户端在停住期满之前就关闭了这条连接。
    pub closed_early: Arc<AtomicBool>,
    /// 从开始停住到检测到关闭的毫秒数；未提前关闭时保持 `u64::MAX`。
    pub close_after_ms: Arc<AtomicU64>,
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
    /// 只对"起点 == 前者"的那个请求，在写到后者的绝对位置后断开（Phase 2.2
    /// 退避期间被接管测试：只让第 0 号分块断一次，不受请求到达顺序影响）。
    pub break_at_start: Option<(u64, u64)>,
    /// 只对前 N 个非 Probe 响应生效（配合 `break_at` / `break_after_bytes`）；`None`
    /// 表示所有响应。用于"真实中途断开 → resume"测试：第一次下载断开，之后的
    /// 续传不再断开。
    pub break_responses: Option<u64>,
    /// **相对**当前响应起点发送 N 字节后断开（Phase 1d 重试测试）：每次重试
    /// 从新的起点开始，都会再收到 N 字节后断开，直到服务器停止断开。
    pub break_after_bytes: Option<u64>,
    /// **相对**当前响应起点发送 N 字节后静默停顿 `freeze_duration` 再关闭
    /// （触发读取空闲超时）。受 `freeze_responses` 限制。
    pub freeze_after_bytes: Option<u64>,
    /// `freeze_after_bytes` / `freeze_after` 只对前 N 个非 Probe 响应生效；
    /// `None` 表示所有响应（与旧 `freeze_after` 行为兼容）。
    pub freeze_responses: Option<u64>,
    /// 非 Probe 响应返回该状态码（Phase 1d：503+Retry-After、持续 500、404 等）。
    pub error_status: Option<u16>,
    /// 配合 `error_status` 的 `Retry-After` 头（秒）。
    pub retry_after: Option<u64>,
    /// 只对前 N 个非 Probe 响应返回 `error_status`；`None` 表示由
    /// `error_switch` 运行期开关控制（未提供开关时对所有响应生效）。
    pub error_responses: Option<u64>,
    /// 运行期错误开关：置位时（且 `error_responses` 为 None）非 Probe 响应返回
    /// `error_status`。测试可用它在"坏服务器 → 修复服务器"之间切换。
    pub error_switch: Option<Arc<AtomicBool>>,
    /// 收到的每个请求的 `Range` 头都会被 push 到这里（Phase 1d 测试 2：
    /// 验证重试请求的起点等于断开前已写入的位置）。
    pub range_sink: Option<Arc<Mutex<Vec<String>>>>,
    /// Phase 4a：所有收到的请求头（名字转小写, 值）都会被 push 到这里，用于断言
    /// 浏览器接管透传的 Referer / Cookie / User-Agent 到达了服务器。
    pub header_sink: Option<Arc<Mutex<Vec<(String, String)>>>>,
    /// 只记录**非 Probe** 请求的 Range 头（Phase 2.2：断言没有起点等于终点的请求）。
    pub chunk_range_sink: Option<Arc<Mutex<Vec<String>>>>,
    /// 非 Probe 请求计数（Phase 1d：验证"服务器一共收到 N 次请求"/
    /// "该分块只收到 1 次请求"）。
    pub request_counter: Option<Arc<AtomicU64>>,
    /// 非 Probe 请求的到达时间（Phase 1d 测试 4：测量两次请求之间的间隔）。
    pub request_times: Option<Arc<Mutex<Vec<Instant>>>>,
    /// 只对非 Probe 响应发送 `content_encoding`（Phase 1d 测试 7：让 gzip 在
    /// 下载阶段而非 Probe 阶段被拒绝，从而统计该分块的请求次数）。
    pub chunk_only_encoding: bool,
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
    /// 非 Probe 响应的速度策略（Phase 2）。
    pub speed: Option<SpeedPolicy>,
    /// 响应体每块的大小；`None` 用默认 64 KiB。Phase 2.2 的 10 KiB/s 极慢分块
    /// 需要更小的块，才能形成"持续有数据但很慢"而不是"发一块就静默 6 秒"。
    pub block: Option<usize>,
    /// 起点为 0 的响应在写出一部分数据后完全停住（Phase 2.2）。
    pub stall: Option<StallConfig>,
    /// 服务器端"当前正在发送正文的响应数"（Phase 2：验证最大并发连接数）。
    pub active: Option<Arc<AtomicU64>>,
    /// 服务器端并发峰值。
    pub max_active: Option<Arc<AtomicU64>>,
    /// 服务器实际写出的响应正文字节数（Phase 3a：动态降速测试统计真实传输量）。
    pub bytes_sent: Option<Arc<AtomicU64>>,
    /// Phase 3d：前 N 个非 Probe 响应在发送响应头之前延迟（模拟请求已发出、
    /// 服务器迟迟不响应，让这些分块持有 Host 名额却卡在 `req.send()`）。
    pub response_delay: Option<(u64, Duration)>,
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
            break_at_start: None,
            break_responses: None,
            break_after_bytes: None,
            freeze_after_bytes: None,
            freeze_responses: None,
            error_status: None,
            retry_after: None,
            error_responses: None,
            error_switch: None,
            range_sink: None,
            header_sink: None,
            chunk_range_sink: None,
            request_counter: None,
            request_times: None,
            chunk_only_encoding: false,
            chunk_delay: None,
            freeze_after: None,
            freeze_duration: None,
            last_modified: None,
            probe_delay: None,
            speed: None,
            block: None,
            stall: None,
            active: None,
            max_active: None,
            bytes_sent: None,
            response_delay: None,
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

/// 已启动的测试服务器。
pub struct TestServer {
    base: String,
    handle: tokio::task::JoinHandle<()>,
    /// 运行期 ETag 切换开关（真实事件）：置位后所有响应改用 `"v2"` 并变更
    /// Last-Modified，使 resume 时的重新 Probe 能观察到远端变化。
    pub etag_switched: Arc<AtomicBool>,
}

impl TestServer {
    /// 拼接 URL（如 `/start`、`/target`）。
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// 停止 accept 循环（已建立的短连接会自行结束），并复位运行期 ETag 开关。
    pub fn shutdown(&self) {
        self.etag_switched.store(false, Ordering::SeqCst);
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
    let etag_switched = Arc::new(AtomicBool::new(false));
    let switch_for_loop = Arc::clone(&etag_switched);
    let handle = tokio::spawn(async move {
        loop {
            let (sock, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => break,
            };
            let c = Arc::clone(&cfg);
            let n = Arc::clone(&counter);
            let e = Arc::clone(&switch_for_loop);
            tokio::spawn(async move {
                let _ = handle_conn(sock, c, n, e, port).await;
            });
        }
    });
    Ok(TestServer {
        base,
        handle,
        etag_switched,
    })
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
    etag_switch: Arc<AtomicBool>,
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
        let k_trim = k.trim();
        let v_trim = v.trim();
        // 记录所有请求头（名字转小写）到 header_sink，供 Phase 4a 测试断言透传的请求头。
        if let Some(sink) = &cfg.header_sink {
            sink.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((k_trim.to_ascii_lowercase(), v_trim.to_string()));
        }
        match k_trim.to_ascii_lowercase().as_str() {
            "range" => range_header = Some(v_trim.to_string()),
            "if-range" => if_range = Some(v_trim.to_string()),
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
    // Phase 1d 测试 2：记录收到的 Range 头，供验证重试请求的起点。
    if let Some(sink) = &cfg.range_sink {
        sink.lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(range_header.clone().unwrap_or_default());
    }
    // Phase 2.2：只记录分块请求（排除 Probe 的 bytes=0-0）。
    if !is_probe && let Some(sink) = &cfg.chunk_range_sink {
        sink.lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(range_header.clone().unwrap_or_default());
    }
    // Phase 1d：统计非 Probe 请求次数与到达时间（重试次数、Retry-After 间隔）。
    if !is_probe {
        if let Some(c) = &cfg.request_counter {
            c.fetch_add(1, Ordering::SeqCst);
        }
        if let Some(t) = &cfg.request_times {
            t.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(Instant::now());
        }
    }
    // Phase 1d 错误状态模式：只作用于非 Probe 响应。
    let should_error = if is_probe {
        false
    } else if let Some(n) = cfg.error_responses {
        request_no <= n
    } else {
        cfg.error_switch
            .as_ref()
            .is_none_or(|s| s.load(Ordering::SeqCst))
    };
    // 运行期开关（真实事件）或基于请求数的阈值，都会让 ETag / Last-Modified 变化。
    let changed = etag_switch.load(Ordering::SeqCst)
        || cfg
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

    // Phase 1d 错误状态模式：返回配置的状态码（可带 Retry-After），不发送正文。
    if should_error && let Some(code) = cfg.error_status {
        let mut head = format!("HTTP/1.1 {code} X\r\nContent-Length: 0\r\nConnection: close\r\n");
        if let Some(ra) = cfg.retry_after {
            head.push_str(&format!("Retry-After: {ra}\r\n"));
        }
        head.push_str("\r\n");
        sock.write_all(head.as_bytes()).await?;
        return Ok(());
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
        // `chunk_only_encoding`：Probe 响应保持干净，只在分块响应里声明编码，
        // 用于在下载阶段触发不可重试的 UnsupportedContentEncoding。
        if !cfg.chunk_only_encoding || !is_probe {
            head.push_str(&format!("Content-Encoding: {ce}\r\n"));
        }
    }
    if cfg.etag.is_some() || changed {
        head.push_str(&format!("ETag: {current_etag}\r\n"));
    }
    if cfg.last_modified.is_some() {
        head.push_str(&format!("Last-Modified: {current_lm}\r\n"));
    }
    head.push_str("Content-Type: application/octet-stream\r\n");
    head.push_str("Connection: close\r\n\r\n");

    // Phase 2.2.1：只对第一个起点为 0 的非 Probe 请求停住。零进度接管产生的
    // 新分块起点仍是 0，不能被再次停住（否则会无限接管）。
    let stall_this = if !is_probe && start == 0 {
        cfg.stall
            .as_ref()
            .is_some_and(|s| !s.stall_used.swap(true, Ordering::SeqCst))
    } else {
        false
    };

    // 零进度停顿：连响应头都不发，客户端卡在 req.send()。
    if stall_this
        && let Some(st) = &cfg.stall
        && st.after_bytes == 0
        && wait_for_client_close(&mut sock, st).await?
    {
        return Ok(());
    }
    // Phase 3d：前 N 个非 Probe 响应在响应头之前停住。客户端仍持有 Host 名额，
    // 服务器却迟迟不响应，正好构造「2 个请求占满名额、其余分块排队」的场景。
    if !is_probe
        && let Some((count, delay)) = cfg.response_delay
        && request_no <= count
    {
        tokio::time::sleep(delay).await;
    }
    sock.write_all(head.as_bytes()).await?;

    // Phase 2：统计服务器端同时在发送正文的响应数（最大并发请求数）。
    let _concurrency = ActiveGuard::enter(&cfg, !is_probe);
    // Phase 2：本次响应的限速（字节/秒）。
    let speed_bps = if is_probe {
        None
    } else {
        cfg.speed.as_ref().and_then(|p| (p.0)(start, request_no))
    };

    // 提前断开模式：Content-Length 仍声明完整长度，但写到 break_at 后直接关闭
    // 连接，让客户端观察到响应体被截断。
    let mut written = 0u64;
    let mut stalled = false;
    let fixed_block = cfg.block.unwrap_or(BLOCK).max(1);
    let variable = cfg.stall.as_ref().is_some_and(|s| s.variable_blocks);
    while written < length {
        // Phase 2.2.1：写出 after_bytes 之后完全停住。停住期间绝不发数据，
        // 也不主动关闭连接；停满 duration（< read_timeout）才继续。
        if stall_this
            && let Some(st) = &cfg.stall
            && !stalled
            && st.after_bytes > 0
            && written >= st.after_bytes
        {
            stalled = true;
            if wait_for_client_close(&mut sock, st).await? {
                return Ok(());
            }
        }
        let mut block = fixed_block;
        if variable {
            // 确定性伪随机：1 KiB ~ 16 KiB 之间大小不一，保证 reserved 不对齐。
            let h = (start ^ written).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 33;
            block = 1024 + (h % (16 * 1024 - 1024 + 1)) as usize;
        }
        let mut n = ((length - written) as usize).min(block);
        // 恰好发送 after_bytes 字节之后才停住（不因块大小越过这个位置）。
        if let Some(st) = &cfg.stall
            && stall_this
            && st.after_bytes > 0
            && written < st.after_bytes
        {
            n = n.min((st.after_bytes - written) as usize);
        }
        let mut buf = vec![0u8; n];
        fill_block(&mut buf, start + written);
        // 冻结模式：已写到 freeze_after 之后，静默停顿 freeze_duration 再关闭连接。
        // 停顿期间客户端收不到任何字节，其 read_timeout 会先于本函数返回而触发超时错误。
        let freeze_active =
            !is_probe && cfg.freeze_responses.is_none_or(|limit| request_no <= limit);
        let freeze_point = cfg
            .freeze_after_bytes
            .map(|n| start.saturating_add(n))
            .or(cfg.freeze_after);
        if let Some(fa) = freeze_point.filter(|_| freeze_active) {
            let abs = start + written;
            if abs >= fa {
                if let Some(d) = cfg.freeze_duration {
                    tokio::time::sleep(d).await;
                }
                return Ok(());
            }
        }
        // 只对前 N 个非 Probe 响应断开：`break_responses` 为 None 时对所有响应生效。
        // 请求编号是服务器全局递增的，因此续传时的新请求自然落在范围之外。
        let break_active = match cfg.break_responses {
            Some(n_responses) => !is_probe && request_no <= n_responses,
            None => true,
        };
        // 提前断开：只写到 break_at 为止就关闭连接，保证 Content-Length 声明的
        // 长度无法被满足，客户端必须观察到响应体被截断。
        let break_point = cfg
            .break_at_start
            .filter(|(only_start, _)| *only_start == start)
            .map(|(_, at)| at)
            .or_else(|| {
                cfg.break_after_bytes
                    .filter(|n| *n < length)
                    .map(|n| start.saturating_add(n))
            })
            .or(cfg.break_at);
        if let Some(b) = break_point.filter(|_| break_active && !is_probe) {
            let abs = start + written;
            if abs >= b {
                return Ok(());
            }
            if abs + n as u64 >= b {
                let partial = (b - abs) as usize;
                sock.write_all(&buf[..partial]).await?;
                if let Some(c) = &cfg.bytes_sent {
                    c.fetch_add(partial as u64, Ordering::SeqCst);
                }
                return Ok(());
            }
        }
        sock.write_all(&buf).await?;
        if let Some(c) = &cfg.bytes_sent {
            c.fetch_add(n as u64, Ordering::SeqCst);
        }
        written += n as u64;
        if let Some(d) = cfg.chunk_delay {
            tokio::time::sleep(d).await;
        }
        if let Some(bps) = speed_bps {
            // 按块限速：写完 n 字节后 sleep n/bps 秒。
            // 限速等待期间若客户端已经关闭连接（例如 10.5 接管时主动 drop 响应），
            // 立即结束本响应并注销并发计数——它已经不再是"正在下载"的分块，
            // 否则服务器端峰值会短暂地把已断开的旧连接与新连接一起算进去。
            let secs = n as f64 / (bps.max(1) as f64);
            let mut probe = [0u8; 64];
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs_f64(secs)) => {}
                // peek 不消费任何字节；只在读到 EOF/错误时判定客户端已断开。
                r = sock.peek(&mut probe) => {
                    if matches!(r, Ok(0) | Err(_)) {
                        return Ok(());
                    }
                }
            }
        }
    }
    sock.flush().await?;
    Ok(())
}

/// 停住期间只读取对端：读到关闭（`Ok(0)` / 错误）就记录并让调用方结束；
/// 停满 `duration` 则返回 `false`，由调用方继续发送响应。
async fn wait_for_client_close(sock: &mut TcpStream, st: &StallConfig) -> std::io::Result<bool> {
    let stall_started = Instant::now();
    let mut probe = [0u8; 64];
    let closed = tokio::time::timeout(st.duration, async {
        loop {
            match sock.read(&mut probe).await {
                Ok(0) | Err(_) => return true,
                Ok(_) => continue, // 客户端不会在响应期间发数据
            }
        }
    })
    .await;
    if matches!(closed, Ok(true)) {
        st.closed_early.store(true, Ordering::SeqCst);
        st.close_after_ms
            .store(stall_started.elapsed().as_millis() as u64, Ordering::SeqCst);
        return Ok(true);
    }
    Ok(false)
}

/// 进入响应体发送阶段时登记并发，离开时（含提前返回）注销。
struct ActiveGuard {
    active: Option<Arc<AtomicU64>>,
}

impl ActiveGuard {
    fn enter(cfg: &ServerConfig, counting: bool) -> Self {
        if !counting {
            return ActiveGuard { active: None };
        }
        let active = cfg.active.clone();
        if let Some(a) = &active {
            let now = a.fetch_add(1, Ordering::SeqCst) + 1;
            if let Some(m) = &cfg.max_active {
                m.fetch_max(now, Ordering::SeqCst);
            }
        }
        ActiveGuard { active }
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        if let Some(a) = &self.active {
            a.fetch_sub(1, Ordering::SeqCst);
        }
    }
}
