//! HTTP 响应头解析的纯函数（无 IO、无依赖），便于单元测试。
//!
//! 仅实现 Probe 与分块下载所需的最小子集：
//! - `Content-Range`（`bytes start-end/total`，total 可为 `*`）
//! - `Content-Disposition`（`filename*=UTF-8''...` RFC 5987 优先于 `filename=`）

use thiserror::Error;

/// `Content-Range` 解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentRange {
    pub start: u64,
    pub end: u64,
    pub total: Option<u64>,
}

/// `Content-Range` / `Content-Disposition` 解析错误（仅本模块内部使用，不是对外错误类型）。
#[derive(Debug, Error, PartialEq, Eq)]
pub enum HeaderParseError {
    #[error("empty header value")]
    Empty,
    #[error("missing 'bytes ' prefix")]
    BadPrefix,
    #[error("malformed range section")]
    MalformedRange,
    #[error("invalid number in header")]
    InvalidNumber,
    #[error("total size does not match the requested/calculated length")]
    TotalMismatch,
}

impl ContentRange {
    /// 校验：已知 `expected_total` 时，`total`（若给出）必须一致；`end >= start`。
    pub fn validate(&self, expected_total: Option<u64>) -> Result<(), HeaderParseError> {
        if self.end < self.start {
            return Err(HeaderParseError::MalformedRange);
        }
        if let (Some(given), Some(exp)) = (self.total, expected_total)
            && given != exp
        {
            return Err(HeaderParseError::TotalMismatch);
        }
        Ok(())
    }
}

/// 解析 `Content-Range` 头。接受 `bytes 0-0/1234`、`bytes 0-0/*`、`bytes */1234` 等写法。
pub fn parse_content_range(value: &str) -> Result<ContentRange, HeaderParseError> {
    let v = value.trim();
    if v.is_empty() {
        return Err(HeaderParseError::Empty);
    }
    if !v.to_ascii_lowercase().starts_with("bytes ") {
        return Err(HeaderParseError::BadPrefix);
    }
    let body = &v[6..];
    let (range_part, total_part) = body
        .rsplit_once('/')
        .ok_or(HeaderParseError::MalformedRange)?;
    let (start_s, end_s) = range_part
        .split_once('-')
        .ok_or(HeaderParseError::MalformedRange)?;
    let start: u64 = start_s
        .trim()
        .parse()
        .map_err(|_| HeaderParseError::InvalidNumber)?;
    let end: u64 = end_s
        .trim()
        .parse()
        .map_err(|_| HeaderParseError::InvalidNumber)?;
    let total = match total_part.trim() {
        "*" => None,
        t => Some(t.parse().map_err(|_| HeaderParseError::InvalidNumber)?),
    };
    ContentRange { start, end, total }
        .validate(None)
        .map(|()| ContentRange { start, end, total })
}

/// 解析 `Content-Disposition`，返回已百分号解码的文件名。优先 RFC 5987 的
/// `filename*=UTF-8''...`，其次 `filename=`。两者都没有时返回 `None`。
pub fn parse_content_disposition(value: &str) -> Option<String> {
    if let Some(name) = parse_rfc5987(value) {
        return Some(name);
    }
    parse_filename_param(value)
}

/// 解析 RFC 5987 扩展：`filename*=charset'lang'value`，仅支持 UTF-8（大小写不敏感）。
fn parse_rfc5987(value: &str) -> Option<String> {
    let lower = value.to_ascii_lowercase();
    let marker = "filename*=";
    let idx = lower.find(marker)?;
    let after = &value[idx + marker.len()..];
    // 形如 `UTF-8''<pct-encoded>` 或 `UTF-8'lang'<pct-encoded>`。
    let after_lower = after.to_ascii_lowercase();
    let (charset_lang, encoded) = if let Some(p) = after_lower.find("''") {
        // 前半段应是 `utf-8`（语言为空）。
        let cl = &after_lower[..p];
        if cl.trim() != "utf-8" {
            return None;
        }
        (&after[..p], &after[p + 2..])
    } else {
        // `charset'lang'value`：先找到第一个 `'`，再找第二个 `'`。
        let first = after.find('\'')?;
        let cl_end = after[first + 1..].find('\'')? + first + 1;
        let cl = &after_lower[..first];
        if cl.trim() != "utf-8" {
            return None;
        }
        (&after[..first], &after[cl_end + 1..])
    };
    let _ = charset_lang;
    Some(percent_decode(encoded))
}

