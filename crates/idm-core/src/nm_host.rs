//! Native Messaging host 注册（仅 Windows）：读写 HKCU 注册表与 manifest 文件。
//!
//! 从 idm-cli 的 `nm` 模块上移，统一由桌面程序与 CLI 共用（与 autostart 的
//! 迁移方式一致）。只用 `reg.exe` 子进程与标准库读写文件，不引入任何依赖，
//! 不触碰 HKLM。所有打印与退出码留在调用方，本模块只返回数据与字符串错误。

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// 默认 host 名（与 Firefox 扩展 background.js 中的 HOST_NAME 一致）。
pub const DEFAULT_HOST: &str = "idm_rs";
/// 默认扩展 ID（与 Firefox 扩展 manifest.json 的 id 一致）。
pub const DEFAULT_EXTENSION_ID: &str = "idm-rs@local";
/// Native Messaging host 注册表根键（仅 HKCU）。
pub const REG_ROOT: &str = r"HKCU\Software\Mozilla\NativeMessagingHosts";
const MAX_EXT_ID_LEN: usize = 128;

/// 查询状态：是否注册、manifest 路径、manifest 中的 host 路径及该文件是否存在。
#[derive(Debug, Clone)]
pub struct Status {
    pub host_name: String,
    /// 注册表键。
    pub registry_key: String,
    /// 注册表默认值是否存在。
    pub registered: bool,
    /// `reg query` 的原始输出（成功时 trim 后），供 CLI 保持原输出格式。
    pub registry_value: Option<String>,
    /// manifest 文件路径。
    pub manifest_path: PathBuf,
    pub manifest_exists: bool,
    /// 调用方传入的期望 host exe 路径（CLI 按 current_exe 同目录计算）。
    pub host_path: PathBuf,
    pub host_path_exists: bool,
    /// manifest  JSON 中 `path` 字段指向的 host exe 路径。
    pub manifest_host_path: Option<PathBuf>,
    /// manifest 中的 host 路径文件是否存在。
    pub manifest_host_path_exists: bool,
    /// manifest 中的 allowed_extensions（逗号分隔的文本）。
    pub allowed_extensions: String,
}

fn reg(args: &[&str]) -> Result<Output, String> {
    Command::new("reg")
        .args(args)
        .output()
        .map_err(|e| e.to_string())
}

/// host 名：Firefox 规定 `^\w+(\.\w+)*$`（字母数字下划线点，不能以点开头/结尾，
/// 不允许连续点）；任务进一步限定只允许小写，这里按更严格者实现。
pub fn validate_host_name(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("host name must not be empty".to_string());
    }
    if !s
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
    {
        return Err(
            "host name may contain only lowercase letters, digits, '_' and '.'".to_string(),
        );
    }
    if s.starts_with('.') || s.ends_with('.') || s.contains("..") {
        return Err("host name must match Firefox's dot-separated name rule".to_string());
    }
    Ok(())
}

/// 扩展 ID：非空、只允许字母数字与 `@` / `.` / `_` / `-`、长度 ≤ 128。
pub fn validate_ext_id(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("extension id must not be empty".to_string());
    }
    if s.len() > MAX_EXT_ID_LEN {
        return Err(format!("extension id exceeds {MAX_EXT_ID_LEN} characters"));
    }
    if !s
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'@' | b'.' | b'_' | b'-'))
    {
        return Err(
            "extension id may contain only letters, digits, '@', '.', '_' and '-'".to_string(),
        );
    }
    Ok(())
}

/// 注册表键完整路径。
pub fn reg_key(host: &str) -> String {
    format!("{REG_ROOT}\\{host}")
}

/// manifest 文件名（`<host>.json`）。
pub fn manifest_file_name(host: &str) -> String {
    format!("{host}.json")
}

/// manifest 完整路径 = `manifest_dir/<host>.json`。
pub fn manifest_path(manifest_dir: &Path, host: &str) -> PathBuf {
    manifest_dir.join(manifest_file_name(host))
}

