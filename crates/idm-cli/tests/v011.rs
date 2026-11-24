#![cfg(windows)]

//! v0.1.1 CLI 层测试：enqueue --out 生效。

#[path = "../../idm-core/tests/support/mod.rs"]
mod support;

use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use support::{ServerConfig, StallConfig, byte_at, start};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

fn run_cli(args: &[&str]) -> std::io::Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_idm-cli"))
        .args(args)
        .output()
}

struct QueueRunner(Child);

impl QueueRunner {
    fn spawn(db: &str) -> std::io::Result<Self> {
        let child = Command::new(env!("CARGO_BIN_EXE_idm-cli"))
            .args(["queue", "run", "--db", db])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(Self(child))
    }
}

impl Drop for QueueRunner {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn wait_complete(path: &Path, size: u64, timeout: Duration) -> bool {
    let began = Instant::now();
    loop {
        if std::fs::metadata(path).map(|m| m.len()).ok() == Some(size) {
            return true;
        }
        if began.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn assert_bytes(path: &Path, size: u64) -> TestResult {
    let data = std::fs::read(path)?;
    if data.len() as u64 != size {
        return Err(format!("文件大小不符: got {}, want {size}", data.len()).into());
    }
    for (i, &b) in data.iter().enumerate() {
        let expected = byte_at(i as u64);
        if b != expected {
            return Err(format!("第 {i} 字节不符: got {b}, want {expected}").into());
        }
    }
    Ok(())
}

async fn enqueue_out_and_download(
    srv_url: &str,
    out: &str,
    expected_name: &str,
    size: u64,
) -> TestResult {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path().join("out");
    std::fs::create_dir_all(&dir)?;
    let db = tmp.path().join("idm.db");
    let dir_s = dir.to_string_lossy().into_owned();
    let db_s = db.to_string_lossy().into_owned();

    let enq = run_cli(&[
        "enqueue", srv_url, "--dir", &dir_s, "--db", &db_s, "--out", out,
    ])?;
    if !enq.status.success() {
        return Err(format!(
            "enqueue 失败: stdout={} stderr={}",
            String::from_utf8_lossy(&enq.stdout),
            String::from_utf8_lossy(&enq.stderr)
        )
        .into());
    }

    let _runner = QueueRunner::spawn(&db_s)?;
    let out_path = dir.join(expected_name);
    if !wait_complete(&out_path, size, Duration::from_secs(10)).await {
        let listing = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        return Err(format!("未下载到 {out_path:?}，目录内容: {listing}").into());
    }
    assert_bytes(&out_path, size)?;
    Ok(())
}

#[tokio::test]
async fn enqueue_out_sets_file_name() -> TestResult {
    let size = 128 * 1024;
    let srv = start(ServerConfig {
        size,
        ..ServerConfig::default()
    })
    .await?;

    let url = srv.url("/source.bin");
    enqueue_out_and_download(&url, "custom.bin", "custom.bin", size).await?;

    // 路径分隔符只取最后一段：不得生成 .._evil.bin 之类名称。
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path().join("out");
    std::fs::create_dir_all(&dir)?;
    let db = tmp.path().join("idm.db");
    let dir_s = dir.to_string_lossy().into_owned();
    let db_s = db.to_string_lossy().into_owned();
    let enq = run_cli(&[
        "enqueue",
        &url,
        "--dir",
        &dir_s,
        "--db",
        &db_s,
        "--out",
        "..\\evil.bin",
    ])?;
    if !enq.status.success() {
        return Err(format!(
            "enqueue(evil) 失败: stdout={} stderr={}",
            String::from_utf8_lossy(&enq.stdout),
            String::from_utf8_lossy(&enq.stderr)
        )
        .into());
    }
    let _runner = QueueRunner::spawn(&db_s)?;
    let evil = dir.join("evil.bin");
    if !wait_complete(&evil, size, Duration::from_secs(10)).await {
        let listing = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        return Err(format!("未下载到 evil.bin，目录内容: {listing}").into());
    }
    assert_bytes(&evil, size)?;
    if dir.join(".._evil.bin").exists() {
        return Err("不应生成 .._evil.bin（路径分隔符未取最后一段）".into());
    }

    srv.shutdown();
    Ok(())
}

// --- 第 2 步：queue pause / resume -------------------------------------------

fn parse_enqueue_id(out: &Output) -> TestResult<i64> {
    let text = String::from_utf8_lossy(&out.stdout);
    let id = text
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix("id="))
        .and_then(|s| s.parse::<i64>().ok())
        .ok_or_else(|| format!("no task id in enqueue output: {text}"))?;
    Ok(id)
}

fn task_state_pause(db: &Path, id: i64) -> TestResult<(String, Option<String>)> {
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let state: String =
        conn.query_row("SELECT state FROM tasks WHERE id=?1", [id], |r| r.get(0))?;
    let pause: Option<String> =
        conn.query_row("SELECT pause_reason FROM tasks WHERE id=?1", [id], |r| {
            r.get(0)
        })?;
    Ok((state, pause))
}

fn enqueue_one(url: &str, db: &Path, dir: &Path) -> TestResult<i64> {
    let db_s = db.to_string_lossy().into_owned();
    let dir_s = dir.to_string_lossy().into_owned();
    let out = run_cli(&["enqueue", url, "--dir", &dir_s, "--db", &db_s])?;
    if !out.status.success() {
        return Err(format!("enqueue failed: {}", String::from_utf8_lossy(&out.stderr)).into());
    }
    parse_enqueue_id(&out)
}

fn stdout_text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn queue_pause_resume_states() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path().join("out");
    std::fs::create_dir_all(&dir)?;
    let db = tmp.path().join("idm.db");
    let db_s = db.to_string_lossy().into_owned();

