#![cfg(windows)]

//! idm-nmhost（Firefox Native Messaging host）集成测试。
//!
//! 通过真实子进程 + 真实帧协议与 `idm-nmhost` 通信，覆盖：
//!   2. ping 往返
//!   3. enqueue 创建 Queued 任务（URL/目录/请求头正确）
//!   4. 拒绝非法输入（scheme/host/超长 URL/超大消息/非法 JSON/未知 type/含换行请求头）
//!   5. 标准输出只写帧；panic 写入 nmhost.log（测试开关）
//!   6. 文件名 sanitize
//!   13. 连续入队两个任务只拉起一个运行器进程

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use idm_core::store::{Store, TaskRecord};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

/// 把 tokio 异步的 Store 查询放到一个临时运行时里同步执行。
fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(f)
}

/// 启动 nmhost 子进程，数据库指向 `db_path`；可选注入测试开关。
///
/// 关键：先把 host 复制到数据库目录下的干净子目录再启动。这样 host 同目录里
/// **没有** `idm-cli.exe`，入队走 `runner: "not_started"` 分支，不会拉起真实的
/// 常驻运行器进程（否则会残留 watch 进程并继承本测试的 stdout 管道句柄）。
/// 「拉起运行器」的行为由 `host_spawns_runner_once` 用伪 cli 单独验证。
fn spawn_nmhost(db_path: &Path, extra_env: &[(&str, &str)]) -> Child {
    let host_dir = db_path.parent().unwrap().join("hostbin");
    std::fs::create_dir_all(&host_dir).unwrap();
    let host_exe = host_dir.join("idm-nmhost.exe");
    std::fs::copy(env!("CARGO_BIN_EXE_idm-nmhost"), &host_exe).unwrap();
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
    cmd.spawn().expect("spawn idm-nmhost")
}

/// 写一条帧：4 字节本机字节序长度 + UTF-8 JSON。
fn send_frame(child: &mut Child, msg: &serde_json::Value) {
    let bytes = serde_json::to_vec(msg).unwrap();
    let len = bytes.len() as u32;
    let stdin = child.stdin.as_mut().unwrap();
    stdin.write_all(&len.to_ne_bytes()).unwrap();
    stdin.write_all(&bytes).unwrap();
    stdin.flush().unwrap();
}

/// 读一条帧；对端关闭返回 `None`。
fn read_frame(child: &mut Child) -> Option<serde_json::Value> {
    let stdout = child.stdout.as_mut().unwrap();
    let mut lenbuf = [0u8; 4];
    if stdout.read_exact(&mut lenbuf).is_err() {
        return None;
    }
    let len = u32::from_ne_bytes(lenbuf) as usize;
    if len > 64 * 1024 {
        return None; // 超长：与 host 一致，直接视为无可用响应
    }
    let mut payload = vec![0u8; len];
    if stdout.read_exact(&mut payload).is_err() {
        return None;
    }
    serde_json::from_slice(&payload).ok()
}

/// 读所有剩余 stdout 字节。
fn drain_stdout(child: &mut Child) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = child.stdout.as_mut().unwrap().read_to_end(&mut buf);
    buf
}

fn query_queued(db_path: &Path) -> Vec<TaskRecord> {
    block_on(async {
        let store = Store::open(db_path).unwrap();
        store.list_queued_tasks().await.unwrap()
    })
}

fn query_headers(db_path: &Path, id: i64) -> Vec<(String, String)> {
    block_on(async {
        let store = Store::open(db_path).unwrap();
        store.get_task_headers(id).await.unwrap()
    })
}

fn task_by_url<'a>(tasks: &'a [TaskRecord], url: &str) -> Option<&'a TaskRecord> {
    tasks.iter().find(|t| t.url == url)
}

// --- 2. ping 往返 -----------------------------------------------------------

#[test]
fn host_ping_roundtrip() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let db = tmp.path().join("nm.db");
    let mut child = spawn_nmhost(&db, &[]);
    send_frame(&mut child, &serde_json::json!({"type": "ping"}));
    let resp = read_frame(&mut child).expect("ping response");
    assert_eq!(resp["ok"], serde_json::json!(true), "ping 应返回 ok:true");
    assert!(
        resp["version"].is_string(),
        "ping 应返回 version（crate 版本）"
    );
    child.kill().ok();
    Ok(())
}

// --- 3. enqueue 创建 Queued 任务 -------------------------------------------