/// 最小的 JSON 字符串转义：清单字段由本函数生成，字符集已被校验函数收窄。
pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// 从自己生成的清单文本中提取 allowed_extensions（避免运行时依赖 JSON 解析器）。
pub fn extract_allowed(text: &str) -> String {
    let marker = "\"allowed_extensions\"";
    let rest = match text.find(marker) {
        Some(pos) => &text[pos + marker.len()..],
        None => return String::new(),
    };
    match (rest.find('['), rest.find(']')) {
        (Some(a), Some(b)) if b > a => rest[a + 1..b].replace('"', "").trim().to_string(),
        _ => String::new(),
    }
}

/// 还原 json_escape 生成的最小转义序列（路径含反斜杠，必须反转义）。
fn json_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                if let Ok(code) = u32::from_str_radix(&hex, 16)
                    && let Some(ch) = char::from_u32(code)
                {
                    out.push(ch);
                }
            }
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// 从自己生成的清单文本中提取 `path` 字段（含 JSON 反转义）。
pub fn extract_host_path(text: &str) -> Option<PathBuf> {
    let marker = "\"path\"";
    let rest = &text[text.find(marker)? + marker.len()..];
    let colon = rest.find(':')?;
    let rest = &rest[colon + 1..];
    let start = rest.find('"')? + 1;
    let tail = &rest[start..];
    let end = tail.find('"')?;
    Some(PathBuf::from(json_unescape(&tail[..end])))
}

