#![cfg(windows)]

//! idm-cli `nm install / uninstall / status` 集成测试。
//!
//! 绝不使用默认的 `idm_rs` 名：每个测试用 `idm_rs_test_<随机>` 专用名，
//! 并用 `RegGuard` 的 Drop 在测试失败/断言 panic 时也清理注册表项和清单文件。
//!
//! `--db` 指向临时目录，因此清单写在临时数据目录；host 二进制从 CLI 所在
//! 临时 bin 目录解析，测试可自行决定放不放 `idm-nmhost.exe`。

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

const REG_ROOT: &str = r"HKCU\Software\Mozilla\NativeMessagingHosts";
const EXT_ID: &str = "idm-rs@local";

fn reg_key(host: &str) -> String {
    format!("{REG_ROOT}\\{host}")
}

fn unique_host() -> TestResult<String> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    Ok(format!(
        "idm_rs_test_{}_{}_{}",
        std::process::id(),
        nanos,
        n
    ))
}

/// 无论测试成功或失败都删除注册表项与清单文件。
struct RegGuard {
    host: String,
    manifest: PathBuf,
    armed: bool,
}

impl RegGuard {
    fn new(host: String, manifest: PathBuf) -> Self {
        Self {
            host,
            manifest,
            armed: true,
        }
    }

    fn cleanup(&self) {
        let _ = Command::new("reg")
            .args(["delete", &reg_key(&self.host), "/f"])
            .output();
        let _ = std::fs::remove_file(&self.manifest);
    }
}

impl Drop for RegGuard {
    fn drop(&mut self) {
        if self.armed {
            self.cleanup();
        }
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    cli: PathBuf,
    db: PathBuf,
    host: String,
    manifest: PathBuf,
    guard: RegGuard,
}

/// 建立隔离夹具：复制 cli 到临时 bin 目录；`with_host` 决定是否放置
/// 一个名为 `idm-nmhost.exe` 的文件（内容无关，只用于存在性检查）。
fn fixture(with_host: bool) -> TestResult<Fixture> {
    let tmp = tempfile::tempdir()?;
    let bin = tmp.path().join("bin");
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&bin)?;
    std::fs::create_dir_all(&data)?;
    let cli = bin.join("idm-cli.exe");
    std::fs::copy(env!("CARGO_BIN_EXE_idm-cli"), &cli)?;
    if with_host {
        std::fs::copy(env!("CARGO_BIN_EXE_idm-cli"), bin.join("idm-nmhost.exe"))?;
    }
    let db = data.join("idm.db");
    let host = unique_host()?;
    let manifest = data.join(format!("{host}.json"));
    let guard = RegGuard::new(host.clone(), manifest.clone());
    Ok(Fixture {
        _tmp: tmp,
        cli,
        db,
        host,
        manifest,
        guard,
    })
}

fn nm(f: &Fixture, args: &[&str]) -> TestResult<Output> {
    let mut cmd = Command::new(&f.cli);
    cmd.arg("nm").args(args);
    cmd.arg("--db").arg(&f.db);
    Ok(cmd.output()?)
}

