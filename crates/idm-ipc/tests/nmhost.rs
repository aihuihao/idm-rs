#![cfg(windows)]

//! idm-nmhost（Firefox Native Messaging host）集成测试。
//!
//! 通过真实子进程 + 真实帧协议与 `idm-nmhost` 通信，覆盖：
//!   2. ping 往返
//!   3. enqueue 创建 Queued 任务（URL/目录/请求头正确）
//!   4. 拒绝非法输入（scheme/host/空白/控制字符/畸形 authority/超长 URL/超大消息/
//!      非法 JSON/未知 type/含换行请求头）
//!   5. 标准输出只写帧；panic 写入 nmhost.log（测试开关）
//!   6. 文件名 sanitize（对具体结果值断言）
//!   14. 运行器未运行时拒绝入队；持锁模拟运行中时成功；同 URL 去重
//!
//! 说明：所有辅助函数一律返回 `TestResult` 并用 `?` 传播错误，不使用
//! `unwrap()`/`expect()`，以便通过 `clippy -D warnings`（`clippy.toml` 的
//! `allow-unwrap-in-tests` 只豁免测试函数体，不豁免辅助函数）。

use std::io::{Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use idm_core::store::{Store, TaskRecord};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

fn io_missing(what: &str) -> std::io::Error {
    std::io::Error::other(what)
}

/// 全局持有各测试创建的 `runner.lock` 独占句柄，模拟“运行器在运行”。
/// host 现在以只读方式探测锁：只要锁被独占持有，enqueue 就会被接受。
static HELD_RUNNER_LOCKS: OnceLock<Mutex<Vec<std::fs::File>>> = OnceLock::new();

/// 在 `db_path` 所在目录独占持有 `runner.lock`，并保持句柄直到进程退出。
fn hold_runner_lock(db_path: &Path) -> TestResult {
    let dir = db_path
        .parent()
        .ok_or_else(|| io_missing("db path has no parent"))?;
    let lock = dir.join("runner.lock");
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(lock)?;
    let store = HELD_RUNNER_LOCKS.get_or_init(|| Mutex::new(Vec::new()));
    let mut guard = store.lock().map_err(|_| io_missing("lock poisoned"))?;
    guard.push(f);
    Ok(())
}

/// 启动 nmhost 子进程，数据库指向 `db_path`；可选注入测试开关。
///
/// 默认在数据库目录模拟持锁的运行器，使 enqueue 成功（Phase 4c 起 host 未运行
/// 时拒绝入队）。host 复制到数据库目录下的干净子目录，确保不启动任何子进程。
fn spawn_nmhost(db_path: &Path, extra_env: &[(&str, &str)]) -> TestResult<Child> {
    spawn_nmhost_inner(db_path, extra_env, true)
}

/// 不模拟运行器（runner.lock 不存在）地启动 host，用于验证拒绝入队。
fn spawn_nmhost_no_lock(db_path: &Path) -> TestResult<Child> {
    spawn_nmhost_inner(db_path, &[], false)
}

fn spawn_nmhost_inner(
    db_path: &Path,
    extra_env: &[(&str, &str)],
    hold_lock: bool,
) -> TestResult<Child> {
    if hold_lock {
        hold_runner_lock(db_path)?;
    }
    let host_dir = db_path
        .parent()
        .ok_or_else(|| io_missing("db path has no parent"))?
        .join("hostbin");
    std::fs::create_dir_all(&host_dir)?;
    let host_exe = host_dir.join("idm-nmhost.exe");
    std::fs::copy(env!("CARGO_BIN_EXE_idm-nmhost"), &host_exe)?;
    let mut cmd = Command::new(&host_exe);
    cmd.env("IDM_RS_DB", db_path)
        .env_remove("IDM_NMHOST_TEST_PANIC")
        .current_dir(&host_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    Ok(cmd.spawn()?)
}

/// 写一条帧：4 字节本机字节序长度 + UTF-8 JSON。
fn send_frame(child: &mut Child, msg: &serde_json::Value) -> TestResult {
    let bytes = serde_json::to_vec(msg)?;
    let len = bytes.len() as u32;
    let stdin = child
        .stdin
        .as_mut()
        .ok_or_else(|| io_missing("child stdin missing"))?;
    stdin.write_all(&len.to_ne_bytes())?;
    stdin.write_all(&bytes)?;
    stdin.flush()?;
    Ok(())
}

/// 读一条帧；对端关闭返回 `Ok(None)`。
fn read_frame(child: &mut Child) -> TestResult<Option<serde_json::Value>> {
    let stdout = child
        .stdout
        .as_mut()
        .ok_or_else(|| io_missing("child stdout missing"))?;
    let mut lenbuf = [0u8; 4];
    if stdout.read_exact(&mut lenbuf).is_err() {
        return Ok(None);
    }
    let len = u32::from_ne_bytes(lenbuf) as usize;
    if len > 64 * 1024 {
        return Ok(None); // 超长：与 host 一致，直接视为无可用响应
    }
    let mut payload = vec![0u8; len];
    if stdout.read_exact(&mut payload).is_err() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_slice(&payload)?))
}

