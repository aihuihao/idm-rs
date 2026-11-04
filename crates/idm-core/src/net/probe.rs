//! Probe：发送 `Range: bytes=0-0` 探测远端是否支持 Range、总长、文件名等元信息。
//!
//! 只读取响应头，立即丢弃响应体，不下载正文。

use reqwest::Url;
use reqwest::header::{
    CONTENT_DISPOSITION, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG,
    LAST_MODIFIED, RANGE,
};

use crate::error::{Error, Result};
use crate::net::client::DownloadConfig;
use crate::net::filename::infer_filename;
use crate::net::headers::parse_content_range;

/// Probe 结果。
#[derive(Debug, Clone)]
pub struct ProbeResult {
    /// 重定向后的最终 URL。
    pub final_url: Url,
    /// 总长（已知时为 `Some`）。
    pub total_size: Option<u64>,
    /// 是否支持 Range（且能据此分块）。
    pub supports_range: bool,
    /// `(ETag 原始值, 是否为弱 ETag)`。
    pub etag: Option<(String, bool)>,
    /// Last-Modified 原始值。
    pub last_modified: Option<String>,
    /// 推断出的文件名（已 Windows 合法化）。
    pub filename: String,
    /// Content-Type。
    pub content_type: Option<String>,
}

/// 一次 Probe。
pub struct Probe<'a> {
    client: &'a reqwest::Client,
    config: &'a DownloadConfig,
}

impl<'a> Probe<'a> {
    pub fn new(client: &'a reqwest::Client, config: &'a DownloadConfig) -> Self {
        Probe { client, config }
    }

    /// 执行探测。
    pub async fn probe(&self, url: &Url) -> Result<ProbeResult> {
        DownloadConfig::check_scheme(url)?;

        let resp = self
            .client
            .get(url.clone())
            .header(RANGE, "bytes=0-0")
            .timeout(self.config.request_timeout())
            .send()
            .await
            .map_err(|e| Error::Http {
                url: url.to_string(),
                source: e,
            })?;

        let final_url = resp.url().clone();

        // 响应带有非 `identity` 的 Content-Encoding → 必须报错，绝不把压缩字节当文件内容。
        if let Some(ce) = resp.headers().get(CONTENT_ENCODING) {
            let ce = ce.to_str().unwrap_or("").trim().to_ascii_lowercase();
            if ce != "identity" {
                return Err(Error::UnsupportedContentEncoding { encoding: ce });
            }
        }

        // 4xx / 5xx 直接报错。
        resp.error_for_status_ref().map_err(|e| Error::Http {
            url: final_url.to_string(),
            source: e,
        })?;

        let status = resp.status();

        // 先抽取需要的响应头（之后再决定状态分支）。
        let content_range = resp
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| parse_content_range(v).ok());
        let content_length = resp
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        let content_disposition = resp
            .headers()
            .get(CONTENT_DISPOSITION)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let etag = resp
            .headers()
            .get(ETAG)
            .and_then(|v| v.to_str().ok())
            .map(|s| {
                let trimmed = s.trim();
                let is_weak = trimmed.to_ascii_lowercase().starts_with("w/");
                (trimmed.to_string(), is_weak)
            });
        let last_modified = resp
            .headers()
            .get(LAST_MODIFIED)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let content_type = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let filename = infer_filename(&final_url, content_disposition.as_deref());

        // 丢弃响应体，不下载正文。
        drop(resp);

        let (total_size, supports_range) = match status {
            reqwest::StatusCode::PARTIAL_CONTENT => {
                // 206：支持 Range。
                match content_range.and_then(|cr| cr.total) {
                    Some(total) => (Some(total), true),
                    // 总长为 `*`：支持 Range 但总长未知，按"不支持分块"处理。
                    None => (None, true),
                }
            }
            reqwest::StatusCode::OK => {
                // 200：不支持 Range；有 Content-Length 就记录总长。
                (content_length, false)
            }
            _ => {
                // 其它 2xx（理论上不应出现）按不支持 Range、无总长处理。
                (None, false)
            }
        };

        Ok(ProbeResult {
            final_url,
            total_size,
            supports_range,
            etag,
            last_modified,
            filename,
            content_type,
        })
    }
}
