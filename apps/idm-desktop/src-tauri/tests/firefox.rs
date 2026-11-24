#![cfg(windows)]

//! v0.2.0 步骤 2/5：Firefox 检测纯函数与可注入查找测试。

use std::path::{Path, PathBuf};

use idm_desktop::firefox::{
    ExtensionStatus, detect_extension, find_firefox_with, parse_profiles_ini,
};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

/// 不引入依赖的临时目录：随机名，Drop 时递归删除。
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TestResult<Self> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let p = std::env::temp_dir().join(format!("idmrs-ff-{tag}-{nonce}"));
        std::fs::create_dir_all(&p)?;
        Ok(Self(p))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// --- profiles.ini 解析 ------------------------------------------------------

#[test]
fn parse_profiles_relative_and_absolute() -> TestResult {
    let content = "[General]\nStartWithLastProfile=1\n\n\
                   [Profile0]\nName=default-release\nIsRelative=1\nPath=Profiles/abc.default-release\nDefault=1\n\n\
                   [Profile1]\nName=abs\nIsRelative=0\nPath=C:\\Users\\me\\AppData\\Roaming\\Mozilla\\Firefox\\Profiles\\xyz.dev\n";
    let base = Path::new("C:/Users/me/AppData/Roaming/Mozilla/Firefox");
    let dirs = parse_profiles_ini(content, base);
    let expected = vec![
        base.join("Profiles/abc.default-release"),
        PathBuf::from("C:\\Users\\me\\AppData\\Roaming\\Mozilla\\Firefox\\Profiles\\xyz.dev"),
    ];
    if dirs != expected {
        return Err(format!("解析结果不符: got {dirs:?}, want {expected:?}").into());
    }
    Ok(())
}

#[test]
fn parse_profiles_multiple_entries() -> TestResult {
    let content = "[Profile0]\nName=a\nIsRelative=1\nPath=Profiles/a\n\
                   [Profile1]\nName=b\nIsRelative=1\nPath=Profiles/b\n\
                   [Profile2]\nName=c\nIsRelative=1\nPath=Profiles/c\n";
    let base = Path::new("C:/base");
    let dirs = parse_profiles_ini(content, base);
    if dirs.len() != 3 {
        return Err(format!("应有 3 个配置, got {}: {dirs:?}", dirs.len()).into());
    }
    Ok(())
}

#[test]
fn parse_profiles_empty_and_malformed() -> TestResult {
    if !parse_profiles_ini("", Path::new("C:/base")).is_empty() {
        return Err("空文件应返回空列表".into());
    }
    let malformed = "[Profile0]\nName=only-name\nPathWithoutEqualSign\n\
                     [Broken\nName=x\nPath=Profiles/x\n";
    // 只有含合法 Path= 的条目应被解析；坏行不应 panic。
    let dirs = parse_profiles_ini(malformed, Path::new("C:/base"));
    if dirs != vec![PathBuf::from("C:/base/Profiles/x")] {
        return Err(format!("畸形文件解析不符: {dirs:?}").into());
    }
    Ok(())
}

// --- 扩展检测 ---------------------------------------------------------------

#[test]
fn detect_extension_installed_and_absent() -> TestResult {
    let tmp = TempDir::new("detect")?;
    let base = tmp.path();
    let p1 = base.join("Profiles/one");
    let p2 = base.join("Profiles/two");
    std::fs::create_dir_all(p1.join("extensions"))?;
    std::fs::create_dir_all(p2.join("extensions"))?;
    std::fs::write(p1.join("extensions/idm-rs@local.xpi"), b"xpi")?;
    let content = "[Profile0]\nName=one\nIsRelative=1\nPath=Profiles/one\n\
                   [Profile1]\nName=two\nIsRelative=1\nPath=Profiles/two\n";
    let ini = base.join("profiles.ini");
    std::fs::write(&ini, content)?;

    match detect_extension(&ini)? {
        ExtensionStatus::Installed(names) => {
            if names != vec!["one".to_string()] {
                return Err(format!("已安装配置名不符: {names:?}").into());
            }
        }
        other => return Err(format!("应为 Installed, got {other:?}").into()),
    }

    std::fs::remove_file(p1.join("extensions/idm-rs@local.xpi"))?;
    if !matches!(detect_extension(&ini)?, ExtensionStatus::NotInstalled) {
        return Err("删除 xpi 后应为 NotInstalled".into());
    }
    Ok(())
}

#[test]
fn detect_extension_missing_ini_is_unknown() -> TestResult {
    let tmp = TempDir::new("missing")?;
    let ini = tmp.path().join("no-such-profiles.ini");
    if !matches!(detect_extension(&ini)?, ExtensionStatus::Unknown) {
        return Err("profiles.ini 不存在应为 Unknown".into());
    }
    Ok(())
}

// --- Firefox 路径查找（可注入） ---------------------------------------------

#[test]
fn find_firefox_registry_hit() -> TestResult {
    let keys = std::cell::RefCell::new(Vec::<String>::new());
    let reg = |key: &str| {
        keys.borrow_mut().push(key.to_string());
        Some(PathBuf::from(
            "C:/Program Files/Mozilla Firefox/firefox.exe",
        ))
    };
    let exists = |_p: &Path| false;
    let found = find_firefox_with(reg, exists);
    if found
        != Some(PathBuf::from(
            "C:/Program Files/Mozilla Firefox/firefox.exe",
        ))
    {
        return Err(format!("注册表命中应返回该路径: {found:?}").into());
    }
    let asked = keys.borrow();
    let want = r"HKLM\Software\Microsoft\Windows\CurrentVersion\App Paths\firefox.exe";
    if !asked.iter().any(|k| k == want) {
        return Err(format!("应先查 HKLM 的 App Paths 键, got {asked:?}").into());
    }
    Ok(())
}

#[test]
fn find_firefox_file_fallback() -> TestResult {
    let reg = |_key: &str| None::<PathBuf>;
    let exists = |p: &Path| {
        let s = p.to_string_lossy().replace('\\', "/");
        s.contains("Program Files/Mozilla Firefox")
    };
    let found = find_firefox_with(reg, exists);
    if found
        != Some(PathBuf::from(
            "C:/Program Files/Mozilla Firefox/firefox.exe",
        ))
    {
        return Err(format!("注册表无但文件存在应回退: {found:?}").into());
    }
    Ok(())
}

#[test]
fn find_firefox_not_installed() -> TestResult {
    let reg = |_key: &str| None::<PathBuf>;
    let exists = |_p: &Path| false;
    if find_firefox_with(reg, exists).is_some() {
        return Err("都没有时应返回 None".into());
    }
    Ok(())
}