/// 读一条帧，缺失时报错。
fn expect_frame(child: &mut Child, what: &str) -> TestResult<serde_json::Value> {
    match read_frame(child)? {
        Some(v) => Ok(v),
        None => Err(io_missing(what).into()),
    }
}

/// 读所有剩余 stdout 字节。
fn drain_stdout(child: &mut Child) -> TestResult<Vec<u8>> {
    let mut buf = Vec::new();
    let stdout = child
        .stdout
        .as_mut()
        .ok_or_else(|| io_missing("child stdout missing"))?;
    stdout.read_to_end(&mut buf)?;
    Ok(buf)
}

/// 读所有剩余 stderr 字节。
fn drain_stderr(child: &mut Child) -> TestResult<Vec<u8>> {
    let mut buf = Vec::new();
    let stderr = child
        .stderr
        .as_mut()
        .ok_or_else(|| io_missing("child stderr missing"))?;
    stderr.read_to_end(&mut buf)?;
    Ok(buf)
}

fn query_queued(db_path: &Path) -> TestResult<Vec<TaskRecord>> {
    let store = Store::open(db_path)?;
    let rt = tokio::runtime::Runtime::new()?;
    Ok(rt.block_on(store.list_queued_tasks())?)
}

fn query_headers(db_path: &Path, id: i64) -> TestResult<Vec<(String, String)>> {
    let store = Store::open(db_path)?;
    let rt = tokio::runtime::Runtime::new()?;
    Ok(rt.block_on(store.get_task_headers(id))?)
}

fn task_by_url<'a>(tasks: &'a [TaskRecord], url: &str) -> Option<&'a TaskRecord> {
    tasks.iter().find(|t| t.url == url)
}

fn log_path(tmp: &Path) -> PathBuf {
    tmp.join("nmhost.log")
}

// --- 2. ping 往返 -----------------------------------------------------------

#[test]
fn host_ping_roundtrip() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let db = tmp.path().join("nm.db");
    let mut child = spawn_nmhost(&db, &[])?;
    send_frame(&mut child, &serde_json::json!({"type": "ping"}))?;
    let resp = expect_frame(&mut child, "ping response")?;
    assert_eq!(resp["ok"], serde_json::json!(true), "ping 应返回 ok:true");
    assert!(
        resp["version"].is_string(),
        "ping 应返回 version（crate 版本）"
    );
    child.kill()?;
    Ok(())
}

// --- 3b. 半截消息体读取超时 -------------------------------------------------

