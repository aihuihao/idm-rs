//! 网络层：HTTP 客户端、Probe、响应头与文件名解析。

pub mod client;
pub mod filename;
pub mod headers;
pub mod probe;

pub use client::DownloadConfig;
pub use filename::{infer_filename, resolve_conflict, sanitize_windows_filename};
pub use headers::{ContentRange, HeaderParseError, parse_content_disposition, parse_content_range};
pub use probe::{Probe, ProbeResult};
/// 重导出 `reqwest::Url`，便于上层构造/校验 URL 而无需直接依赖 reqwest。
pub use reqwest::Url;
