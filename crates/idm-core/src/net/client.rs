//! HTTP 客户端构建与下载配置。

use std::time::Duration;

use reqwest::Url;
use reqwest::header::{ACCEPT_ENCODING, HeaderMap, HeaderValue};

use crate::error::{Error, Result};

/// 下载配置。连接数默认 8（范围 1..=32），最小分块大小默认 1 MiB。
#[derive(Debug, Clone)]
pub struct DownloadConfig {
    pub connections: usize,
    pub min_chunk_size: u64,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub user_agent: String,
    pub max_redirects: usize,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        DownloadConfig {
            connections: 8,
            min_chunk_size: 1024 * 1024,
            connect_timeout: Duration::from_secs(30),
            read_timeout: Duration::from_secs(30),
            user_agent: "idm-rs/0.1.0".to_string(),
            max_redirects: 10,
        }
    }
}

impl DownloadConfig {
    pub fn new() -> Self {
        Self::default()
    }

    /// 校验参数：连接数必须在 1..=32。
    pub fn validate(&self) -> Result<()> {
        if self.connections < 1 || self.connections > 32 {
            return Err(Error::InvalidArgument {
                message: format!("connections must be in 1..=32, got {}", self.connections),
            });
        }
        Ok(())
    }

    /// 仅允许 http / https，其他 scheme 一律拒绝。
    pub fn check_scheme(url: &Url) -> Result<()> {
        match url.scheme() {
            "http" | "https" => Ok(()),
            other => Err(Error::InvalidArgument {
                message: format!("unsupported URL scheme: {other} (only http/https allowed)"),
            }),
        }
    }

    /// 单请求总超时（连接 + 读取），用于 `request.timeout()`。
    pub fn request_timeout(&self) -> Duration {
        self.connect_timeout + self.read_timeout
    }

    /// 构建 reqwest 客户端：强制 `Accept-Encoding: identity`、限制重定向次数。
    /// 代理设置留到 Phase 3，本阶段不做。
    pub fn build_client(&self) -> Result<reqwest::Client> {
        self.validate()?;
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        reqwest::Client::builder()
            .user_agent(&self.user_agent)
            .default_headers(headers)
            .connect_timeout(self.connect_timeout)
            .redirect(reqwest::redirect::Policy::limited(self.max_redirects))
            .build()
            .map_err(|e| Error::Http {
                url: String::new(),
                source: e,
            })
    }
}