/// 只写 4 字节长度前缀（声明 100 字节）而不写消息体，host 应在数秒内退出且退出码非 0。
#[test]
fn host_times_out_on_partial_frame() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let db = tmp.path().join("nm.db");
    let mut child = spawn_nmhost_no_lock(&db)?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| io_missing("child stdin missing"))?;
        stdin.write_all(&100u32.to_ne_bytes())?;
        stdin.flush()?;
    }

    let began = std::time::Instant::now();
    let exited = loop {
        match child.try_wait()? {
            Some(status) => break Some(status),
            None if began.elapsed() >= Duration::from_secs(8) => break None,
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    let elapsed = began.elapsed();
    let status = exited.ok_or_else(|| {
        let _ = child.kill();
        io_missing(&format!(
            "host 应在 4-8 秒内因半截消息体退出，但 {elapsed:?} 后仍在运行"
        ))
    })?;
    if elapsed < Duration::from_secs(4) || elapsed > Duration::from_secs(8) {
        return Err(format!("退出耗时应在 4-8 秒，实际 {elapsed:?}").into());
    }
    if status.success() {
        return Err("半截消息体应以非 0 退出码退出".into());
    }
    let log = std::fs::read_to_string(log_path(tmp.path())).unwrap_or_default();
    if !log.contains("timeout") {
        return Err(format!("nmhost.log 应含 timeout 记录，实际: {log}").into());
    }
    Ok(())
}

// --- 3. enqueue 创建 Queued 任务 -------------------------------------------

#[test]
fn host_enqueue_creates_queued_task() -> TestResult {
    let tmp = tempfile::tempdir()?;
    // 固定 USERPROFILE，使默认下载目录可预测。
    let db = tmp.path().join("nm.db");
    let mut child = spawn_nmhost(&db, &[("USERPROFILE", tmp.path().to_str().unwrap_or(""))])?;
    let url = "http://example.com/path/file.bin";
    send_frame(
        &mut child,
        &serde_json::json!({
            "type": "enqueue",
            "url": url,
            "referrer": "http://example.com/page",
            "cookies": "session=abc123",
            "userAgent": "idm-rs-test/1.0"
        }),
    )?;
    let resp = expect_frame(&mut child, "enqueue response")?;
    assert_eq!(
        resp["ok"],
        serde_json::json!(true),
        "enqueue 应成功: {resp}"
    );
    let id = resp["taskId"]
        .as_i64()
        .ok_or_else(|| io_missing("missing taskId"))?;

    let tasks = query_queued(&db)?;
    let rec = task_by_url(&tasks, url).ok_or_else(|| io_missing("task not in db"))?;
    assert_eq!(rec.dir, tmp.path().join("Downloads").join("idm-rs"));
    let headers = query_headers(&db, id)?;
    let by_name = |n: &str| headers.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
    assert_eq!(
        by_name("Referer").as_deref(),
        Some("http://example.com/page")
    );
    assert_eq!(by_name("Cookie").as_deref(), Some("session=abc123"));
    assert_eq!(by_name("User-Agent").as_deref(), Some("idm-rs-test/1.0"));
    child.kill()?;
    Ok(())
}

// --- 4. 拒绝非法输入 -------------------------------------------------------

#[test]
fn host_rejects_bad_input() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let db = tmp.path().join("nm.db");
    let mut child = spawn_nmhost(&db, &[])?;

    // 每种「会导致整条消息失败」的非法输入：返回错误且数据库无新任务，之后仍能 ping。
    let bad_messages: Vec<serde_json::Value> = vec![
        serde_json::json!({"type": "enqueue", "url": "file:///C:/Windows/notepad.exe"}),
        serde_json::json!({"type": "enqueue", "url": "javascript:alert(1)"}),
        serde_json::json!({"type": "enqueue", "url": "ftp://example.com/x"}),
        serde_json::json!({"type": "enqueue", "url": "http://"}), // 无 host
        serde_json::json!({"type": "enqueue", "url": format!("http://x.com/{}", "a".repeat(9000))}), // 超长 URL
        serde_json::json!({"type": "frobnicate"}), // 未知 type
        serde_json::json!({"not": "a valid message"}), // 缺 type
    ];

    for (i, msg) in bad_messages.iter().enumerate() {
        let before = query_queued(&db)?.len();
        send_frame(&mut child, msg)?;
        let resp = expect_frame(&mut child, "应有错误响应帧")?;
        assert_eq!(
            resp["ok"],
            serde_json::json!(false),
            "非法输入 #{i} 应返回 ok:false: {resp}"
        );
        let after = query_queued(&db)?.len();
        assert_eq!(after, before, "非法输入 #{i} 不得创建新任务");
        // host 仍存活：ping 应得到 pong。
        send_frame(&mut child, &serde_json::json!({"type": "ping"}))?;
        let pong = expect_frame(&mut child, "host 应在拒绝后继续存活")?;
        assert_eq!(
            pong["ok"],
            serde_json::json!(true),
            "host 应在拒绝后继续存活 (case #{i})"
        );
    }

    // 含换行的请求头：整条入队仍成功（非法头被丢弃），但该头不进数据库。
    let before = query_queued(&db)?.len();
    send_frame(
        &mut child,
        &serde_json::json!({
            "type": "enqueue",
            "url": "http://example.com/sanitized-header.bin",
            "cookies": "a\r\nb"
        }),
    )?;
    let resp = expect_frame(&mut child, "enqueue 响应")?;
    assert_eq!(
        resp["ok"],
        serde_json::json!(true),
        "含换行头的任务仍应入队: {resp}"
    );
    let after = query_queued(&db)?.len();
    assert_eq!(after, before + 1, "含换行头的任务应入队（头被丢弃）");
    let tasks = query_queued(&db)?;
    let rec = task_by_url(&tasks, "http://example.com/sanitized-header.bin")
        .ok_or_else(|| io_missing("sanitized-header task missing"))?;
    let id = rec.id;
    let headers = query_headers(&db, id)?;
    assert!(
        !headers.iter().any(|(k, _)| k == "Cookie"),
        "含换行的 Cookie 不得入库"
    );
    // host 仍存活
    send_frame(&mut child, &serde_json::json!({"type": "ping"}))?;
    let pong = expect_frame(&mut child, "host 应存活")?;
    assert_eq!(pong["ok"], serde_json::json!(true));

    child.kill()?;
    Ok(())
}

