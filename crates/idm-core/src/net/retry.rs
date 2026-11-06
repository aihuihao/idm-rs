//! 分块级自动重试：错误分类、指数退避与 `Retry-After` 解析（Phase 1d）。
//!
//! 分类原则：只有**临时性**错误才值得重试——网络抖动、连接重置、读取空闲超时、
//! 响应体中途截断，以及明确的临时状态码（408/429/500/502/503/504）。远端内容
//! 变化、Range 不被遵守、压缩编码、磁盘满（应暂停而非重试）、磁盘写入错误和
//! 参数错误一律直接失败，重试只会得到相同结果。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::Error;

/// 可重试的 HTTP 状态码：请求超时、限流与常见网关/服务端临时错误。
pub fn is_retryable_status(code: u16) -> bool {
    matches!(code, 408 | 429 | 500 | 502 | 503 | 504)
}

/// 错误是否属于临时性错误、值得重试。
///
/// - [`Error::UnexpectedEof`]：响应体中途截断，从断点重发 Range 即可续传；
/// - [`Error::Http`]：
///   - 带状态码（`error_for_status` 产生）→ 按状态码分类；
///   - 不带状态码（网络层）→ 连接失败 / 空闲超时 / 请求发送 / 响应体读取 /
///     解码错误视为临时性错误；builder（参数）与重定向错误不在白名单；
/// - 其余一律不可重试：`RemoteChanged` / `RangeNotHonored` / `InvalidContentRange` /
///   `UnsupportedContentEncoding`（重试必然得到相同结果）、磁盘满（按原规则暂停
///   任务）、任何磁盘写入错误、数据库与参数错误。
pub fn is_retryable(e: &Error) -> bool {
    match e {
        Error::UnexpectedEof { .. } => true,
        Error::Http { source, .. } => match source.status() {
            Some(code) => is_retryable_status(code.as_u16()),
            None => {
                source.is_connect()
                    || source.is_timeout()
                    || source.is_request()
                    || source.is_body()
                    || source.is_decode()
            }
        },
        _ => false,
    }
}

/// 每个分块的总尝试次数硬上限：防止服务器每次只发送少量数据后断开导致的
/// 无限循环（即使每次都有 ≥1 MiB 进展从而不断清零连续失败计数）。
pub const MAX_TOTAL_ATTEMPTS: u32 = 100;

/// 清零连续失败计数所需的最小单次进展（见任务二.4）。
pub const PROGRESS_RESET_BYTES: u64 = 1024 * 1024;

