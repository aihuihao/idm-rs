//! idm-rs Native Messaging host（Firefox）。
//!
//! 协议：Firefox 的 Native Messaging 帧格式 —— 每条消息前 4 字节为消息长度（本机字节序，
//! Windows 上为小端），后面是 UTF-8 JSON。host 循环读取，直到标准输入关闭。
//!
//! 输出纪律（必须）：标准输出**只写协议帧**。任何日志、panic 信息都写入数据目录下的
//! `nmhost.log`（追加，单文件超过 1 MiB 则覆盖重写）。panic hook 把 panic 写进日志，
//! 绝不写标准输出。
//!
//! 本二进制仅做**校验与入队**，不做下载；下载由独立的常驻队列运行器
//!（`idm-cli queue run --watch`）在后台执行。两者只通过 SQLite 队列通信。

use std::fs::OpenOptions;
use std::io::Write;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use idm_core::error::Error;
use idm_core::net::Url;
use idm_core::net::client::validate_extra_headers;
use idm_core::net::filename::sanitize_windows_filename;
use idm_core::store::{QueuedTask, Store, default_db_path};
use idm_ipc::frame::{ReadFrame, read_frame, write_frame};
use serde_json::{Value, json};

/// 日志文件容量上限：1 MiB。超过则在打开时覆盖重写。
const LOG_CAP: u64 = 1_048_576;
/// 进程创建标志（Windows）：脱离父进程控制台、不创建窗口。
const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 当前使用的数据库文件路径（与 `idm-cli` 解析规则一致：优先 `IDM_RS_DB` 环境变量）。
fn current_db_path() -> PathBuf {
    match std::env::var("IDM_RS_DB") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => default_db_path().unwrap_or_else(|_| PathBuf::from("idm.db")),
    }
}

/// 日志文件路径：数据目录（数据库所在目录）下的 `nmhost.log`。
fn log_path() -> &'static PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let db = current_db_path();
        let dir = db.parent().unwrap_or_else(|| Path::new("."));
        dir.join("nmhost.log")
    })
}

/// 追加一行日志到 `nmhost.log`；超过 1 MiB 则覆盖重写。绝不写标准输出。
fn host_log(msg: &str) {
    let path = log_path();
    let mut file = match OpenOptions::new().create(true).append(true).open(path) {
        Ok(f) => f,
        Err(_) => return,
    };
    if file.metadata().map(|m| m.len()).unwrap_or(0) > LOG_CAP
        && let Ok(f) = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
    {
        file = f;
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = writeln!(file, "[{secs}] {msg}");
}

/// 安装 panic hook：把 panic 写进日志文件，而不是标准输出（或 stderr 之外的任何帧外字节）。
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        host_log(&format!("panic: {info}"));
    }));
}

/// 测试钩子：仅在 `test-hooks` feature 启用时编译。值为 N 时在第 N 条消息触发 panic，
/// 用于验证「stdout 纯净」与「panic 写入 nmhost.log」。
///
/// 默认/发布构建中此函数为空操作，**不读取任何环境变量**、二进制里也不含开关字符串，
/// 因此外部无法通过环境变量诱导发布版 host 崩溃。
#[cfg(feature = "test-hooks")]
fn maybe_test_panic(seen: usize) {
    static PANIC_AT: OnceLock<Option<usize>> = OnceLock::new();
    let at = PANIC_AT.get_or_init(|| {
        std::env::var("IDM_NMHOST_TEST_PANIC")
            .ok()
            .map(|v| match v.parse::<usize>() {
                Ok(n) => n.max(1),
                Err(_) => 1,
            })
    });
    if *at == Some(seen) {
        panic!("injected panic for self-test");
    }
}

/// 默认/发布构建：空操作，无环境变量读取、无注入点。
#[cfg(not(feature = "test-hooks"))]
fn maybe_test_panic(_seen: usize) {}

/// 打开数据库（优先 `IDM_RS_DB`，否则默认路径）。
async fn open_db() -> Result<std::sync::Arc<Store>, Error> {
    match std::env::var("IDM_RS_DB") {
        Ok(p) if !p.is_empty() => Store::open(&p),
        _ => Store::open_default(),
    }
}

#[tokio::main]
async fn main() {
    install_panic_hook();
    let db = match open_db().await {
        Ok(d) => d,
        Err(e) => {
            host_log(&format!("db open failed: {e}"));
            std::process::exit(1);
        }
    };
    if let Err(e) = run_loop(&db).await {
        host_log(&format!("run_loop error: {e}"));
        std::process::exit(1);
    }
}