// --- 4a. URL 校验：畸形 authority / 空白 / userinfo / 内网主机名 ------------

#[test]
fn host_url_validation_cases() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let db = tmp.path().join("nm.db");
    let mut child = spawn_nmhost(&db, &[])?;

    // 必须拒绝：空白字符、无 host 的端口、畸形 IPv6、控制字符、非法 scheme。
    let rejected = [
        "http://a b/",
        "http://:80/",
        "http://[::1/",
        "http://x.com/a\u{0007}b",
        "javascript:alert(1)",
        "ftp://example.com/x",
    ];
    for url in rejected {
        let before = query_queued(&db)?.len();
        send_frame(
            &mut child,
            &serde_json::json!({"type": "enqueue", "url": url}),
        )?;
        let resp = expect_frame(&mut child, "rejected url response")?;
        assert_eq!(
            resp["ok"],
            serde_json::json!(false),
            "{url:?} 必须被拒绝: {resp}"
        );
        // 错误原因不得回显 URL 本身（避免 userinfo 泄露）。
        let reason = resp["error"].as_str().unwrap_or("");
        assert!(
            !reason.contains("a b") && !reason.contains("[::1"),
            "错误信息不得回显 URL: {reason}"
        );
        assert_eq!(query_queued(&db)?.len(), before, "{url:?} 不得创建任务");
    }

    // 必须允许：内网单标签主机名（无点号）。
    send_frame(
        &mut child,
        &serde_json::json!({"type": "enqueue", "url": "http://nas/file.zip"}),
    )?;
    let r_nas = expect_frame(&mut child, "intranet host response")?;
    assert_eq!(
        r_nas["ok"],
        serde_json::json!(true),
        "内网主机名 http://nas/file.zip 必须允许: {r_nas}"
    );

    // 必须拒绝：带 userinfo 的 URL，错误信息固定且不含凭据。
    send_frame(
        &mut child,
        &serde_json::json!({"type": "enqueue", "url": "http://user:pass@host/x"}),
    )?;
    let r_userinfo = expect_frame(&mut child, "userinfo url response")?;
    assert_eq!(
        r_userinfo["ok"],
        serde_json::json!(false),
        "带 userinfo 的 URL 必须拒绝: {r_userinfo}"
    );
    assert_eq!(
        r_userinfo["error"].as_str(),
        Some("url must not contain credentials")
    );

    // 用一个合法 URL + 含换行的 Cookie 触发一次 nmhost.log 写入，
    // 以便断言 userinfo 从未进入日志。
    send_frame(
        &mut child,
        &serde_json::json!({
            "type": "enqueue",
            "url": "http://example.com/log-probe.bin",
            "cookies": "a\r\nb"
        }),
    )?;
    let r_probe = expect_frame(&mut child, "log probe response")?;
    assert_eq!(r_probe["ok"], serde_json::json!(true));

    // host 仍存活
    send_frame(&mut child, &serde_json::json!({"type": "ping"}))?;
    let pong = expect_frame(&mut child, "host 应存活")?;
    assert_eq!(pong["ok"], serde_json::json!(true));

    // 数据库里两条合法任务 URL 正确，凭据 URL 未入队。
    let tasks = query_queued(&db)?;
    assert!(task_by_url(&tasks, "http://nas/file.zip").is_some());
    assert!(task_by_url(&tasks, "http://example.com/log-probe.bin").is_some());
    assert!(task_by_url(&tasks, "http://user:pass@host/x").is_none());

    // 关闭 stdin 让 host 退出，随后检查日志。
    drop(child.stdin.take());
    child.wait()?;

    let log = std::fs::read_to_string(log_path(tmp.path()))?;
    println!("=== nmhost.log 原文 ===\n{log}=== end of nmhost.log ===");
    println!("log.contains(\"user\") = {}", log.contains("user"));
    println!("log.contains(\"pass\") = {}", log.contains("pass"));
    assert!(!log.is_empty(), "应有一条 dropped invalid header 日志");
    assert!(
        !log.contains("user")
            && !log.contains("pass")
            && !log.contains("user:pass")
            && !log.contains("pass@")
            && !log.contains("user:pass@host"),
        "userinfo 不得写入 nmhost.log: {log}"
    );
    Ok(())
}

