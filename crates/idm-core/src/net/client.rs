//! HTTP 客户端构建与下载配置。

use std::time::Duration;

use reqwest::Url;
use reqwest::header::{ACCEPT_ENCODING, HeaderMap, HeaderValue};

use crate::error::{Error, Result};
use crate::net::host_limit::MAX_PER_HOST_CEILING;

/// 代理配置（Phase 3a）。默认 `System`：使用系统 / 环境变量代理。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ProxyConfig {
    /// 使用系统 / 环境变量（`HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` 等）。
    #[default]
    System,
    /// 显式禁用代理（`ClientBuilder::no_proxy()`）。
    Disabled,
    /// 自定义代理 URL，支持 `http://`、`https://`、`socks5://`、`socks5h://`，
    /// 并可内嵌 `user:pass@host:port` 认证信息。
    Custom(String),
}

/// 校验自定义代理 URL，返回解析后的 `Url`。
fn validate_proxy_url(raw: &str) -> Result<Url> {
    let parsed = Url::parse(raw).map_err(|e| Error::InvalidProxy {
        url: raw.to_string(),
        reason: format!("cannot parse proxy URL: {e}"),
    })?;
    let supported = matches!(parsed.scheme(), "http" | "https" | "socks5" | "socks5h");
    if !supported {
        return Err(Error::InvalidProxy {
            url: raw.to_string(),
            reason: format!(
                "unsupported proxy scheme {:?} (allowed: http, https, socks5, socks5h)",
                parsed.scheme()
            ),
        });
    }
    if parsed.host_str().unwrap_or("").is_empty() {
        return Err(Error::InvalidProxy {
            url: raw.to_string(),
            reason: "proxy URL has no host".to_string(),
        });
    }
    Ok(parsed)
}

/// 下载配置。连接数默认 8（范围 1..=32），最小分块大小默认 1 MiB。
///
/// **TLS 策略**：本结构体不含任何 TLS 配置项，也刻意不提供
/// `danger_accept_invalid_certs` / `danger_accept_invalid_hostnames`、
/// 自定义根证书或关闭证书校验的开关。`build_client()` 直接使用 reqwest 的默认
/// TLS 后端，证书链与主机名校验始终开启，调用方无法削弱。
#[derive(Clone)]
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
    /// 退避基准：第一次重试等待该时长，之后每次翻倍，封顶 `retry_max_delay`。
    /// 默认 1 秒；测试可调小以加速。
    pub retry_base_delay: Duration,
    /// 退避封顶。默认 30 秒；叠加随机抖动后仍会被钳制到该上限，因此单次等待
    /// 永远不会超过该值。
    pub retry_max_delay: Duration,
    /// 动态切分后两段各自的最小长度（Phase 2）。默认 1 MiB；小于该值时不得切分。
    pub min_split_size: u64,
    /// 是否启用动态切分与工作窃取（Phase 2）。默认开启；`--no-split` 关闭。
    pub dynamic_split: bool,
    /// 每个 Host 的并发连接上限（Phase 3a）。默认 8，合法范围 1..=64。
    /// 使用共享 [`crate::net::HostConnectionLimiter`] 时以其构造参数为准。
    pub max_connections_per_host: usize,
    /// 代理配置（Phase 3a）。默认使用系统 / 环境变量代理。
    pub proxy: ProxyConfig,
    /// Phase 4a：浏览器接管透传的请求头（Referer / Cookie / User-Agent）。
    ///
    /// 仅这三个名字（不区分大小写）被 [`validate_extra_headers`] 接受；值不得含 `\r`/`\n`，
    /// 单值 ≤ 8 KiB。注入到 `build_client` 的默认请求头，对 Probe 与所有分块 / 重试 / 续传
    /// 请求统一生效。`User-Agent` 覆盖默认值，其余名字并入默认头。
    pub extra_headers: Vec<(String, String)>,
}

