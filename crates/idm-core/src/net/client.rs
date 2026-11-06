//! HTTP 客户端构建与下载配置。

use std::time::Duration;

use reqwest::Url;
use reqwest::header::{ACCEPT_ENCODING, HeaderMap, HeaderValue};

use crate::error::{Error, Result};

/// 下载配置。连接数默认 8（范围 1..=32），最小分块大小默认 1 MiB。
///
/// **TLS 策略**：本结构体不含任何 TLS 配置项，也刻意不提供
/// `danger_accept_invalid_certs` / `danger_accept_invalid_hostnames`、
/// 自定义根证书或关闭证书校验的开关。`build_client()` 直接使用 reqwest 的默认
/// TLS 后端，证书链与主机名校验始终开启，调用方无法削弱。
#[derive(Debug, Clone)]
pub struct DownloadConfig {
    pub connections: usize,
    pub min_chunk_size: u64,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub user_agent: String,
    pub max_redirects: usize,
    /// 分块连续失败次数上限（Phase 1d）。单次尝试写入 ≥1 MiB 时清零计数。
    /// 0 表示不重试。另有每分块总尝试次数硬上限（`retry::MAX_TOTAL_ATTEMPTS`）。
    pub max_retries: u32,
    /// 退避基准：第一次重试等待该时长，之后每次翻倍，封顶 `retry_backoff_cap`。
    /// 默认 1 秒；测试可调小以加速。
    pub retry_backoff_base: Duration,
    /// 退避封顶。默认 30 秒；叠加随机抖动后仍会被钳制到该上限，因此单次等待
    /// 永远不会超过 30 秒。
    pub retry_backoff_cap: Duration,
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
            max_retries: 5,
            retry_backoff_base: Duration::from_secs(1),
            retry_backoff_cap: Duration::from_secs(30),
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

    /// **仅供 Probe 使用** 的单请求总超时：连接 + 读取。Probe 不读取响应体，因此
    /// 整个请求的总时长可以设上限。分块/顺序下载请求**不得**使用此总时长超时
    /// （大分块在慢速网络下会因读取全部响应体超时而失败），它们依赖 Client 级别的
    /// `connect_timeout`（连接超时）与 `read_timeout`（两次收到数据之间的空闲超时）。
    pub fn request_timeout(&self) -> Duration {
        self.connect_timeout + self.read_timeout
    }

    /// 构建 reqwest 客户端：强制 `Accept-Encoding: identity`、限制重定向次数。
    /// 代理设置留到 Phase 3，本阶段不做。
    ///
    /// 三层超时（见 `ARCHITECTURE.md`）：
    /// - `connect_timeout`：建立 TCP/TLS 连接的最长耗时；
    /// - `read_timeout`：两次收到响应体字节之间的最长空闲间隔（空闲超时），
    ///   由 Client 级别统一施加，**不**针对单个请求设总时长上限；
    /// - Probe 单独用 [`Self::request_timeout`] 设一个总时长上限（它不读取正文）。
    ///
    /// 不调用任何 `danger_*` 方法，证书校验保持默认开启；Cargo 依赖也只启用默认
    /// TLS 后端 + `stream`，未开启 gzip/brotli/zstd/deflate 等压缩特性。
    pub fn build_client(&self) -> Result<reqwest::Client> {
        self.validate()?;
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        reqwest::Client::builder()
            .user_agent(&self.user_agent)
            .default_headers(headers)
            .connect_timeout(self.connect_timeout)
            .read_timeout(self.read_timeout)
            .redirect(reqwest::redirect::Policy::limited(self.max_redirects))
            .build()
            .map_err(|e| Error::Http {
                url: String::new(),
                source: e,
            })
    }
}