// --- 4a-extra. 入队时拒绝 URL 凭据，且绝不入库/入日志 -----------------------

/// URL 含 userinfo（用户名或密码）时，`enqueue` 必须直接返回
/// `"url must not contain credentials"`，不创建任何任务，凭据也不进入日志；
/// host 仍存活，且无凭据的 URL 仍可正常入队。
#[test]
fn host_enqueue_rejects_url_credentials() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let db = tmp.path().join("nm.db");
    let mut child = spawn_nmhost(&db, &[])?;

    let with_credentials = [
        "http://user@host/x",
        "http://user:pass@host/x",
        "http://:pass@host/x",
        "https://user:pass@example.com/a",
        "http://user:pass@nas/file.zip", // 内网主机名同样不许带凭据
    ];
    for url in with_credentials {
        let before = query_queued(&db)?.len();
        send_frame(
            &mut child,
            &serde_json::json!({
                "type": "enqueue",
                "url": url,
                "filename": "x.bin",
                "cookies": "session=abc"
            }),
        )?;
        let resp = expect_frame(&mut child, "credentials rejection response")?;
        assert_eq!(
            resp["ok"],
            serde_json::json!(false),
            "{url:?} 必须被拒绝: {resp}"
        );
        assert_eq!(
            resp["error"].as_str(),
            Some("url must not contain credentials"),
            "{url:?} 的错误原因必须精确: {resp}"
        );
        assert_eq!(query_queued(&db)?.len(), before, "{url:?} 不得创建任务");
    }

    // 无凭据 URL 仍可入队（避免误杀）。
    send_frame(
        &mut child,
        &serde_json::json!({"type": "enqueue", "url": "http://nas/ok.zip"}),
    )?;
    let ok = expect_frame(&mut child, "no-credentials enqueue response")?;
    assert_eq!(ok["ok"], serde_json::json!(true), "无凭据 URL 应入队: {ok}");
    let tasks = query_queued(&db)?;
    assert_eq!(tasks.len(), 1, "只有无凭据的那条应入库");
    assert_eq!(tasks[0].url, "http://nas/ok.zip");

    // host 仍存活。
    send_frame(&mut child, &serde_json::json!({"type": "ping"}))?;
    let pong = expect_frame(&mut child, "host 应存活")?;
    assert_eq!(pong["ok"], serde_json::json!(true));

    drop(child.stdin.take());
    child.wait()?;

    // 凭据不得出现在日志中（日志文件可能根本不存在）。
    let log = std::fs::read_to_string(log_path(tmp.path())).unwrap_or_default();
    for secret in ["user:pass", ":pass@", "pass@", "user@", "session=abc"] {
        assert!(
            !log.contains(secret),
            "nmhost.log 不得包含 {secret:?}: {log}"
        );
    }
    Ok(())
}

// --- 4b. 超大消息：host 返回错误并关闭，不崩溃，无新任务 -------------------

#[test]
fn host_rejects_oversized_message() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let db = tmp.path().join("nm.db");
    let mut child = spawn_nmhost(&db, &[])?;
    // 构造一个长度前缀超过 64 KiB 的帧（仅长度前缀，不发送实际负载，模拟超长）。
    let big_len: u32 = 200_000;
    let stdin = child
        .stdin
        .as_mut()
        .ok_or_else(|| io_missing("child stdin missing"))?;
    stdin.write_all(&big_len.to_ne_bytes())?;
    stdin.flush()?;
    // host 应在读负载前判定 TooLarge 并返回错误帧后关闭。
    let resp = read_frame(&mut child)?;
    // 可能读到错误帧，也可能直接关闭（None）；二者都算「未崩溃」。
    if let Some(r) = resp {
        assert_eq!(r["ok"], serde_json::json!(false));
    }
    // 等待进程退出（不应崩溃，退出码非 panic 的 101）。
    let status = child.wait()?;
    assert!(
        status.success() || status.code() != Some(101),
        "host 不得 panic 崩溃"
    );
    // 数据库里无任务。
    assert_eq!(query_queued(&db)?.len(), 0, "超大消息不得创建任务");
    Ok(())
}