fn install(f: &Fixture) -> TestResult<Output> {
    nm(
        f,
        &["install", "--extension-id", EXT_ID, "--host-name", &f.host],
    )
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn reg_query(host: &str) -> TestResult<Output> {
    Ok(Command::new("reg")
        .args(["query", &reg_key(host), "/ve"])
        .output()?)
}

fn reg_root_children() -> TestResult<String> {
    let o = Command::new("reg").args(["query", REG_ROOT]).output()?;
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

fn manifest_json(f: &Fixture) -> TestResult<serde_json::Value> {
    let text = std::fs::read_to_string(&f.manifest)?;
    Ok(serde_json::from_str(&text)?)
}

fn is_absolute_windows(s: &str) -> bool {
    let p = Path::new(s);
    p.is_absolute()
}

// --- 3.1 install / reg query / status / uninstall；失败也清理 ----------------

#[test]
fn nm_install_registers_manifest_and_registry() -> TestResult {
    let f = fixture(true)?;
    let o = install(&f)?;
    assert!(
        o.status.success(),
        "install 应成功: out={} err={}",
        stdout(&o),
        stderr(&o)
    );

    // reg query 能查到，默认值是清单的绝对路径。
    let q = reg_query(&f.host)?;
    assert!(q.status.success(), "reg query 应查到注册表项");
    let qout = String::from_utf8_lossy(&q.stdout).into_owned();
    assert!(
        qout.contains(&f.manifest.to_string_lossy().into_owned()),
        "注册表默认值应指向清单文件: {qout}"
    );

    // status 正确。
    let s = nm(&f, &["status", "--host-name", &f.host])?;
    assert!(s.status.success(), "status 应成功");
    let sout = stdout(&s);
    assert!(sout.contains("manifest_exists: true"), "{sout}");
    assert!(sout.contains("host_path_exists: true"), "{sout}");
    assert!(sout.contains("registry_exists: true"), "{sout}");
    assert!(
        sout.contains(&format!("allowed_extensions: {EXT_ID}")),
        "{sout}"
    );
    assert!(
        sout.contains(&f.manifest.to_string_lossy().into_owned()),
        "{sout}"
    );

    // uninstall 后注册表项和清单文件都不存在。
    let u = nm(&f, &["uninstall", "--host-name", &f.host])?;
    assert!(u.status.success(), "uninstall 应成功: {}", stderr(&u));
    assert!(!f.manifest.exists(), "清单文件应被删除");
    assert!(!reg_query(&f.host)?.status.success(), "注册表项应被删除");
    f.guard.cleanup();
    Ok(())
}

// --- 3.3 清单 JSON 字段逐项断言 ---------------------------------------------

#[test]
fn nm_manifest_json_fields_are_exact() -> TestResult {
    let f = fixture(true)?;
    assert!(install(&f)?.status.success());
    let v = manifest_json(&f)?;
    let obj = v.as_object().ok_or("manifest 必须是 JSON 对象")?;

    assert_eq!(obj.len(), 5, "清单必须恰好 5 个字段: {obj:?}");
    assert_eq!(
        obj.get("name").and_then(|x| x.as_str()),
        Some(f.host.as_str())
    );
    assert!(
        obj.get("description")
            .and_then(|x| x.as_str())
            .is_some_and(|s| !s.is_empty()),
        "description 必须是非空字符串"
    );
    assert_eq!(obj.get("type").and_then(|x| x.as_str()), Some("stdio"));
    assert!(
        obj.get("allowed_origins").is_none(),
        "不得有 allowed_origins"
    );

    let allowed = obj
        .get("allowed_extensions")
        .ok_or("缺少 allowed_extensions")?;
    let arr = allowed.as_array().ok_or("allowed_extensions 必须是数组")?;
    assert_eq!(arr.len(), 1, "allowed_extensions 必须恰好 1 项");
    assert_eq!(arr[0].as_str(), Some(EXT_ID));

    let path = obj
        .get("path")
        .and_then(|x| x.as_str())
        .ok_or("缺少 path")?;
    assert!(is_absolute_windows(path), "path 必须是绝对路径: {path}");
    assert!(Path::new(path).is_file(), "path 必须指向存在的文件: {path}");
    assert!(
        path.to_ascii_lowercase().ends_with("idm-nmhost.exe"),
        "path 必须指向 idm-nmhost.exe: {path}"
    );
    Ok(())
}

// --- 3.2 非法输入：都返回错误且绝不写注册表 ---------------------------------

#[test]
fn nm_rejects_invalid_extension_id() -> TestResult {
    let f = fixture(true)?;
    let bad = [
        "",
        "has space",
        "has/slash",
        "has:colon",
        "has\nnewline",
        "has\"quote",
        &"x".repeat(129),
    ];
    for id in bad {
        let o = nm(
            &f,
            &["install", "--extension-id", id, "--host-name", &f.host],
        )?;
        assert!(
            !o.status.success(),
            "非法扩展 ID {id:?} 应返回错误: out={} err={}",
            stdout(&o),
            stderr(&o)
        );
        assert!(!f.manifest.exists(), "非法 ID 不得写清单: {id:?}");
        assert!(
            !reg_query(&f.host)?.status.success(),
            "非法 ID 不得写注册表: {id:?}"
        );
    }
    Ok(())
}

#[test]
fn nm_rejects_invalid_host_name() -> TestResult {
    let f = fixture(true)?;
    let bad = [
        "",
        "Uppercase",
        "has-hyphen",
        ".leadingdot",
        "trailingdot.",
        "double..dot",
        "has space",
    ];
    for name in bad {
        let o = nm(
            &f,
            &["install", "--extension-id", EXT_ID, "--host-name", name],
        )?;
        assert!(
            !o.status.success(),
            "非法 host 名 {name:?} 应返回错误: out={} err={}",
            stdout(&o),
            stderr(&o)
        );
    }
    // 注册表根下不得出现任何测试专用名（非法名也没被写入）。
    let root = reg_root_children()?;
    assert!(
        !root.contains(&f.host),
        "不得写入任何测试 host 注册表项: {root}"
    );
    Ok(())
}

#[test]
fn nm_rejects_missing_host_exe() -> TestResult {
    let f = fixture(false)?;
    let o = install(&f)?;
    assert!(
        !o.status.success(),
        "idm-nmhost.exe 不存在应报错: out={} err={}",
        stdout(&o),
        stderr(&o)
    );
    assert!(
        stderr(&o).contains("idm-nmhost.exe not found"),
        "错误原因应说明 host 不存在: {}",
        stderr(&o)
    );
    assert!(!f.manifest.exists(), "host 不存在时不得写清单");
    assert!(
        !reg_query(&f.host)?.status.success(),
        "host 不存在时不得写注册表"
    );
    Ok(())
}

// --- 一.1 幂等 ---------------------------------------------------------------

#[test]
fn nm_install_is_idempotent() -> TestResult {
    let f = fixture(true)?;
    assert!(install(&f)?.status.success(), "第一次 install 应成功");
    let first = std::fs::read_to_string(&f.manifest)?;
    let q1 = String::from_utf8_lossy(&reg_query(&f.host)?.stdout).into_owned();

    assert!(
        install(&f)?.status.success(),
        "第二次 install 应成功（覆盖）"
    );
    let second = std::fs::read_to_string(&f.manifest)?;
    let q2 = String::from_utf8_lossy(&reg_query(&f.host)?.stdout).into_owned();
    assert_eq!(first, second, "重复 install 清单内容应一致");
    assert_eq!(q1, q2, "重复 install 注册表默认值应一致");

    let v = manifest_json(&f)?;
    assert_eq!(
        v["allowed_extensions"].as_array().map(|a| a.len()),
        Some(1),
        "重复 install 不得累加 allowed_extensions"
    );
    Ok(())
}

// --- 一.3 status 在 uninstall 之后 -------------------------------------------

#[test]
fn nm_status_after_uninstall_reports_absent() -> TestResult {
    let f = fixture(true)?;
    assert!(install(&f)?.status.success());
    assert!(
        nm(&f, &["uninstall", "--host-name", &f.host])?
            .status
            .success()
    );
    let s = nm(&f, &["status", "--host-name", &f.host])?;
    assert!(s.status.success(), "status 本身应成功返回");
    let sout = stdout(&s);
    assert!(sout.contains("manifest_exists: false"), "{sout}");
    assert!(sout.contains("registry_exists: false"), "{sout}");
    assert!(
        sout.contains("allowed_extensions:") && !sout.contains(EXT_ID),
        "{sout}"
    );
    Ok(())
}
