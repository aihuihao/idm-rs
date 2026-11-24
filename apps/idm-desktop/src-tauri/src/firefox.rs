//! Firefox 安装与扩展状态检测（只读）。
//!
//! - `parse_profiles_ini` 是纯函数：接收 profiles.ini 内容与基础目录，返回配置文件夹路径。
//! - `find_firefox_with` 把「读注册表」与「检查文件存在」作为可注入参数，便于测试各分支。
//! - 生产入口 `find_firefox` / `detect_extension` 只用 `reg.exe` 子进程与标准库，不新增依赖。

use std::path::{Path, PathBuf};

/// 扩展在 Firefox 配置文件夹内的相对路径（Mozilla 对自行分发扩展的存放约定）。
const EXTENSION_REL: &str = "extensions/idm-rs@local.xpi";

/// HKLM 与 HKCU 下 Firefox App Paths 注册表键（默认值即 firefox.exe 路径）。
const FIREFOX_APP_PATHS: &str = r"Software\Microsoft\Windows\CurrentVersion\App Paths\firefox.exe";

/// 扩展检测状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionStatus {
    /// 已安装，列出含该扩展的配置名。
    Installed(Vec<String>),
    /// profiles.ini 可读，但没有任何配置安装该扩展。
    NotInstalled,
    /// 无法判断（如 profiles.ini 不存在或读取失败）。
    Unknown,
}

/// 解析 profiles.ini 内容，返回各配置的文件夹路径（相对路径基于 `base`）。
/// 只识别含 `Path=` 的条目；格式异常的行被忽略，绝不 panic。
pub fn parse_profiles_ini(content: &str, base: &Path) -> Vec<PathBuf> {
    parse_profiles(content, base)
        .into_iter()
        .map(|(_name, path)| path)
        .collect()
}

/// 解析 profiles.ini，保留配置名与路径。
fn parse_profiles(content: &str, base: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let mut name = String::new();
    let mut is_relative = true;
    let mut path: Option<String> = None;

    let flush = |out: &mut Vec<(String, PathBuf)>,
                 name: &String,
                 is_relative: bool,
                 path: &Option<String>| {
        if let Some(p) = path {
            if p.is_empty() {
                return;
            }
            let pv = Path::new(p);
            let folder = if is_relative {
                base.join(pv)
            } else {
                pv.to_path_buf()
            };
            out.push((name.clone(), folder));
        }
    };

    for raw in content.lines() {
        let line = raw.trim();
        if line.starts_with('[') && line.ends_with(']') && line.len() > 2 {
            // 新 section：先落盘上一条。
            flush(&mut out, &name, is_relative, &path);
            name.clear();
            is_relative = true;
            path = None;
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            let v = v.trim().trim_matches('"');
            match k {
                "Name" => name = v.to_string(),
                "IsRelative" => is_relative = v != "0",
                "Path" => path = Some(v.to_string()),
                _ => {}
            }
        }
    }
    flush(&mut out, &name, is_relative, &path);
    out
}

/// 检测扩展是否已安装。`ini_path` 通常为 `%APPDATA%\Mozilla\Firefox\profiles.ini`。
/// 只读，不写任何 Firefox 文件。
pub fn detect_extension(ini_path: &Path) -> std::io::Result<ExtensionStatus> {
    let content = match std::fs::read_to_string(ini_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ExtensionStatus::Unknown),
        Err(e) => return Err(e),
    };
    let base = ini_path
        .parent()
        .ok_or_else(|| std::io::Error::other("profiles.ini has no parent"))?;
    let installed: Vec<String> = parse_profiles(&content, base)
        .into_iter()
        .filter(|(_name, dir)| dir.join(EXTENSION_REL).is_file())
        .map(|(name, _dir)| name)
        .collect();
    if installed.is_empty() {
        Ok(ExtensionStatus::NotInstalled)
    } else {
        Ok(ExtensionStatus::Installed(installed))
    }
}

/// 用可注入的「读注册表默认值」与「检查文件存在」查找 firefox.exe。
/// `read_reg(key)` 返回给定键的默认值路径（注册表无该键时返回 None）。
pub fn find_firefox_with<F, E>(read_reg: F, exists: E) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<PathBuf>,
    E: Fn(&Path) -> bool,
{
    // 1) 注册表 HKLM / HKCU 的 App Paths（存在即视为安装）。
    for root in ["HKLM", "HKCU"] {
        let key = format!("{root}\\{FIREFOX_APP_PATHS}");
        if let Some(path) = read_reg(&key) {
            return Some(path);
        }
    }
    // 2) 文件系统回退。
    for dir in ["ProgramFiles", "ProgramFiles(x86)"] {
        let root = std::env::var(dir).unwrap_or_default();
        if root.is_empty() {
            continue;
        }
        let candidate = Path::new(&root).join("Mozilla Firefox/firefox.exe");
        if exists(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// 生产入口：用 `reg.exe` 读注册表默认值 + 文件存在检查查找 Firefox。
pub fn find_firefox() -> Option<PathBuf> {
    find_firefox_with(read_reg_default, |p| p.is_file())
}

/// 用 `reg query <key> /ve` 读取注册表键的默认值；不引入依赖。
fn read_reg_default(key: &str) -> Option<PathBuf> {
    let out = std::process::Command::new("reg")
        .args(["query", key, "/ve"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        if let Some(pos) = line.find("REG_SZ") {
            let value = line[pos + "REG_SZ".len()..].trim();
            if !value.is_empty() {
                return Some(PathBuf::from(value));
            }
        }
    }
    None
}

/// profiles.ini 的完整路径（%APPDATA%\Mozilla\Firefox\profiles.ini）。
pub fn profiles_ini_path() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(Path::new(&appdata).join("Mozilla/Firefox/profiles.ini"))
}