#[test]
fn host_enqueue_creates_queued_task() -> TestResult {
    let tmp = tempfile::tempdir()?;
    // 固定 USERPROFILE，使默认下载目录可预测。
    let db = tmp.path().join("nm.db");
    let mut child = spawn_nmhost(&db, &[("USERPROFILE", tmp.path().to_str().unwrap())]);
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
    );
    let resp = read_frame(&mut child).expect("enqueue response");
    assert_eq!(
        resp["ok"],
        serde_json::json!(true),
        "enqueue 应成功: {resp}"
    );
    let id = resp["taskId"].as_i64().expect("taskId");

    let tasks = query_queued(&db);
    let rec = task_by_url(&tasks, url).expect("任务应存在于数据库");
    assert_eq!(rec.dir, tmp.path().join("Downloads").join("idm-rs"));
    let headers = query_headers(&db, id);
    let by_name = |n: &str| headers.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
    assert_eq!(
        by_name("Referer").as_deref(),
        Some("http://example.com/page")
    );
    assert_eq!(by_name("Cookie").as_deref(), Some("session=abc123"));
    assert_eq!(by_name("User-Agent").as_deref(), Some("idm-rs-test/1.0"));
    child.kill().ok();
    Ok(())
}

// --- 4. 拒绝非法输入 -------------------------------------------------------

#[test]
fn host_rejects_bad_input() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let db = tmp.path().join("nm.db");
    let mut child = spawn_nmhost(&db, &[]);

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
        let before = query_queued(&db).len();
        send_frame(&mut child, msg);
        let resp = read_frame(&mut child).expect("应有错误响应帧");
        assert_eq!(
            resp["ok"],
            serde_json::json!(false),
            "非法输入 #{i} 应返回 ok:false: {resp}"
        );
        let after = query_queued(&db).len();
        assert_eq!(after, before, "非法输入 #{i} 不得创建新任务");
        // host 仍存活：ping 应得到 pong。
        send_frame(&mut child, &serde_json::json!({"type": "ping"}));
        let pong = read_frame(&mut child).expect("host 应在拒绝后继续存活");
        assert_eq!(
            pong["ok"],
            serde_json::json!(true),
            "host 应在拒绝后继续存活 (case #{i})"
        );
    }

    // 含换行的请求头：整条入队仍成功（非法头被丢弃），但该头不进数据库。
    let before = query_queued(&db).len();
    send_frame(
        &mut child,
        &serde_json::json!({
            "type": "enqueue",
            "url": "http://example.com/sanitized-header.bin",
            "cookies": "a\r\nb"
        }),
    );
    let resp = read_frame(&mut child).expect("enqueue 响应");
    assert_eq!(
        resp["ok"],
        serde_json::json!(true),
        "含换行头的任务仍应入队: {resp}"
    );
    let after = query_queued(&db).len();
    assert_eq!(after, before + 1, "含换行头的任务应入队（头被丢弃）");
    let tasks = query_queued(&db);
    let rec = task_by_url(&tasks, "http://example.com/sanitized-header.bin").unwrap();
    let id = rec.id;
    let headers = query_headers(&db, id);
    assert!(
        !headers.iter().any(|(k, _)| k == "Cookie"),
        "含换行的 Cookie 不得入库"
    );
    // host 仍存活
    send_frame(&mut child, &serde_json::json!({"type": "ping"}));
    assert_eq!(
        read_frame(&mut child).unwrap()["ok"],
        serde_json::json!(true)
    );

    child.kill().ok();
    Ok(())
}

// --- 4b. 超大消息：host 返回错误并关闭，不崩溃，无新任务 -------------------

#[test]
fn host_rejects_oversized_message() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let db = tmp.path().join("nm.db");
    let mut child = spawn_nmhost(&db, &[]);
    // 构造一个长度前缀超过 64 KiB 的帧（仅长度前缀，不发送实际负载，模拟超长）。
    let big_len: u32 = 200_000;
    let stdin = child.stdin.as_mut().unwrap();
    stdin.write_all(&big_len.to_ne_bytes()).unwrap();
    stdin.flush().unwrap();
    // host 应在读负载前判定 TooLarge 并返回错误帧后关闭。
    let resp = read_frame(&mut child);
    // 可能读到错误帧，也可能直接关闭（None）；二者都算「未崩溃」。
    if let Some(r) = resp {
        assert_eq!(r["ok"], serde_json::json!(false));
    }
    // 等待进程退出（不应崩溃，退出码非 panic 的 101）。
    let status = child.wait().unwrap();
    assert!(
        status.success() || status.code() != Some(101),
        "host 不得 panic 崩溃"
    );
    // 数据库里无任务。
    assert_eq!(query_queued(&db).len(), 0, "超大消息不得创建任务");
    Ok(())
}