    // 1. 排队中任务暂停成功。
    let id = enqueue_one("http://127.0.0.1:1/queued.bin", &db, &dir)?;
    let paused = run_cli(&["queue", "pause", &id.to_string(), "--db", &db_s])?;
    if !paused.status.success() || stdout_text(&paused).trim() != format!("paused {id}") {
        return Err(format!(
            "pause(queued) 应成功并输出 paused {id}: code={:?} stdout={}",
            paused.status.code(),
            stdout_text(&paused)
        )
        .into());
    }
    let (state, reason) = task_state_pause(&db, id)?;
    if state != "Paused" || reason.as_deref() != Some("User") {
        return Err(format!("pause 后应为 Paused/User, got {state}/{reason:?}").into());
    }

    // 2. 已暂停任务继续成功。
    let resumed = run_cli(&["queue", "resume", &id.to_string(), "--db", &db_s])?;
    if !resumed.status.success() || stdout_text(&resumed).trim() != format!("resumed {id}") {
        return Err(format!(
            "resume(paused) 应成功并输出 resumed {id}: code={:?} stdout={}",
            resumed.status.code(),
            stdout_text(&resumed)
        )
        .into());
    }
    let (state, _) = task_state_pause(&db, id)?;
    if state != "Queued" {
        return Err(format!("resume 后应为 Queued, got {state}").into());
    }

    // 3. 不存在的 id 两种命令都被拒绝（退出码 2，且给出原因）。
    for cmd in ["pause", "resume"] {
        let out = run_cli(&["queue", cmd, "999999", "--db", &db_s])?;
        let reason = String::from_utf8_lossy(&out.stderr);
        if out.status.code() != Some(2)
            || reason.contains("unknown queue subcommand")
            || reason.is_empty()
        {
            return Err(format!(
                "{cmd}(missing) 应退出码 2 且给出原因, got {:?} stderr={reason}",
                out.status.code()
            )
            .into());
        }
    }
    Ok(())
}

#[tokio::test]
async fn queue_pause_resume_rejects_completed() -> TestResult {
    let size = 64 * 1024;
    let srv = start(ServerConfig {
        size,
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path().join("out");
    std::fs::create_dir_all(&dir)?;
    let db = tmp.path().join("idm.db");
    let db_s = db.to_string_lossy().into_owned();
    let url = srv.url("/done.bin");

    let id = enqueue_one(&url, &db, &dir)?;
    let _runner = QueueRunner::spawn(&db_s)?;
    let out_path = dir.join("done.bin");
    if !wait_complete(&out_path, size, Duration::from_secs(10)).await {
        return Err("已完成文件未出现".into());
    }
    let (state, _) = task_state_pause(&db, id)?;
    if state != "Completed" {
        return Err(format!("期望 Completed, got {state}").into());
    }
    drop(_runner);

    // 已完成任务两种命令都拒绝（退出码 2，且给出原因）。
    for cmd in ["pause", "resume"] {
        let out = run_cli(&["queue", cmd, &id.to_string(), "--db", &db_s])?;
        let reason = String::from_utf8_lossy(&out.stderr);
        if out.status.code() != Some(2)
            || reason.contains("unknown queue subcommand")
            || reason.is_empty()
        {
            return Err(format!(
                "{cmd}(completed) 应退出码 2 且给出原因, got {:?} stderr={reason}",
                out.status.code()
            )
            .into());
        }
    }
    srv.shutdown();
    Ok(())
}

#[tokio::test]
async fn queue_pause_downloading_rejected() -> TestResult {
    let size = 4 * 1024 * 1024;
    let stall_used = Arc::new(AtomicBool::new(false));
    let srv = start(ServerConfig {
        size,
        stall: Some(StallConfig {
            after_bytes: 0,
            duration: Duration::from_secs(8),
            stall_used: Arc::clone(&stall_used),
            variable_blocks: false,
            closed_early: Arc::new(AtomicBool::new(false)),
            close_after_ms: Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX)),
        }),
        ..ServerConfig::default()
    })
    .await?;
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path().join("out");
    std::fs::create_dir_all(&dir)?;
    let db = tmp.path().join("idm.db");
    let db_s = db.to_string_lossy().into_owned();
    let url = srv.url("/slow.bin");

    let id = enqueue_one(&url, &db, &dir)?;
    let _runner = QueueRunner::spawn(&db_s)?;

    // 等待任务进入 Downloading。
    let began = Instant::now();
    let mut downloading = false;
    while began.elapsed() < Duration::from_secs(10) {
        if let Ok((state, _)) = task_state_pause(&db, id)
            && state == "Downloading"
        {
            downloading = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if !downloading {
        return Err("任务未进入 Downloading 状态".into());
    }

    let out = run_cli(&["queue", "pause", &id.to_string(), "--db", &db_s])?;
    if out.status.code() != Some(3) {
        return Err(format!(
            "pause(downloading) 应退出码 3, got {:?} stdout={} stderr={}",
            out.status.code(),
            stdout_text(&out),
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    let (state, _) = task_state_pause(&db, id)?;
    if state != "Downloading" {
        return Err(format!("pause(downloading) 不应修改状态, got {state}").into());
    }
    drop(_runner);
    srv.shutdown();
    Ok(())
}