impl std::fmt::Debug for DownloadConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownloadConfig")
            .field("connections", &self.connections)
            .field("min_chunk_size", &self.min_chunk_size)
            .field("connect_timeout", &self.connect_timeout)
            .field("read_timeout", &self.read_timeout)
            .field("user_agent", &self.user_agent)
            .field("max_redirects", &self.max_redirects)
            .field("max_retries", &self.max_retries)
            .field("retry_base_delay", &self.retry_base_delay)
            .field("retry_max_delay", &self.retry_max_delay)
            .field("min_split_size", &self.min_split_size)
            .field("dynamic_split", &self.dynamic_split)
            .field("max_connections_per_host", &self.max_connections_per_host)
            .field("proxy", &self.proxy)
            // Cookie 的值不进入任何日志 / 调试输出，仅显示是否存在。
            .field(
                "extra_headers",
                &self
                    .extra_headers
                    .iter()
                    .map(|(k, v)| {
                        if k.eq_ignore_ascii_case("Cookie") {
                            (k.as_str(), "<cookie>")
                        } else {
                            (k.as_str(), v.as_str())
                        }
                    })
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
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
            retry_base_delay: Duration::from_secs(1),
            retry_max_delay: Duration::from_secs(30),
            min_split_size: 1024 * 1024,
            dynamic_split: true,
            max_connections_per_host: 8,
            proxy: ProxyConfig::System,
            extra_headers: Vec::new(),
        }
    }
}

/// Phase 4a：校验浏览器传来的请求头。
///
/// 只允许 `Referer` / `Cookie` / `User-Agent`（不区分大小写）；值中不得含 `\r` 或 `\n`；
/// 单值长度不超过 8 KiB。错误信息中**不包含**任何请求头的值（避免泄露 Cookie）。
pub fn validate_extra_headers(headers: &[(String, String)]) -> Result<()> {
    const ALLOWED: [&str; 3] = ["Referer", "Cookie", "User-Agent"];
    const MAX_VALUE: usize = 8 * 1024;
    for (name, value) in headers {
        if !ALLOWED.iter().any(|a| a.eq_ignore_ascii_case(name)) {
            return Err(Error::InvalidArgument {
                message: format!("header name not allowed: {name}"),
            });
        }
        if value.contains('\r') || value.contains('\n') {
            return Err(Error::InvalidArgument {
                message: format!("header value for {name} contains forbidden CR/LF"),
            });
        }
        if value.len() > MAX_VALUE {
            return Err(Error::InvalidArgument {
                message: format!("header value for {name} exceeds {MAX_VALUE} bytes"),
            });
        }
    }
    Ok(())
}

impl DownloadConfig {
    pub fn new() -> Self {
        Self::default()
    }

