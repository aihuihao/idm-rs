#![cfg(windows)]

//! Phase 4a CLI 层测试：常驻队列运行器（`queue run --watch`）、单实例锁、config。
//!
//! 只使用本地回环服务器与临时数据库，不访问外网、不碰真实注册表。

#[path = "../../idm-core/tests/support/mod.rs"]
mod support;

use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use support::{ServerConfig, byte_at, start};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

fn run_cli(args: &[&str]) -> std::io::Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_idm-cli"))
        .args(args)
        .output()
}

/// 常驻运行器句柄：Drop 时强杀子进程，确保测试失败也不会残留 watch 进程。
/// 可选把 stdout/stderr 重定向到日志文件，便于诊断。
struct WatchRunner(Child);

impl WatchRunner {
    fn spawn(db: &str, log: Option<&Path>) -> std::io::Result<Self> {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_idm-cli"));
        cmd.args(["queue", "run", "--watch", "--db", db])
            .stdin(Stdio::null());
        if let Some(p) = log {
            let stdout = std::fs::File::create(p)?;
            let stderr = std::fs::File::create(p.with_extension("err"))?;
            cmd.stdout(Stdio::from(stdout)).stderr(Stdio::from(stderr));
        } else {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
        let child = cmd.spawn()?;
        Ok(Self(child))
    }
}

impl Drop for WatchRunner {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for_file(path: &Path, timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        if path.exists() {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 异步版文件等待：轮询间用 `tokio::time::sleep` 让出，避免饿死同进程内的
/// current-thread 测试服务器（否则 Probe 请求无人处理，下载会一直挂住）。
async fn wait_for_file_async(path: &Path, timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        if path.exists() {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 异步版完整产物等待（同上，轮询期间让出 runtime）。
async fn wait_for_complete_async(path: &Path, size: u64, timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        if std::fs::metadata(path).map(|m| m.len()).ok() == Some(size) {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn assert_bytes(path: &Path, size: u64) -> TestResult {
    let data = std::fs::read(path)?;
    assert_eq!(data.len() as u64, size, "文件大小不符: {}", data.len());
    for (i, &b) in data.iter().enumerate() {
        let expected = byte_at(i as u64);
        assert_eq!(b, expected, "第 {i} 字节不符: got {b}, want {expected}");
    }
    Ok(())
}

// --- 11. watch 运行器自动接纳新增任务 -------------------------------------

#[tokio::test]
async fn watch_runner_picks_up_new_tasks() -> TestResult {
    let size = 256 * 1024;
    let srv = start(ServerConfig {
        size,
        ..ServerConfig::default()
    })
    .await?;

    let tmp = tempfile::tempdir()?;
    let dir = tmp.path().join("out");
    std::fs::create_dir_all(&dir)?;
    let dir_s = dir.to_string_lossy().into_owned();
    let db = tmp.path().join("idm.db");
    let db_s = db.to_string_lossy().into_owned();
    let url = srv.url("/watch.bin");

    // 启动常驻运行器（此时数据库为空，队列立即空闲）。
    let watch_log = tmp.path().join("watch.log");
    let _runner = WatchRunner::spawn(&db_s, Some(&watch_log))?;
    assert!(
        wait_for_file_async(&tmp.path().join("runner.lock"), Duration::from_secs(10)).await,
        "watch 运行器应创建 runner.lock 并进入常驻循环"
    );
    // 稍等确保已完成首次 adopt_queued，进入每秒轮询。
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 空闲后入队一个任务。
    let enq = {
        let url = url.clone();
        let dir = dir_s.clone();
        let db = db_s.clone();
        tokio::task::spawn_blocking(move || run_cli(&["enqueue", &url, "--dir", &dir, "--db", &db]))
            .await??
    };
    assert!(
        enq.status.success(),
        "enqueue 应成功: stdout={} stderr={}",
        String::from_utf8_lossy(&enq.stdout),
        String::from_utf8_lossy(&enq.stderr)
    );

    // 3 秒内开始并下载完成（给轮询与进程调度留出充足余量）。
    let out = dir.join("watch.bin");
    if !wait_for_complete_async(&out, size, Duration::from_secs(10)).await {
        let so = std::fs::read_to_string(&watch_log).unwrap_or_default();
        let se = std::fs::read_to_string(watch_log.with_extension("err")).unwrap_or_default();
        let listing = run_cli(&["list", "--db", &db_s])
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        panic!(
            "watch 未接纳任务 {out:?}\n--- watch stdout ---\n{so}\n--- watch stderr ---\n{se}\n--- list ---\n{listing}"
        );
    }
    assert_bytes(&out, size)?;

    srv.shutdown();
    Ok(())
}

// --- 12. watch 单实例：第二个退出码 3，第一个退出后第三个可启动 -----------

#[test]
fn watch_runner_single_instance() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let db = tmp.path().join("idm.db");
    let db_s = db.to_string_lossy().into_owned();
    let lock = tmp.path().join("runner.lock");

    // 实例 1：常驻，持有 runner.lock。
    let runner1 = WatchRunner::spawn(&db_s, None)?;
    assert!(
        wait_for_file(&lock, Duration::from_secs(10)),
        "实例 1 应持有 runner.lock"
    );

    // 实例 2：应被锁挡住，退出码 3，stderr 含 "already running"。
    let second = run_cli(&["queue", "run", "--db", &db_s])?;
    assert_eq!(
        second.status.code(),
        Some(3),
        "第二个实例应以退出码 3 退出: stdout={} stderr={}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("already running"),
        "第二个实例应打印 already running"
    );

    // 实例 1 退出，释放锁。
    drop(runner1);

    // 实例 3：此时应能启动（空队列 → 正常退出码 0，打印 QUEUE DONE）。
    // 进程退出后 Windows 释放独占句柄可能有极短延迟，故重试到成功为止。
    let start = Instant::now();
    let mut third = None;
    loop {
        let out = run_cli(&["queue", "run", "--db", &db_s])?;
        if out.status.code() != Some(3) {
            third = Some(out);
            break;
        }
        if start.elapsed() >= Duration::from_secs(10) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let third = third.expect("实例 1 退出后，第三个实例应能启动（不再返回退出码 3）");
    assert!(
        third.status.success(),
        "第三个实例应正常退出: stdout={} stderr={}",
        String::from_utf8_lossy(&third.stdout),
        String::from_utf8_lossy(&third.stderr)
    );
    assert!(
        String::from_utf8_lossy(&third.stdout).contains("QUEUE DONE"),
        "第三个实例应完成一次空队列调度"
    );
    Ok(())
}

// --- 4（补充）：config set/get download-dir --------------------------------

#[test]
fn config_set_get_download_dir() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let db = tmp.path().join("idm.db");
    let db_s = db.to_string_lossy().into_owned();
    let target = tmp.path().join("my-downloads");
    let target_s = target.to_string_lossy().into_owned();

    // set：目录不存在时应自动创建。
    let set = run_cli(&["config", "set", "download-dir", &target_s, "--db", &db_s])?;
    assert!(
        set.status.success(),
        "config set 应成功: stdout={} stderr={}",
        String::from_utf8_lossy(&set.stdout),
        String::from_utf8_lossy(&set.stderr)
    );
    assert!(target.is_dir(), "config set 应创建下载目录");
    assert!(
        String::from_utf8_lossy(&set.stdout).contains("CONFIG SET"),
        "config set 应回显"
    );

    // get：读回同一值。
    let get = run_cli(&["config", "get", "download-dir", "--db", &db_s])?;
    assert!(get.status.success(), "config get 应成功");
    let out = String::from_utf8_lossy(&get.stdout);
    assert!(out.contains(&target_s), "config get 应返回设置值: {out}");

    // 未知 key 应以退出码 2 拒绝。
    let bad = run_cli(&["config", "set", "unknown-key", "x", "--db", &db_s])?;
    assert_eq!(bad.status.code(), Some(2), "未知 config key 应返回退出码 2");
    Ok(())
}