/// xorshift64*：不引入 rand，以时间为种子的简单伪随机数发生器，
/// 仅用于退避抖动，不要求密码学安全性。
pub struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    /// 种子为 0 时使用固定非零常数，避免全零状态退化。
    pub fn from_seed(seed: u64) -> Self {
        XorShift64 {
            state: if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            },
        }
    }

    /// 用当前时间（纳秒）播种。
    pub fn from_now() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x1234_5678);
        Self::from_seed(nanos ^ (nanos << 21) ^ (nanos >> 13))
    }

    /// 返回 `[0, 1)` 的伪随机浮点数（53 位精度）。
    pub fn next_f64(&mut self) -> f64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        let v = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (v >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// 指数退避加 ±20% 随机抖动：`base × 2^consecutive_failures`，抖动后仍封顶 `cap`。
/// 第一次（consecutive_failures = 0）等待 `base`（默认 1 秒），之后每次翻倍。
pub fn backoff_delay(
    consecutive_failures: u32,
    base: Duration,
    cap: Duration,
    rng: &mut XorShift64,
) -> Duration {
    let exp = base.as_secs_f64() * 2f64.powi(consecutive_failures.min(62) as i32);
    let capped = exp.min(cap.as_secs_f64());
    let jitter = 1.0 + (rng.next_f64() * 0.4 - 0.2);
    Duration::from_secs_f64((capped * jitter).max(0.001).min(cap.as_secs_f64()))
}

/// 解析 `Retry-After` 头（仅秒数格式）；无效值返回 `None`，上限 60 秒。
pub fn parse_retry_after(v: &str) -> Option<Duration> {
    let secs: u64 = v.trim().parse().ok()?;
    Some(Duration::from_secs(secs.min(60)))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

    #[test]
    fn static_error_classification() {
        // 可重试：中途断开。
        assert!(is_retryable(&Error::UnexpectedEof {
            chunk_id: 0,
            expected: 10,
            received: 5,
        }));
        // 不可重试：远端变化 / Range 语义 / 压缩编码。
        assert!(!is_retryable(&Error::RemoteChanged));
        assert!(!is_retryable(&Error::RangeNotHonored));
        assert!(!is_retryable(&Error::InvalidContentRange {
            requested: (0, 1),
            actual: "x".to_string(),
        }));
        assert!(!is_retryable(&Error::UnsupportedContentEncoding {
            encoding: "gzip".to_string(),
        }));
        // 不可重试：磁盘满（应暂停）、任何磁盘写入错误。
        assert!(!is_retryable(&Error::DiskFull {
            path: PathBuf::from("d:/x"),
            needed: 1,
            available: 0,
        }));
        assert!(!is_retryable(&Error::DiskFullDuringWrite {
            path: PathBuf::from("d:/x"),
            offset: 0,
        }));
        assert!(!is_retryable(&Error::WriteZero {
            path: PathBuf::from("d:/x"),
            offset: 0,
        }));
        assert!(!is_retryable(&Error::Io {
            operation: "write",
            path: PathBuf::from("d:/x"),
            offset: 0,
            source: std::io::Error::other("io"),
        }));
        // 不可重试：其余类别。
        assert!(!is_retryable(&Error::PoolClosed));
        assert!(!is_retryable(&Error::InvalidArgument {
            message: "x".to_string(),
        }));
        assert!(!is_retryable(&Error::Resume {
            message: "x".to_string()
        }));
    }

    #[test]
    fn status_code_table() {
        for code in [408u16, 429, 500, 502, 503, 504] {
            assert!(is_retryable_status(code), "{code} must be retryable");
        }
        for code in [200u16, 302, 400, 403, 404, 409, 416, 451] {
            assert!(!is_retryable_status(code), "{code} must not be retryable");
        }
    }

    /// 迷你本地服务器：对所有请求返回固定状态码（仅本地回环，不访问外网）。
    async fn status_error(code: u16) -> Result<Error, Box<dyn std::error::Error>> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let srv = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let _ = sock.read(&mut buf).await;
                    let head = format!(
                        "HTTP/1.1 {code} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                });
            }
        });
        let resp = reqwest::get(format!("http://127.0.0.1:{port}/")).await?;
        let result = match resp.error_for_status() {
            Err(e) => Ok(Error::Http {
                url: format!("http://127.0.0.1:{port}/"),
                source: e,
            }),
            Ok(_) => Err(format!("status {code} should have produced an error").into()),
        };
        srv.abort();
        result
    }

    #[tokio::test]
    async fn http_status_errors_are_classified_by_code() -> TestResult {
        for code in [408u16, 429, 500, 502, 503, 504] {
            let e = status_error(code).await?;
            assert!(is_retryable(&e), "{code} must be retryable");
        }
        for code in [403u16, 404, 416] {
            let e = status_error(code).await?;
            assert!(!is_retryable(&e), "{code} must not be retryable");
        }
        Ok(())
    }

    #[tokio::test]
    async fn connection_refused_is_retryable() -> TestResult {
        // 绑定后立即释放端口：后续连接几乎必然被拒绝。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        drop(listener);
        match reqwest::get(format!("http://127.0.0.1:{port}/x")).await {
            Err(e) => {
                let err = Error::Http {
                    url: format!("http://127.0.0.1:{port}/x"),
                    source: e,
                };
                assert!(is_retryable(&err), "connection failure must be retryable");
            }
            Ok(_) => return Err("expected a connection failure".into()),
        }
        Ok(())
    }

    #[test]
    fn backoff_doubles_then_caps_with_jitter() -> TestResult {
        let base = Duration::from_secs(1);
        let cap = Duration::from_secs(30);
        let mut rng = XorShift64::from_seed(42);
        for cf in 0..6u32 {
            let d = backoff_delay(cf, base, cap, &mut rng);
            let expected = (base.as_secs_f64() * 2f64.powi(cf as i32)).min(30.0);
            let got = d.as_secs_f64();
            assert!(
                got >= expected * 0.8 - 0.01 && got <= expected * 1.2 + 0.01,
                "cf={cf}: got {got:.3}s, expected around {expected:.3}s"
            );
        }
        // 大指数下抖动后仍封顶在 30s（抖动不得突破 cap）。
        let d = backoff_delay(30, base, cap, &mut rng);
        assert!(d.as_secs_f64() <= 30.0 + 0.01, "cap violated: {d:?}");
        // 抖动始终在 ±20% 内。
        let mut rng2 = XorShift64::from_seed(7);
        for _ in 0..200 {
            let d = backoff_delay(0, base, cap, &mut rng2);
            let got = d.as_secs_f64();
            assert!(
                (0.79..=1.21).contains(&got),
                "jitter out of range: {got:.3}s"
            );
        }
        Ok(())
    }

    #[test]
    fn retry_after_parsing() {
        assert_eq!(parse_retry_after("2"), Some(Duration::from_secs(2)));
        assert_eq!(parse_retry_after(" 3 "), Some(Duration::from_secs(3)));
        assert_eq!(parse_retry_after("0"), Some(Duration::from_secs(0)));
        // 上限 60 秒。
        assert_eq!(parse_retry_after("120"), Some(Duration::from_secs(60)));
        // 无效值。
        assert_eq!(parse_retry_after("abc"), None);
        assert_eq!(parse_retry_after("-1"), None);
        assert_eq!(parse_retry_after(""), None);
        assert_eq!(parse_retry_after("1.5"), None);
    }

    #[test]
    fn xorshift_produces_varied_values() {
        let mut rng = XorShift64::from_seed(1);
        let mut distinct = std::collections::HashSet::new();
        for _ in 0..1000 {
            distinct.insert(rng.next_f64().to_bits());
        }
        assert!(distinct.len() > 990, "sequence looks degenerate");
        // 范围 [0,1)。
        let mut rng2 = XorShift64::from_now();
        for _ in 0..1000 {
            let v = rng2.next_f64();
            assert!((0.0..1.0).contains(&v), "out of range: {v}");
        }
    }
}