    /// 校验参数：连接数必须在 1..=32，每 Host 连接上限必须在 1..=64。
    pub fn validate(&self) -> Result<()> {
        if self.connections < 1 || self.connections > 32 {
            return Err(Error::InvalidArgument {
                message: format!("connections must be in 1..=32, got {}", self.connections),
            });
        }
        if !(1..=MAX_PER_HOST_CEILING).contains(&self.max_connections_per_host) {
            return Err(Error::InvalidArgument {
                message: format!(
                    "max_connections_per_host must be in 1..={MAX_PER_HOST_CEILING}, got {}",
                    self.max_connections_per_host
                ),
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

    /// 构建 reqwest 客户端：强制 `Accept-Encoding: identity`、限制重定向次数，
    /// 并按 [`ProxyConfig`] 应用代理（Phase 3a）。
    ///
    /// 三层超时（见 `ARCHITECTURE.md`）：
    /// - `connect_timeout`：建立 TCP/TLS 连接的最长耗时；
    /// - `read_timeout`：两次收到响应体字节之间的最长空闲间隔（空闲超时），
    ///   由 Client 级别统一施加，**不**针对单个请求设总时长上限；
    /// - Probe 单独用 [`Self::request_timeout`] 设一个总时长上限（它不读取正文）。
    ///
    /// 不调用任何 `danger_*` 方法，证书校验保持默认开启；Cargo 依赖也只启用默认
    /// TLS 后端 + `stream` / `socks`，未开启 gzip/brotli/zstd/deflate 等压缩特性。
    pub fn build_client(&self) -> Result<reqwest::Client> {
        self.validate()?;
        // 先校验代理 URL，让非法配置在发任何请求之前以强类型错误返回。
        let proxy_url = match &self.proxy {
            ProxyConfig::Custom(raw) => Some(validate_proxy_url(raw)?),
            _ => None,
        };
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        // Phase 4a：把浏览器透传的请求头（Referer / Cookie）并入默认头；
        // User-Agent 单独覆盖默认值（见下方 `builder.user_agent`）。
        for (name, value) in &self.extra_headers {
            if name.eq_ignore_ascii_case("User-Agent") {
                continue;
            }
            if let (Ok(hn), Ok(hv)) = (
                reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_bytes(value.as_bytes()),
            ) {
                headers.insert(hn, hv);
            }
        }
        let mut builder = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(self.connect_timeout)
            .read_timeout(self.read_timeout)
            .redirect(reqwest::redirect::Policy::limited(self.max_redirects));
        // User-Agent：浏览器传来则覆盖默认值，否则保持。
        let mut user_agent = self.user_agent.clone();
        for (name, value) in &self.extra_headers {
            if name.eq_ignore_ascii_case("User-Agent") {
                user_agent = value.clone();
            }
        }
        builder = builder.user_agent(user_agent);
        match &self.proxy {
            ProxyConfig::System => {
                // reqwest 默认读取系统 / 环境变量代理，不做额外设置。
            }
            ProxyConfig::Disabled => {
                builder = builder.no_proxy();
            }
            ProxyConfig::Custom(raw) => {
                let url = proxy_url.as_ref().ok_or_else(|| Error::InvalidProxy {
                    url: raw.clone(),
                    reason: "proxy URL unexpectedly missing".to_string(),
                })?;
                // 先关掉系统/环境变量代理，保证 Custom 是唯一生效的代理；
                // reqwest 会从 URL 内嵌的 user:pass 解析代理认证信息。
                let proxy = reqwest::Proxy::all(url.clone()).map_err(|e| Error::InvalidProxy {
                    url: raw.clone(),
                    reason: e.to_string(),
                })?;
                builder = builder.no_proxy().proxy(proxy);
            }
        }
        builder.build().map_err(|e| Error::Http {
            url: String::new(),
            source: e,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_config_default_is_system() {
        assert_eq!(ProxyConfig::default(), ProxyConfig::System);
        assert_eq!(DownloadConfig::default().max_connections_per_host, 8);
    }

    /// Phase 3a：非法代理 URL 与不支持的 scheme 返回强类型错误。
    #[test]
    fn invalid_proxy_urls_are_rejected() {
        for bad in [
            "ftp://127.0.0.1:1080",
            "socks4://127.0.0.1:1080",
            "not a url",
            "http://",
        ] {
            let cfg = DownloadConfig {
                proxy: ProxyConfig::Custom(bad.to_string()),
                ..DownloadConfig::default()
            };
            assert!(
                matches!(cfg.build_client(), Err(Error::InvalidProxy { .. })),
                "expected InvalidProxy for {bad:?}"
            );
        }
    }

    #[test]
    fn supported_proxy_urls_build_a_client() {
        for good in [
            "http://127.0.0.1:3128",
            "https://user:pass@127.0.0.1:3128",
            "socks5://127.0.0.1:1080",
            "socks5h://user:pass@127.0.0.1:1080",
        ] {
            let cfg = DownloadConfig {
                proxy: ProxyConfig::Custom(good.to_string()),
                ..DownloadConfig::default()
            };
            assert!(cfg.build_client().is_ok(), "expected client for {good:?}");
        }
        assert!(
            DownloadConfig {
                proxy: ProxyConfig::Disabled,
                ..DownloadConfig::default()
            }
            .build_client()
            .is_ok()
        );
    }
}