// --- 5a. 默认构建：测试开关环境变量必须完全无效 ----------------------------

/// 仅在**未启用** `test-hooks` 的默认/发布构建里编译运行。
/// 刻意注入 `IDM_NMHOST_TEST_PANIC=1`，host 必须正常回复每一条 ping，
/// 不 panic、不写 panic 日志、stdout 只有协议帧。
#[cfg(not(feature = "test-hooks"))]
#[test]
fn default_build_ignores_panic_env() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let db = tmp.path().join("nm.db");
    let mut child = spawn_nmhost(&db, &[("IDM_NMHOST_TEST_PANIC", "1")])?;
    for _ in 0..4 {
        send_frame(&mut child, &serde_json::json!({"type": "ping"}))?;
        let pong = expect_frame(&mut child, "ping response")?;
        assert_eq!(pong["ok"], serde_json::json!(true), "不得因环境变量 panic");
        assert!(pong["version"].is_string(), "ping 回复应带 version");
    }
    drop(child.stdin.take());
    let stdout = drain_stdout(&mut child)?;
    let stderr = drain_stderr(&mut child)?;
    let status = child.wait()?;
    assert!(status.success(), "默认构建应正常退出，不得 panic 崩溃");
    assert_eq!(
        assert_stdout_is_frames_only(&stdout)?,
        0,
        "响应帧已被逐条读走，剩余 stdout 应为空"
    );
    let se = String::from_utf8_lossy(&stderr);
    assert!(!se.contains("panic"), "stderr 不得有 panic 文本: {se}");
    let log = std::fs::read_to_string(log_path(tmp.path())).unwrap_or_default();
    assert!(
        !log.contains("panic"),
        "nmhost.log 不得有 panic 记录: {log}"
    );
    Ok(())
}

// --- 5. 标准输出只写帧；panic 入日志 --------------------------------------

#[cfg(feature = "test-hooks")]
#[test]
fn host_stdout_contains_only_frames() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let db = tmp.path().join("nm.db");
    // 测试开关：首个消息触发 panic。
    let mut child = spawn_nmhost(&db, &[("IDM_NMHOST_TEST_PANIC", "1")])?;
    // 发送一个消息触发 panic（host 在写出任何响应帧之前 panic）。
    send_frame(&mut child, &serde_json::json!({"type": "ping"}))?;
    // 关闭 stdin 让 host 自然退出。
    drop(child.stdin.take());
    let stdout = drain_stdout(&mut child)?;
    let stderr = drain_stderr(&mut child)?;
    child.wait()?;

    // stdout 必须全由合法帧组成（此处 panic 发生在写帧前，stdout 应为空）。
    assert_stdout_is_frames_only(&stdout)?;
    // 不得有任何 panic 文本泄漏到 stdout / stderr。
    let so = String::from_utf8_lossy(&stdout);
    let se = String::from_utf8_lossy(&stderr);
    assert!(!so.contains("panic"), "panic 文本不得出现在 stdout: {so}");
    assert!(!se.contains("panic"), "panic 文本不得出现在 stderr: {se}");

    // panic 必须写入 nmhost.log（数据库所在目录）。
    let log_content = std::fs::read_to_string(log_path(tmp.path()))?;
    assert!(
        log_content.contains("panic"),
        "nmhost.log 应包含 panic 记录；日志内容片段: {}",
        log_content.chars().take(500).collect::<String>()
    );
    Ok(())
}

/// 断言 stdout 全由完整帧组成，返回帧数量。
fn assert_stdout_is_frames_only(stdout: &[u8]) -> TestResult<usize> {
    let mut cursor = stdout;
    let mut frames = 0usize;
    while !cursor.is_empty() {
        if cursor.len() < 4 {
            return Err(io_missing("stdout 尾部不足以构成帧长度前缀").into());
        }
        let len = u32::from_ne_bytes([cursor[0], cursor[1], cursor[2], cursor[3]]) as usize;
        if len > 64 * 1024 {
            return Err(io_missing("stdout 含超长帧（非协议字节）").into());
        }
        if cursor.len() < 4 + len {
            return Err(io_missing("stdout 帧负载不完整").into());
        }
        cursor = &cursor[4 + len..];
        frames += 1;
    }
    Ok(frames)
}

