//! URL 校验：供 Native Messaging host 等不可信输入入口使用。

use crate::net::Url;

/// 校验 URL：总长 ≤ 8 KiB、不含空白字符/控制字符、scheme 只能是 http/https、
/// `host_str()` 必须非空、authority 不得为空（`http:///x`）、不得携带 userinfo
/// （用户名或密码）。内网单标签主机名（如 `nas`）合法。
///
/// 使用 `crate::net::Url`（reqwest/url crate）做解析，可正确拒绝
/// `http://a b/`、`http://:80/`、`http://[::1/` 等畸形 authority；
/// `http:///x` 因 url crate 会规范化出 host，额外在解析前按原串拦截。
/// 错误信息一律是固定字符串、不含 URL 本身，凭据既不会入库也不会进入日志。
pub fn validate_url(url: &str) -> Result<Url, String> {
    if url.len() > 8 * 1024 {
        return Err("url exceeds 8 KiB".to_string());
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("url contains whitespace or control characters".to_string());
    }
    // url crate 会把 `http:///x` 规范化成 `http://x/` 并给出非空 host，
    // 但这类输入的 authority 在原串中为空，必须在解析前拒绝（host 入口也受益）。
    if url
        .split_once("://")
        .is_some_and(|(_, rest)| rest.starts_with('/'))
    {
        return Err("url must have a host".to_string());
    }
    let parsed =
        Url::parse(url).map_err(|_| "url must be a valid http(s) URL with a host".to_string())?;
    match parsed.scheme() {
        "http" | "https" => {}
        _ => return Err("url scheme must be http or https".to_string()),
    }
    match parsed.host_str() {
        Some(h) if !h.is_empty() => {}
        _ => return Err("url must have a host".to_string()),
    }
    // 浏览器传来的 URL 不得携带 userinfo：凭据不该被写入数据库、日志或错误回复，
    // 因此在入队前整条拒绝。
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("url must not contain credentials".to_string());
    }
    Ok(parsed)
}

#[cfg(test)]
mod url_tests {
    use super::*;

    /// 断言 `validate_url` 对三个边界输入的实际返回值。
    #[test]
    fn validate_url_returns_for_three_inputs() {
        assert!(validate_url("http://:80/").is_err());
        assert!(validate_url("http://[::1/").is_err());
        assert_eq!(
            validate_url("http://user:pass@host/x").err().as_deref(),
            Some("url must not contain credentials")
        );
    }

    /// userinfo 的各种形态（仅用户名 / 用户名+密码 / 仅密码）一律拒绝，
    /// 错误信息固定且不含凭据本身。
    #[test]
    fn validate_url_rejects_all_userinfo_forms() {
        for url in [
            "http://user@host/x",
            "http://user:pass@host/x",
            "http://:pass@host/x",
            "https://user:pass@example.com/a",
        ] {
            assert_eq!(
                validate_url(url).err().as_deref(),
                Some("url must not contain credentials"),
                "{url} 必须被拒绝"
            );
        }
        // 无 userinfo 的内网主机名 / 普通 URL 仍允许。
        assert!(validate_url("http://nas/file.zip").is_ok());
        assert!(validate_url("http://example.com/x").is_ok());
    }

    /// authority 为空的 URL（url crate 会规范化出 host）也必须被拒绝。
    #[test]
    fn validate_url_rejects_empty_authority() {
        assert_eq!(
            validate_url("http:///x").err().as_deref(),
            Some("url must have a host")
        );
        assert_eq!(
            validate_url("https:///path").err().as_deref(),
            Some("url must have a host")
        );
    }
}