/// 主循环：读取帧 → 解析 → 处理 → 写回响应帧。标准输入关闭则退出。
async fn run_loop(db: &Store) -> Result<(), Error> {
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut seen = 0usize;
    loop {
        let payload = match read_frame(&mut stdin).await.map_err(|e| Error::Io {
            operation: "nmhost read_frame",
            path: std::path::PathBuf::from("<stdin>"),
            offset: 0,
            source: e,
        })? {
            ReadFrame::Message(p) => p,
            ReadFrame::Eof => break, // 标准输入关闭
            ReadFrame::TooLarge => {
                // 超长：返回错误并关闭（frame 模块已在读取负载前拒绝）。
                let _ = write_frame(
                    &mut stdout,
                    &json!({"ok": false, "error": "message too large"}),
                )
                .await;
                break;
            }
        };
        seen += 1;
        maybe_test_panic(seen);
        match handle_message(&payload, db).await {
            Ok(resp) => {
                if write_frame(&mut stdout, &resp).await.is_err() {
                    break;
                }
            }
            Err(errmsg) => {
                // 非法 JSON / 未知 type：返回错误帧并继续处理下一条，不影响后续。
                let _ = write_frame(&mut stdout, &json!({"ok": false, "error": errmsg})).await;
            }
        }
    }
    Ok(())
}

/// 解析消息并分发。
async fn handle_message(payload: &[u8], db: &Store) -> Result<Value, String> {
    let v: Value = serde_json::from_slice(payload).map_err(|e| format!("invalid json: {e}"))?;
    let typ = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match typ {
        "ping" => Ok(json!({"ok": true, "version": env!("CARGO_PKG_VERSION")})),
        "enqueue" => handle_enqueue(&v, db).await,
        other => Err(format!("unknown type: {other}")),
    }
}

/// 处理 `enqueue`：校验不可信输入，校验/规范化请求头，入队，并视情况拉起运行器。
async fn handle_enqueue(v: &Value, db: &Store) -> Result<Value, String> {
    let url = v
        .get("url")
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .to_string();
    // URL 校验：用 idm_core 重导出的 reqwest::Url 做严格解析。
    // 错误信息一律不含 URL 本身，避免 userinfo 等凭据泄入日志/回复。
    let _ = validate_url(&url)?;
    // 文件名：先取最后一个路径分隔符后的部分，再做 Windows 合法化；空则不指定。
    let file_name = match v.get("filename").and_then(|f| f.as_str()) {
        Some(f) if !f.is_empty() => {
            let base = f.rsplit(['/', '\\']).find(|s| !s.is_empty()).unwrap_or(f);
            Some(sanitize_windows_filename(base))
        }
        _ => None,
    };
    // 请求头：按白名单逐个校验，非法的直接丢弃并记录日志（不影响入队）。
    let mut headers: Vec<(String, String)> = Vec::new();
    let referrer = v.get("referrer").and_then(|r| r.as_str()).unwrap_or("");
    let cookies = v.get("cookies").and_then(|c| c.as_str()).unwrap_or("");
    let ua = v.get("userAgent").and_then(|u| u.as_str()).unwrap_or("");
    for (name, value) in [
        ("Referer", referrer),
        ("Cookie", cookies),
        ("User-Agent", ua),
    ] {
        if value.is_empty() {
            continue;
        }
        if validate_extra_headers(&[(name.to_string(), value.to_string())]).is_err() {
            host_log(&format!(
                "dropped invalid header {name} (len={})",
                value.len()
            ));
            continue;
        }
        headers.push((name.to_string(), value.to_string()));
    }
    // 下载目录：设置 `download-dir`，否则默认 `%USERPROFILE%\Downloads\idm-rs`。
    let dir = match db.get_setting("download-dir").await {
        Ok(Some(d)) => d,
        _ => default_download_dir(),
    };
    let task = QueuedTask {
        url,
        dir,
        strategy: "Auto".to_string(),
        connections: 8,
        max_speed: None,
        file_name,
    };
    match db.enqueue_task(task, Some(headers)).await {
        Ok(id) => {
            // 入队成功后尝试拉起常驻运行器（单实例）。
            let mut resp = json!({"ok": true, "taskId": id});
            match spawn_runner_if_needed(db.db_path()) {
                RunnerSpawn::Spawned | RunnerSpawn::AlreadyRunning => {}
                RunnerSpawn::NotFound => {
                    // 找不到 idm-cli.exe：入队仍然成功，在回复里附加提示。
                    resp["runner"] = json!("not_started");
                }
            }
            Ok(resp)
        }
        Err(e) => Err(format!("enqueue failed: {e}")),
    }
}

