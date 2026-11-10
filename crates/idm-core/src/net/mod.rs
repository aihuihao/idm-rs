//! 网络层：HTTP 客户端、Probe、响应头与文件名解析。

pub mod client;
pub mod filename;
pub mod headers;
pub mod host_limit;
pub mod probe;
pub mod ratelimit;
pub mod retry;

pub use client::{DownloadConfig, ProxyConfig};
pub use filename::{infer_filename, resolve_conflict, sanitize_windows_filename};
pub use headers::{ContentRange, HeaderParseError, parse_content_disposition, parse_content_range};
pub use host_limit::{
    DEFAULT_MAX_PER_HOST, HostConnectionLimiter, HostKey, HostPermit, HostPermitFuture,
    MAX_PER_HOST_CEILING, host_key,
};
pub use probe::{Probe, ProbeResult};
pub use ratelimit::{BandwidthLimiter, RateLimiter};
/// 重导出 `reqwest::Url`，便于上层构造/校验 URL 而无需直接依赖 reqwest。
pub use reqwest::Url;
pub use retry::{
    MAX_TOTAL_ATTEMPTS, PROGRESS_RESET_BYTES, XorShift64, backoff_delay, is_retryable,
    is_retryable_status, parse_retry_after,
};