// --- 5b. 先正常回复 ping，再触发 panic：stdout 恰好一个完整帧 --------------

#[cfg(feature = "test-hooks")]
#[test]
fn host_stdout_single_ping_frame_then_panic() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let db = tmp.path().join("nm.db");
    // 第 2 条消息触发 panic：第 1 条 ping 必须先被正常回复。
    let mut child = spawn_nmhost(&db, &[("IDM_NMHOST_TEST_PANIC", "2")])?;
    send_frame(&mut child, &serde_json::json!({"type": "ping"}))?;
    send_frame(&mut child, &serde_json::json!({"type": "ping"}))?;
    drop(child.stdin.take());
    let stdout = drain_stdout(&mut child)?;
    let stderr = drain_stderr(&mut child)?;
    child.wait()?;

    // stdout 恰好只有一个完整帧，没有任何其它字节。
    let frames = assert_stdout_is_frames_only(&stdout)?;
    assert_eq!(frames, 1, "stdout 应恰好只有一个帧，实际 {frames} 个");
    let len = u32::from_ne_bytes([stdout[0], stdout[1], stdout[2], stdout[3]]) as usize;
    assert_eq!(stdout.len(), 4 + len, "帧之外不得有任何字节");
    let reply: serde_json::Value = serde_json::from_slice(&stdout[4..])?;
    assert_eq!(reply["ok"], serde_json::json!(true), "应是 ping 回复帧");
    assert!(reply["version"].is_string(), "ping 回复应带 version");
    let so = String::from_utf8_lossy(&stdout);
    let se = String::from_utf8_lossy(&stderr);
    assert!(!so.contains("panic"), "panic 文本不得出现在 stdout: {so}");
    assert!(!se.contains("panic"), "panic 文本不得出现在 stderr: {se}");

    let log_content = std::fs::read_to_string(log_path(tmp.path()))?;
    assert!(log_content.contains("panic"), "panic 应写入 nmhost.log");
    Ok(())
}

// --- 6. 文件名 sanitize（对具体结果值断言）---------------------------------

#[test]
fn filename_is_sanitized() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let db = tmp.path().join("nm.db");
    let mut child = spawn_nmhost(&db, &[])?;

    // (原始文件名, 期望的具体结果值, 断言性质)
    let cases = [
        ("..\\..\\evil.exe", "evil.exe"),
        ("C:\\Windows\\x.dll", "x.dll"),
        ("a/b/c.zip", "c.zip"),
        ("CON", "_CON"),
        ("a.txt:evil", "a.txt_evil"),
    ];
    for (i, (raw, _expected)) in cases.iter().enumerate() {
        let url = format!("http://example.com/f{i}.bin");
        send_frame(
            &mut child,
            &serde_json::json!({
                "type": "enqueue",
                "url": url,
                "filename": raw
            }),
        )?;
        let resp = expect_frame(&mut child, "enqueue 响应")?;
        assert_eq!(
            resp["ok"],
            serde_json::json!(true),
            "filename 用例 {raw} 应入队: {resp}"
        );
    }
    // host 仍存活
    send_frame(&mut child, &serde_json::json!({"type": "ping"}))?;
    let pong = expect_frame(&mut child, "host 应存活")?;
    assert_eq!(pong["ok"], serde_json::json!(true));

    // 逐条按 URL 找到任务，断言具体 file_name 值。
    let tasks = query_queued(&db)?;
    assert_eq!(tasks.len(), cases.len(), "应入队 5 个文件名用例");
    for (i, (raw, expected)) in cases.iter().enumerate() {
        let url = format!("http://example.com/f{i}.bin");
        let rec = task_by_url(&tasks, &url).ok_or_else(|| io_missing("filename task missing"))?;
        let name = rec
            .file_name
            .as_deref()
            .ok_or_else(|| io_missing("file_name missing"))?;
        assert_eq!(name, *expected, "filename 用例 {raw} 的具体结果值不对");
        assert!(
            !name.contains('/') && !name.contains('\\') && !name.contains(':'),
            "file_name 不得含路径分隔符/冒号: {name}"
        );
    }
    // 保留名与冒号的性质断言。
    let reserved = task_by_url(&tasks, "http://example.com/f3.bin")
        .and_then(|t| t.file_name.as_deref())
        .ok_or_else(|| io_missing("CON task missing"))?;
    assert_ne!(reserved, "CON", "CON 必须被处理成非保留名");
    let colon = task_by_url(&tasks, "http://example.com/f4.bin")
        .and_then(|t| t.file_name.as_deref())
        .ok_or_else(|| io_missing("colon task missing"))?;
    assert!(!colon.contains(':'), "a.txt:evil 的结果不得含冒号");

    child.kill()?;
    Ok(())
}