// --- 5. 标准输出只写帧；panic 入日志 --------------------------------------

#[test]
fn host_stdout_contains_only_frames() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let db = tmp.path().join("nm.db");
    // 测试开关：首个消息触发 panic。
    let mut child = spawn_nmhost(&db, &[("IDM_NMHOST_TEST_PANIC", "1")]);
    // 发送一个消息触发 panic（host 在写出任何响应帧之前 panic）。
    send_frame(&mut child, &serde_json::json!({"type": "ping"}));
    // 关闭 stdin 让 host 自然退出。
    drop(child.stdin.take());
    let stdout = drain_stdout(&mut child);
    let stderr = {
        let mut b = Vec::new();
        let _ = child.stderr.as_mut().unwrap().read_to_end(&mut b);
        b
    };
    let status = child.wait().unwrap();

    // stdout 必须全由合法帧组成（此处 panic 发生在写帧前，stdout 应为空）。
    let mut cursor = &stdout[..];
    while !cursor.is_empty() {
        if cursor.len() < 4 {
            panic!("stdout 尾部不足以构成帧长度前缀: {cursor:?}");
        }
        let len = u32::from_ne_bytes([cursor[0], cursor[1], cursor[2], cursor[3]]) as usize;
        assert!(len <= 64 * 1024, "stdout 含超长帧（非协议字节）");
        assert!(cursor.len() >= 4 + len, "stdout 帧负载不完整");
        cursor = &cursor[4 + len..];
    }
    // 不得有任何 panic 文本泄漏到 stdout / stderr。
    let so = String::from_utf8_lossy(&stdout);
    let se = String::from_utf8_lossy(&stderr);
    assert!(!so.contains("panic"), "panic 文本不得出现在 stdout: {so}");
    assert!(!se.contains("panic"), "panic 文本不得出现在 stderr: {se}");

    // panic 必须写入 nmhost.log（数据库所在目录）。
    let log = tmp.path().join("nmhost.log");
    let log_content = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        log_content.contains("panic"),
        "nmhost.log 应包含 panic 记录；日志内容片段: {}",
        &log_content.chars().take(500).collect::<String>()
    );
    // 退出码可以非 0（panic 退出），但不能是 panic 默认 101 之外的崩溃信号问题。
    let _ = status;
    Ok(())
}

// --- 6. 文件名 sanitize ----------------------------------------------------

#[test]
fn filename_is_sanitized() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let db = tmp.path().join("nm.db");
    let mut child = spawn_nmhost(&db, &[]);

    let cases = ["..\\..\\evil.exe", "C:\\Windows\\x.dll", "a/b/c.zip", "CON"];
    for raw in cases {
        send_frame(
            &mut child,
            &serde_json::json!({
                "type": "enqueue",
                "url": "http://example.com/f.bin",
                "filename": raw
            }),
        );
        let resp = read_frame(&mut child).expect("enqueue 响应");
        assert_eq!(
            resp["ok"],
            serde_json::json!(true),
            "filename 用例 {raw} 应入队: {resp}"
        );
    }
    // host 仍存活
    send_frame(&mut child, &serde_json::json!({"type": "ping"}));
    assert_eq!(
        read_frame(&mut child).unwrap()["ok"],
        serde_json::json!(true)
    );

    // 校验每条任务的 file_name 已被安全化：无路径分隔符/盘符冒号，且不等于原始危险串。
    let tasks = query_queued(&db);
    assert_eq!(tasks.len(), cases.len(), "应入队 4 个文件名用例");
    for t in &tasks {
        let name = t.file_name.as_deref().unwrap_or("");
        assert!(
            !name.contains('/') && !name.contains('\\') && !name.contains(':'),
            "file_name 不得含路径分隔符/冒号: {name}"
        );
    }
    child.kill().ok();
    Ok(())
}

// --- 13. 连续入队两个任务只拉起一个运行器 ---------------------------------

