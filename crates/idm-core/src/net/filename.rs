//! 文件名推断与 Windows 合法化（纯函数，便于单元测试）。

use std::path::{Path, PathBuf};

use reqwest::Url;

use crate::net::headers::percent_decode;

/// Windows 文件名中不允许的字符。
const ILLEGAL: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// 把非法字符（含控制字符）替换为 `_`，处理保留名，去掉末尾的点/空格，并限制长度。
///
/// 保留名（CON/PRN/AUX/NUL/COM1-9/LPT1-9，含带扩展名形式）通过整体前缀 `_` 规避。
pub fn sanitize_windows_filename(raw: &str) -> String {
    if raw.is_empty() {
        return "download".to_string();
    }
    let mapped: String = raw
        .chars()
        .map(|c| {
            if (c as u32) < 0x20 || ILLEGAL.contains(&c) {
                '_'
            } else {
                c
            }
        })
        .collect();

    // 先处理保留名（基于不含扩展名的主名）。
    let (stem, _ext) = split_ext(&mapped);
    let mapped = if is_reserved(stem) {
        format!("_{mapped}")
    } else {
        mapped
    };

    // 去掉末尾的点和空格（Windows 不允许）。
    let trimmed = mapped.trim_end_matches(['.', ' ']).to_string();
    let trimmed = if trimmed.is_empty() {
        "download".to_string()
    } else {
        trimmed
    };

    // 截断到 200 字符，保留扩展名。
    if trimmed.len() <= 200 {
        return trimmed;
    }
    let (stem, ext) = split_ext(&trimmed);
    let max_stem = 200usize.saturating_sub(ext.len());
    let truncated: String = stem.chars().take(max_stem).collect();
    format!("{truncated}{ext}")
}

/// 在 `dir` 下解析不冲突的目标文件名：`name` 不存在直接返回，否则依次 `name (1).ext`。
///
/// 判断占用时，`name` 本身或其对应的临时文件 `name.idm-part` 任一存在都视为已占用——
/// 否则两个任务会同时下载到同一个 `.idm-part`，最终重命名时互相覆盖。
pub fn resolve_conflict(dir: &Path, name: &str) -> PathBuf {
    let part_name = |base: &str| -> PathBuf { dir.join(format!("{base}.idm-part")) };
    let occupied = |base: &str| -> bool {
        let p = dir.join(base);
        p.exists() || part_name(base).exists()
    };
    if !occupied(name) {
        return dir.join(name);
    }
    let (stem, ext) = split_ext(name);
    let mut n = 1u32;
    loop {
        let new_name = format!("{stem} ({n}){ext}");
        if !occupied(&new_name) {
            return dir.join(&new_name);
        }
        n += 1;
    }
}

/// 文件名推断：Content-Disposition（RFC 5987 > 普通）> URL 路径最后一段（解码）> `download`。
pub fn infer_filename(url: &Url, content_disposition: Option<&str>) -> String {
    let raw = content_disposition
        .and_then(parse_content_disposition)
        .or_else(|| url_path_segment(url))
        .unwrap_or_else(|| "download".to_string());
    sanitize_windows_filename(&raw)
}

fn parse_content_disposition(value: &str) -> Option<String> {
    crate::net::headers::parse_content_disposition(value)
}

fn url_path_segment(url: &Url) -> Option<String> {
    url.path_segments()
        .and_then(|mut segs| segs.rfind(|s| !s.is_empty()))
        .map(percent_decode)
}

/// 拆分为（主名，扩展名含点）。没有扩展名时扩展名为空串。
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(pos) if pos > 0 => (&name[..pos], &name[pos..]),
        _ => (name, ""),
    }
}

/// 主名是否为 Windows 保留名（大小写不敏感）。
fn is_reserved(stem: &str) -> bool {
    let up = stem.to_ascii_uppercase();
    match up.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" => true,
        _ => {
            if let Some(rest) = up.strip_prefix("COM") {
                rest.len() == 1 && rest.as_bytes()[0].is_ascii_digit()
            } else if let Some(rest) = up.strip_prefix("LPT") {
                rest.len() == 1 && rest.as_bytes()[0].is_ascii_digit()
            } else {
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_illegal_chars() {
        assert_eq!(sanitize_windows_filename("a/b:c*"), "a_b_c_");
        assert_eq!(sanitize_windows_filename("q?w\"e"), "q_w_e");
    }

    #[test]
    fn sanitize_reserved_names() {
        assert_eq!(sanitize_windows_filename("CON"), "_CON");
        assert_eq!(sanitize_windows_filename("con.txt"), "_con.txt");
        assert_eq!(sanitize_windows_filename("COM1"), "_COM1");
        assert_eq!(sanitize_windows_filename("lpt9.bin"), "_lpt9.bin");
        // 非保留名不变。
        assert_eq!(sanitize_windows_filename("normal"), "normal");
    }

    #[test]
    fn sanitize_trailing_dots_spaces() {
        assert_eq!(sanitize_windows_filename("file. "), "file");
        assert_eq!(sanitize_windows_filename("file.txt."), "file.txt");
        assert_eq!(sanitize_windows_filename("  lead"), "  lead");
    }

    #[test]
    fn sanitize_max_length_keeps_ext() {
        let long = "a".repeat(300);
        let out = sanitize_windows_filename(&long);
        assert!(out.len() <= 200);
        assert!(out.starts_with('a'));
    }

    #[test]
    fn infer_from_url_segment() {
        let url = Url::parse("https://example.com/path/to/my%20file.bin").unwrap();
        assert_eq!(infer_filename(&url, None), "my file.bin");
    }

    #[test]
    fn infer_from_disposition_preferred() {
        let url = Url::parse("https://example.com/x").unwrap();
        let cd = "attachment; filename=fallback.txt; filename*=UTF-8''%E4%B8%AD.txt";
        assert_eq!(infer_filename(&url, Some(cd)), "中.txt");
    }

    #[test]
    fn infer_fallback_download() {
        let url = Url::parse("https://example.com/").unwrap();
        assert_eq!(infer_filename(&url, None), "download");
    }

    #[test]
    fn resolve_conflict_numbers() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let _ = std::fs::File::create(dir.join("f.bin")).unwrap();
        let first = resolve_conflict(dir, "f.bin");
        assert_eq!(first, dir.join("f (1).bin"));
        let _ = std::fs::File::create(&first).unwrap();
        let second = resolve_conflict(dir, "f.bin");
        assert_eq!(second, dir.join("f (2).bin"));
    }
}