/// 注册 Native Messaging host：写 manifest（指向 host_exe_path，含 extension_ids），
/// 并把注册表默认值指向 manifest。host_exe_path 必须真实存在。
pub fn install(
    host_name: &str,
    host_exe_path: &Path,
    extension_ids: &[String],
    manifest_dir: &Path,
) -> Result<(), String> {
    validate_host_name(host_name)?;
    if extension_ids.is_empty() {
        return Err("install requires at least one extension id".to_string());
    }
    for id in extension_ids {
        validate_ext_id(id)?;
    }
    if !host_exe_path.is_file() {
        return Err(format!(
            "idm-nmhost.exe not found at {}",
            host_exe_path.display()
        ));
    }

    let manifest = manifest_path(manifest_dir, host_name);
    if let Some(dir) = manifest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }

    let ids: Vec<String> = extension_ids
        .iter()
        .map(|id| format!("\"{}\"", json_escape(id)))
        .collect();
    let ids_json = ids.join(", ");
    let body = format!(
        "{{\n  \"name\": \"{host}\",\n  \"description\": \"idm-rs Native Messaging host\",\n  \"path\": \"{path}\",\n  \"type\": \"stdio\",\n  \"allowed_extensions\": [{ids}]\n}}\n",
        host = json_escape(host_name),
        path = json_escape(&host_exe_path.to_string_lossy()),
        ids = ids_json,
    );
    std::fs::write(&manifest, body).map_err(|e| e.to_string())?;

    let key = reg_key(host_name);
    let manifest_s = manifest.to_string_lossy().into_owned();
    let out = reg(&["add", &key, "/ve", "/t", "REG_SZ", "/d", &manifest_s, "/f"])?;
    if !out.status.success() {
        return Err(format!(
            "reg add failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// 注销 Native Messaging host：删除 manifest 与注册表键，两者都幂等。
pub fn uninstall(host_name: &str, manifest_dir: &Path) -> Result<(), String> {
    validate_host_name(host_name)?;
    let manifest = manifest_path(manifest_dir, host_name);
    let key = reg_key(host_name);
    let _ = std::fs::remove_file(&manifest);
    let _ = reg(&["delete", &key, "/f"]);
    Ok(())
}

/// 查询状态（显式 manifest 目录与期望 host 路径）。供 CLI 与桌面端共用。
pub fn status_in(
    host_name: &str,
    manifest_dir: &Path,
    expected_host_exe: &Path,
) -> Result<Status, String> {
    validate_host_name(host_name)?;
    let manifest = manifest_path(manifest_dir, host_name);
    let key = reg_key(host_name);
    let q = reg(&["query", &key, "/ve"])?;
    let registered = q.status.success();
    let registry_value = if registered {
        Some(String::from_utf8_lossy(&q.stdout).trim().to_string())
    } else {
        None
    };

    let text = std::fs::read_to_string(&manifest).unwrap_or_default();
    let manifest_host_path = extract_host_path(&text);
    let manifest_host_path_exists = manifest_host_path
        .as_deref()
        .map(|p| p.is_file())
        .unwrap_or(false);
    let manifest_exists = std::fs::metadata(&manifest)
        .map(|m| m.is_file())
        .unwrap_or(false);

    Ok(Status {
        host_name: host_name.to_string(),
        registry_key: key,
        registered,
        registry_value,
        manifest_path: manifest,
        manifest_exists,
        host_path: expected_host_exe.to_path_buf(),
        host_path_exists: expected_host_exe.is_file(),
        manifest_host_path,
        manifest_host_path_exists,
        allowed_extensions: extract_allowed(&text),
    })
}

/// 查询状态（仅 host 名）：manifest 路径从注册表默认值解析。
///
/// 任务接口要求 `status(host_name)`；当注册表不存在时返回 `registered=false`
/// 的空状态。若需要显式 manifest 目录（测试注入 / 桌面程序数据目录），
/// 请用 `status_in`。
pub fn status(host_name: &str) -> Status {
    let key = reg_key(host_name);
    let empty = Status {
        host_name: host_name.to_string(),
        registry_key: key.clone(),
        registered: false,
        registry_value: None,
        manifest_path: PathBuf::new(),
        manifest_exists: false,
        host_path: PathBuf::new(),
        host_path_exists: false,
        manifest_host_path: None,
        manifest_host_path_exists: false,
        allowed_extensions: String::new(),
    };
    if validate_host_name(host_name).is_err() {
        return empty;
    }
    let q = match reg(&["query", &key, "/ve"]) {
        Ok(out) if out.status.success() => out,
        _ => return empty,
    };
    let text = String::from_utf8_lossy(&q.stdout);
    let manifest_path = text
        .lines()
        .find(|line| line.contains("REG_SZ"))
        .and_then(|line| {
            let pos = line.find("REG_SZ")? + "REG_SZ".len();
            let value = line[pos..].trim();
            if value.is_empty() {
                None
            } else {
                Some(PathBuf::from(value))
            }
        })
        .unwrap_or_default();

    let mut out = empty;
    out.registered = true;
    out.registry_value = Some(text.trim().to_string());
    out.manifest_path = manifest_path.clone();
    out.manifest_exists = manifest_path.is_file();
    if out.manifest_exists {
        let body = std::fs::read_to_string(&manifest_path).unwrap_or_default();
        out.manifest_host_path = extract_host_path(&body);
        out.manifest_host_path_exists = out
            .manifest_host_path
            .as_deref()
            .map(|p| p.is_file())
            .unwrap_or(false);
        out.allowed_extensions = extract_allowed(&body);
        if let Some(ref p) = out.manifest_host_path {
            out.host_path = p.clone();
            out.host_path_exists = p.is_file();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rules() {
        assert!(validate_host_name("idm_rs").is_ok());
        assert!(validate_host_name("a.b_c").is_ok());
        assert!(validate_host_name("").is_err());
        assert!(validate_host_name("UPPER").is_err());
        assert!(validate_host_name(".leading").is_err());
        assert!(validate_host_name("a..b").is_err());
        assert!(validate_ext_id(DEFAULT_EXTENSION_ID).is_ok());
        assert!(validate_ext_id("").is_err());
        assert!(validate_ext_id(&"x".repeat(129)).is_err());
    }

    #[test]
    fn manifest_helpers_roundtrip() {
        let host = "idm_test_helper";
        let dir = Path::new(r"C:\tmp\anything");
        assert_eq!(manifest_file_name(host), "idm_test_helper.json");
        assert_eq!(manifest_path(dir, host), dir.join("idm_test_helper.json"));
        assert_eq!(reg_key(host), format!("{REG_ROOT}\\{host}"));
        let body = "{\n  \"path\": \"C:\\\\bin\\\\idm-nmhost.exe\",\n  \"allowed_extensions\": [\"idm-rs@local\"]\n}\n";
        assert_eq!(extract_allowed(body), DEFAULT_EXTENSION_ID);
        assert_eq!(
            extract_host_path(body),
            Some(PathBuf::from(r"C:\bin\idm-nmhost.exe"))
        );
    }
}