/// 解析普通 `filename=` 参数（引号串或裸 token），并百分号解码。
fn parse_filename_param(value: &str) -> Option<String> {
    let lower = value.to_ascii_lowercase();
    let marker = "filename=";
    let idx = lower.find(marker)?;
    let after = &value[idx + marker.len()..];
    let trimmed = after.trim_start();
    if let Some(rest) = trimmed.strip_prefix('"') {
        // 取直到下一个未转义的双引号。
        let mut out = String::new();
        let mut chars = rest.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '"' {
                return Some(percent_decode(&out));
            }
            if c == '\\'
                && let Some(n) = chars.next()
            {
                out.push(n);
                continue;
            }
            out.push(c);
        }
        // 没有闭合引号：把已收集部分返回。
        Some(percent_decode(&out))
    } else {
        // 裸 token：取到 `;` 或结尾。
        let end = trimmed.find(';').unwrap_or(trimmed.len());
        let token = &trimmed[..end];
        let token = token.trim();
        if token.is_empty() {
            return None;
        }
        Some(percent_decode(token))
    }
}

/// 手写百分号解码（%XX → 字节；非 ASCII 按 UTF-8 解释）。不引入额外依赖。
pub fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() && is_hex(bytes[i + 1]) && is_hex(bytes[i + 2]) {
            let hi = hex_val(bytes[i + 1]) as u8;
            let lo = hex_val(bytes[i + 2]) as u8;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    // 按 UTF-8 解释；无效序列替换为 U+FFFD。
    String::from_utf8_lossy(&out).into_owned()
}

fn is_hex(b: u8) -> bool {
    b.is_ascii_hexdigit()
}

fn hex_val(b: u8) -> u32 {
    b.to_ascii_lowercase() as u32
        - if b.is_ascii_digit() {
            b'0' as u32
        } else {
            b'a' as u32 - 10
        }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_full() {
        let cr = parse_content_range("bytes 0-0/1234").unwrap();
        assert_eq!(
            cr,
            ContentRange {
                start: 0,
                end: 0,
                total: Some(1234)
            }
        );
    }

    #[test]
    fn content_range_star_total() {
        let cr = parse_content_range("bytes 100-199/*").unwrap();
        assert_eq!(
            cr,
            ContentRange {
                start: 100,
                end: 199,
                total: None
            }
        );
        assert!(cr.validate(Some(200)).is_ok());
    }

    #[test]
    fn content_range_validate_mismatch() {
        let cr = parse_content_range("bytes 0-0/1234").unwrap();
        assert_eq!(cr.validate(Some(999)), Err(HeaderParseError::TotalMismatch));
        assert!(cr.validate(Some(1234)).is_ok());
    }

    #[test]
    fn content_range_rejects_bad() {
        assert_eq!(
            parse_content_range("bytes 10-5/100"),
            Err(HeaderParseError::MalformedRange)
        );
        assert_eq!(
            parse_content_range("0-0/100"),
            Err(HeaderParseError::BadPrefix)
        );
        assert_eq!(
            parse_content_range("bytes a-b/100"),
            Err(HeaderParseError::InvalidNumber)
        );
        assert_eq!(parse_content_range(""), Err(HeaderParseError::Empty));
    }

    #[test]
    fn disposition_rfc5987_utf8() {
        // "中文.txt" 的百分号编码。
        let v = "attachment; filename*=UTF-8''%E4%B8%AD%E6%96%87.txt";
        assert_eq!(parse_content_disposition(v).as_deref(), Some("中文.txt"));
    }

    #[test]
    fn disposition_plain_filename() {
        assert_eq!(
            parse_content_disposition("attachment; filename=\"my file.zip\"").as_deref(),
            Some("my file.zip")
        );
        assert_eq!(
            parse_content_disposition("filename=plain.bin").as_deref(),
            Some("plain.bin")
        );
    }

    #[test]
    fn disposition_rfc5987_preferred() {
        let v = "attachment; filename=fallback.txt; filename*=UTF-8''%E4%B8%AD.txt";
        assert_eq!(parse_content_disposition(v).as_deref(), Some("中.txt"));
    }

    #[test]
    fn disposition_none() {
        assert_eq!(parse_content_disposition("attachment"), None);
    }

    #[test]
    fn percent_decode_basic() {
        assert_eq!(percent_decode("a%20b%2Ec"), "a b.c");
    }
}