// --- 14. 运行器生命周期门禁 -----------------------------------------------

/// 运行器未运行（runner.lock 不存在）时 enqueue 必须被拒绝，且数据库无新任务。
#[test]
fn host_rejects_enqueue_when_runner_not_running() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let db = tmp.path().join("nm.db");
    let mut child = spawn_nmhost_no_lock(&db)?;

    send_frame(
        &mut child,
        &serde_json::json!({"type": "enqueue", "url": "http://example.com/no-runner.bin"}),
    )?;
    let r = expect_frame(&mut child, "未运行时的 enqueue 响应")?;
    assert_eq!(r["ok"], serde_json::json!(false), "应拒绝: {r}");
    assert_eq!(
        r["error"],
        serde_json::json!("runner_not_running"),
        "错误码应为 runner_not_running: {r}"
    );

    // 宿主存活且数据库无任务。
    send_frame(&mut child, &serde_json::json!({"type": "ping"}))?;
    let pong = expect_frame(&mut child, "宿主应存活")?;
    assert_eq!(pong["ok"], serde_json::json!(true));
    let tasks = query_queued(&db)?;
    assert!(tasks.is_empty(), "拒绝入队后数据库不应有新任务");

    child.kill()?;
    // nmhost.log 应记录拒绝，且不含 URL。
    let log = std::fs::read_to_string(log_path(tmp.path()))?;
    assert!(
        log.contains("runner_not_running"),
        "日志应记录拒绝原因: {log}"
    );
    assert!(!log.contains("example.com"), "日志不得包含 URL: {log}");
    Ok(())
}

/// 模拟运行器持锁后入队成功；同 URL 再次入队返回 duplicate:true 且无新行。
#[test]
fn host_enqueue_with_lock_and_dedup() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let db = tmp.path().join("nm.db");
    // spawn_nmhost 默认持锁（模拟运行器在运行）。
    let mut child = spawn_nmhost(&db, &[])?;

    let url = "http://example.com/dedup.bin";
    send_frame(
        &mut child,
        &serde_json::json!({"type": "enqueue", "url": url, "filename": "dedup.bin"}),
    )?;
    let r1 = expect_frame(&mut child, "第一次 enqueue 响应")?;
    assert_eq!(r1["ok"], serde_json::json!(true), "持锁时应成功: {r1}");
    let id1 = r1["taskId"].as_i64().ok_or_else(|| io_missing("taskId"))?;

    // 同一 URL 再次入队 → duplicate:true，id 相同。
    send_frame(
        &mut child,
        &serde_json::json!({"type": "enqueue", "url": url, "filename": "dedup.bin"}),
    )?;
    let r2 = expect_frame(&mut child, "重复 enqueue 响应")?;
    assert_eq!(r2["ok"], serde_json::json!(true));
    assert_eq!(r2["duplicate"], serde_json::json!(true), "应标记重复: {r2}");
    assert_eq!(r2["taskId"], serde_json::json!(id1), "重复应返回原任务 id");

    // 另一个 URL → 新任务，非重复。
    send_frame(
        &mut child,
        &serde_json::json!({"type": "enqueue", "url": "http://example.com/other.bin"}),
    )?;
    let r3 = expect_frame(&mut child, "不同 URL enqueue 响应")?;
    assert_eq!(r3["ok"], serde_json::json!(true));
    assert_ne!(r3["duplicate"], serde_json::json!(true));
    let id3 = r3["taskId"].as_i64().ok_or_else(|| io_missing("taskId"))?;
    assert_ne!(id3, id1);

    child.kill()?;

    // 数据库恰好 2 行（同 URL 未新增）。
    let tasks = query_queued(&db)?;
    assert_eq!(tasks.len(), 2, "去重后数据库应恰好 2 个任务");

    // nmhost.log 记录结果，不含 URL 与请求头值。
    let log = std::fs::read_to_string(log_path(tmp.path()))?;
    assert!(log.contains("result=accepted"), "日志应记录接受: {log}");
    assert!(log.contains("result=duplicate"), "日志应记录重复: {log}");
    assert!(!log.contains("example.com"), "日志不得包含 URL: {log}");
    Ok(())
}
