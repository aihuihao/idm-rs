//! idm-rs Native Messaging host（Firefox）。
//!
//! 协议：Firefox 的 Native Messaging 帧格式 —— 每条消息前 4 字节为消息长度（本机字节序，
//! Windows 上为小端），后面是 UTF-8 JSON。host 循环读取，直到标准输入关闭。
//!
//! 输出纪律（必须）：标准输出**只写协议帧**。任何日志、panic 信息都写入数据目录下的
//! `nmhost.log`（追加，单文件超过 1 MiB 则覆盖重写）。panic hook 把 panic 写进日志，
//! 绝不写标准输出。
//!
//! 本二进制仅做**校验与入队**，不启动任何进程、不做下载；下载由独立的常驻
//! 队列运行器（`idm-cli queue run --watch`）执行。两者只通过 SQLite 队列与
//! `runner.lock` 通信：运行器未运行时拒绝入队，浏览器保留自己的下载。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use idm_core::error::Error;
use idm_core::net::client::validate_extra_headers;
use idm_core::net::filename::{sanitize_windows_filename, strip_browser_dup_suffix};
use idm_core::net::validate_url;
use idm_core::store::{QueuedTask, Store, default_db_path, default_download_dir};
use idm_ipc::frame::{ReadFrame, read_frame_body_timeout, write_frame};
use serde_json::{Value, json};

/// 日志文件容量上限：1 MiB。超过则在打开时覆盖重写。
const LOG_CAP: u64 = 1_048_576;

/// 消息体读取超时：长度前缀读到之后，负载必须在此时间内读完，否则报错退出。
/// 等待下一条消息前缀的空闲阶段不设超时。
const BODY_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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
        let payload = match read_frame_body_timeout(&mut stdin, BODY_READ_TIMEOUT)
            .await
            .map_err(|e| Error::Io {
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

/// 处理 `enqueue`：校验不可信输入，检查运行器，URL 去重，校验/规范化请求头，入队。
///
/// 日志纪律：每次 enqueue 写一行到 `nmhost.log`，只含“结果 / task id / 运行器状态 /
/// 是否重复”，绝不写 URL 与请求头的值。
async fn handle_enqueue(v: &Value, db: &Store) -> Result<Value, String> {
    // 先查运行器：未运行直接拒绝，绝不入队（浏览器保留自己的下载）。
    let running = runner_is_running(db.db_path());
    if !running {
        host_log(
            "enqueue result=rejected reason=runner_not_running task=- duplicate=false runner=stopped",
        );
        return Err("runner_not_running".to_string());
    }

    let url = v
        .get("url")
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .to_string();
    // URL 校验：用 idm_core 重导出的 reqwest::Url 做严格解析。
    // 错误信息一律不含 URL 本身，避免 userinfo 等凭据泄入日志/回复。
    let _ = validate_url(&url)?;

    // 同一 URL 已有活动任务（Queued/Probing/Downloading）时不重复入队。
    match db.find_active_by_url(&url).await {
        Ok(Some(existing)) => {
            host_log(&format!(
                "enqueue result=duplicate task={existing} duplicate=true runner=running"
            ));
            return Ok(json!({"ok": true, "taskId": existing, "duplicate": true}));
        }
        Err(e) => {
            host_log("enqueue result=error reason=dedup_failed runner=running");
            return Err(format!("dedup failed: {e}"));
        }
        Ok(None) => {}
    }

    // 文件名：取路径最后一段 → 去掉浏览器重复下载编号 → Windows 合法化；空则不指定。
    let file_name = match v.get("filename").and_then(|f| f.as_str()) {
        Some(f) if !f.is_empty() => {
            let base = f.rsplit(['/', '\\']).find(|s| !s.is_empty()).unwrap_or(f);
            Some(sanitize_windows_filename(&strip_browser_dup_suffix(base)))
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
            host_log(&format!(
                "enqueue result=accepted task={id} duplicate=false runner=running"
            ));
            Ok(json!({"ok": true, "taskId": id}))
        }
        Err(e) => {
            host_log(
                "enqueue result=error reason=enqueue_failed task=- duplicate=false runner=running",
            );
            Err(format!("enqueue failed: {e}"))
        }
    }
}

/// 判断常驻运行器是否在运行：以**只读、不创建**的方式打开 `runner.lock`。
///
/// - 文件不存在，或能以只读方式打开（打开后立即关闭）→ 未运行；
/// - 共享冲突（Windows 错误码 32，`ERROR_SHARING_VIOLATION`）→ 运行器正独占持有锁 → 运行中；
/// - 其他 IO 错误一律按“未运行”处理（宁可拒绝入队，也不误判为运行中而静默丢任务）。
fn runner_is_running(db_path: &Path) -> bool {
    let lock_dir = db_path.parent().unwrap_or_else(|| Path::new("."));
    let lock_path = lock_dir.join("runner.lock");
    match OpenOptions::new().read(true).open(&lock_path) {
        Ok(f) => {
            drop(f);
            false
        }
        Err(e) => e.raw_os_error() == Some(32),
    }
}