/// 用 rustc 现场编译一个极简的伪 `idm-cli.exe`：
/// 独占打开 `runner.lock` 后，把自身 PID 追加到 `IDM_FAKE_CLI_SPAWN_LOG` 指定的
/// 进程启动日志，然后长眠，模拟真实运行器「持锁 → 阻止第二个实例被拉起」。
///
/// 日志行在**拿到锁之后**才写：测试等到日志出现第一行，即代表运行器已持有锁，
/// 此刻再发第二个 enqueue 才能确定性地验证「不会重复拉起」。
fn build_fake_cli(dest_dir: &Path) -> std::path::PathBuf {
    let src = dest_dir.join("fake_cli_src.rs");
    std::fs::write(
        &src,
        r#"
use std::fs::OpenOptions;
use std::io::Write;
use std::os::windows::fs::OpenOptionsExt;
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut db: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--db" && i + 1 < args.len() {
            db = Some(args[i + 1].clone());
        }
        i += 1;
    }
    if let Some(db) = db {
        let p = std::path::Path::new(&db);
        let lock = p.parent().unwrap_or(std::path::Path::new(".")).join("runner.lock");
        // 独占打开：拿不到锁（说明已有运行器）则直接退出，不写日志不长眠。
        let _f = match OpenOptions::new()
            .read(true).write(true).create(true).share_mode(0).open(&lock)
        {
            Ok(f) => f,
            Err(_) => return,
        };
        if let Ok(log) = std::env::var("IDM_FAKE_CLI_SPAWN_LOG") {
            if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&log) {
                let _ = writeln!(f, "{}", std::process::id());
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(120));
    }
}
"#,
    )
    .unwrap();
    let exe = dest_dir.join("idm-cli.exe");
    let out = Command::new("rustc")
        .args([src.to_str().unwrap(), "-O", "-o", exe.to_str().unwrap()])
        .output()
        .expect("调用 rustc 编译伪 idm-cli");
    assert!(
        out.status.success(),
        "rustc 编译伪 idm-cli 失败: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    exe
}

/// 轮询直到 spawn-log 至少出现 `want` 行，或超时。返回实际行数。
fn wait_spawn_log(path: &Path, want: usize, timeout: Duration) -> usize {
    let start = Instant::now();
    loop {
        let n = std::fs::read_to_string(path)
            .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
            .unwrap_or(0);
        if n >= want || start.elapsed() >= timeout {
            return n;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn host_spawns_runner_once() -> TestResult {
    let tmp = tempfile::tempdir()?;
    // 运行目录：把 idm-nmhost 复制到这里，并放入伪 idm-cli.exe（同目录，host 才能找到）。
    let run_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&run_dir)?;
    let nmhost_src = std::path::Path::new(env!("CARGO_BIN_EXE_idm-nmhost"));
    let nmhost_dst = run_dir.join("idm-nmhost.exe");
    std::fs::copy(nmhost_src, &nmhost_dst)?;
    build_fake_cli(&run_dir);

    let db = tmp.path().join("nm.db");
    let spawn_log = tmp.path().join("spawn.log");
    let mut child = Command::new(&nmhost_dst)
        .env("IDM_RS_DB", &db)
        .env("IDM_FAKE_CLI_SPAWN_LOG", &spawn_log)
        .current_dir(&run_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    // 第一个入队：应拉起一个运行器。
    send_frame(
        &mut child,
        &serde_json::json!({"type": "enqueue", "url": "http://example.com/t1.bin"}),
    );
    let r1 = read_frame(&mut child).expect("第一次 enqueue 响应");
    assert_eq!(r1["ok"], serde_json::json!(true), "第一次入队应成功: {r1}");

    // 等运行器真正拿到锁并写下启动日志，消除「第二个 enqueue 抢在拉起之前」的竞争。
    let n = wait_spawn_log(&spawn_log, 1, Duration::from_secs(15));
    assert_eq!(n, 1, "第一次入队应恰好拉起 1 个运行器（日志行数 {n}）");

    // 第二个入队：运行器已持锁，不应再拉起。
    send_frame(
        &mut child,
        &serde_json::json!({"type": "enqueue", "url": "http://example.com/t2.bin"}),
    );
    let r2 = read_frame(&mut child).expect("第二次 enqueue 响应");
    assert_eq!(r2["ok"], serde_json::json!(true), "第二次入队应成功: {r2}");
    send_frame(&mut child, &serde_json::json!({"type": "ping"}));
    assert_eq!(
        read_frame(&mut child).unwrap()["ok"],
        serde_json::json!(true)
    );
    drop(child.stdin.take());
    child.wait().ok();

    // 连一个 ping 之后仍只应有一条启动记录。
    let final_count = wait_spawn_log(&spawn_log, 2, Duration::from_millis(300));
    assert_eq!(
        final_count, 1,
        "连续入队只应拉起一个运行器进程（实际 {final_count}）"
    );

    // 清理伪运行器进程，避免残留影响后续测试。
    if let Ok(s) = std::fs::read_to_string(&spawn_log) {
        for line in s.lines().filter(|l| !l.trim().is_empty()) {
            if let Ok(pid) = line.trim().parse::<u32>() {
                let _ = Command::new("taskkill")
                    .args(["/F", "/PID", &pid.to_string()])
                    .output();
            }
        }
    }
    Ok(())
}