/// 校验 URL：总长 ≤ 8 KiB、不含空白字符/控制字符、scheme 只能是 http/https、
/// `host_str()` 必须非空。内网单标签主机名（如 `nas`）合法。
///
/// 使用 `idm_core::net::Url`（reqwest/url crate）而非手写字符串切分，可正确拒绝
/// `http://a b/`、`http://:80/`、`http://[::1/` 等畸形 authority。
/// 错误信息不含 URL 本身，`user:pass@` 形式的 userinfo 不会进入日志。
fn validate_url(url: &str) -> Result<Url, String> {
    if url.len() > 8 * 1024 {
        return Err("url exceeds 8 KiB".to_string());
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("url contains whitespace or control characters".to_string());
    }
    let parsed =
        Url::parse(url).map_err(|_| "url must be a valid http(s) URL with a host".to_string())?;
    match parsed.scheme() {
        "http" | "https" => {}
        _ => return Err("url scheme must be http or https".to_string()),
    }
    match parsed.host_str() {
        Some(h) if !h.is_empty() => Ok(parsed),
        _ => Err("url must have a host".to_string()),
    }
}

/// 默认下载目录：`%USERPROFILE%\Downloads\idm-rs`。
fn default_download_dir() -> String {
    match std::env::var("USERPROFILE") {
        Ok(p) => PathBuf::from(p)
            .join("Downloads")
            .join("idm-rs")
            .to_string_lossy()
            .into_owned(),
        Err(_) => PathBuf::from(".").to_string_lossy().into_owned(),
    }
}

/// 运行器拉起结果。
enum RunnerSpawn {
    /// 已成功拉起。
    Spawned,
    /// 运行器已在运行（runner.lock 被占用）。
    AlreadyRunning,
    /// 找不到 idm-cli.exe，未拉起。
    NotFound,
}

/// 入队成功后尝试拉起常驻运行器：独占打开 `runner.lock`，拿到锁说明无运行器，
/// 关闭锁后启动同目录的 `idm-cli.exe queue run --watch --db <同一数据库>`；拿不到锁说明
/// 运行器已在运行，不做任何事。
fn spawn_runner_if_needed(db_path: &Path) -> RunnerSpawn {
    let lock_dir = db_path.parent().unwrap_or_else(|| Path::new("."));
    let lock_path = lock_dir.join("runner.lock");
    // 独占打开（share_mode(0)）；成功说明当前无运行器持有锁。临时 File 在语句结束即关闭，
    // 以便随后启动的 cli 能重新独占。
    let got_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(&lock_path)
        .is_ok();
    if !got_lock {
        return RunnerSpawn::AlreadyRunning;
    }
    // 找同目录的 idm-cli.exe。
    let host_exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return RunnerSpawn::NotFound,
    };
    let cli = match host_exe.parent() {
        Some(p) => p.join("idm-cli.exe"),
        None => return RunnerSpawn::NotFound,
    };
    if !cli.exists() {
        return RunnerSpawn::NotFound;
    }
    let db_str = db_path.to_string_lossy().into_owned();
    match Command::new(&cli)
        .args(["queue", "run", "--watch", "--db", &db_str])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
        .spawn()
    {
        Ok(_) => RunnerSpawn::Spawned,
        Err(_) => RunnerSpawn::NotFound,
    }
}

#[cfg(test)]
mod url_tests {
    use super::*;

    /// 打印并断言 `validate_url` 对三个边界输入的实际返回值。
    /// 以 `--nocapture` 运行可看到原始 `Result<Url, String>` 调试输出。
    #[test]
    fn validate_url_returns_for_three_inputs() {
        let inputs = ["http://:80/", "http://[::1/", "http://user:pass@host/x"];
        for url in inputs {
            println!("validate_url({url:?}) = {:?}", validate_url(url));
        }
        assert!(validate_url("http://:80/").is_err());
        assert!(validate_url("http://[::1/").is_err());
        assert!(validate_url("http://user:pass@host/x").is_ok());
    }
}
