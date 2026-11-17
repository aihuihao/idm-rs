#![cfg(windows)]

//! Phase 4c 集成测试：独立常驻运行器的生命周期。
//!
//! - `runner install/uninstall/status`：使用测试专用值名 `idm-rs-runner-test-<随机>`，
//!   RunGuard 的 Drop 保证失败也清理注册表值。
//! - `runner start/stop`：start 后 runner.lock 被占用；stop 后 10 秒内锁释放、
//!   runner.stop 被删除。
//! - 启动抢锁重试：另一句柄短暂占用 runner.lock 500ms 后，运行器仍能启动。
//! - global-max-speed：config set/get 与 2M K/M 后缀解析。
//!
//! 约定：辅助函数一律返回 `TestResult`，用 `?` 传播错误，避免 unwrap/expect。

#[path = "../../idm-core/tests/support/mod.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use support::{ServerConfig, SpeedPolicy, start as start_server};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

const REG_RUN: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

fn io_missing(what: &str) -> std::io::Error {
    std::io::Error::other(what)
}

fn unique_value_name() -> TestResult<String> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    Ok(format!(
        "idm-rs-runner-test-{}-{}-{}",
        std::process::id(),
        nanos,
        n
    ))
}

/// 无论测试成功或失败都删除测试值名对应的注册表 Run 值。
struct RunGuard {
    name: String,
    armed: bool,
}

impl RunGuard {
    fn new(name: String) -> Self {
        Self { name, armed: true }
    }
    fn cleanup(&self) {
        let _ = Command::new("reg")
            .args(["delete", REG_RUN, "/v", &self.name, "/f"])
            .output();
    }
}

impl Drop for RunGuard {
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
    data: PathBuf,
}

fn fixture() -> TestResult<Fixture> {
    let tmp = tempfile::tempdir()?;
    let bin = tmp.path().join("bin");
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&bin)?;
    std::fs::create_dir_all(&data)?;
    let cli = bin.join("idm-cli.exe");
    std::fs::copy(env!("CARGO_BIN_EXE_idm-cli"), &cli)?;
    Ok(Fixture {
        _tmp: tmp,
        cli,
        db: data.join("idm.db"),
        data,
    })
}

fn runner(f: &Fixture, args: &[&str]) -> TestResult<Output> {
    let mut cmd = Command::new(&f.cli);
    cmd.arg("runner").args(args);
    cmd.arg("--db").arg(&f.db);
    Ok(cmd.output()?)
}

/// start/stop 在正常产品代码中会清除自身 std 句柄的继承标志，
/// 因此 Command::output() 不会因分离运行器持有管道而挂起。
fn runner_captured_timed(f: &Fixture, args: &[&str]) -> TestResult<(Output, Duration)> {
    let mut cmd = Command::new(&f.cli);
    cmd.arg("runner").args(args);
    cmd.arg("--db").arg(&f.db);
    let start = Instant::now();
    let output = cmd.output()?;
    Ok((output, start.elapsed()))
}

fn cli(f: &Fixture, args: &[&str]) -> TestResult<Output> {
    let mut cmd = Command::new(&f.cli);
    cmd.args(args);
    cmd.arg("--db").arg(&f.db);
    Ok(cmd.output()?)
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn reg_query_value(name: &str) -> TestResult<Output> {
    Ok(Command::new("reg")
        .args(["query", REG_RUN, "/v", name])
        .output()?)
}

/// 独占打开 runner.lock 是否失败（失败=被运行器持有=运行中）。
fn lock_held(data: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(false)
        .share_mode(0)
        .open(data.join("runner.lock"))
    {
        Ok(f) => {
            drop(f);
            false
        }
        Err(e) => e.raw_os_error() == Some(32),
    }
}

/// 等待条件成立，最多 `timeout`。
fn wait_until<F: Fn() -> bool>(timeout: Duration, cond: F) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// 确保停止运行器并清理哨兵，避免测试间残留。
fn cleanup_runner(f: &Fixture) {
    if lock_held(&f.data) {
        let _ = runner(f, &["stop"]);
    }
    let _ = std::fs::remove_file(f.data.join("runner.stop"));
}

// --- 1. runner install / uninstall / status --------------------------------

#[test]
fn runner_install_uninstall_status() -> TestResult {
    let f = fixture()?;
    let name = unique_value_name()?;
    let _guard = RunGuard::new(name.clone());

    let out = runner(&f, &["install", "--name", &name])?;
    assert!(
        out.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let q = reg_query_value(&name)?;
    assert!(q.status.success(), "Run 值应已写入");
    let val = String::from_utf8_lossy(&q.stdout);
    assert!(val.contains("queue"), "Run 值应含 queue run 参数: {val}");
    assert!(val.contains("idm-cli.exe"), "Run 值应含 cli 路径: {val}");

    let st = runner(&f, &["status", "--name", &name])?;
    assert!(st.status.success());
    let so = stdout(&st);
    assert!(
        so.contains(&format!("value_name: {name}")),
        "status 应回显值名: {so}"
    );
    assert!(
        so.contains("autostart_registered: true"),
        "status 应报告已注册: {so}"
    );

    let un = runner(&f, &["uninstall", "--name", &name])?;
    assert!(un.status.success());
    let q2 = reg_query_value(&name)?;
    assert!(!q2.status.success(), "卸载后 Run 值应不存在");

    let st2 = runner(&f, &["status", "--name", &name])?;
    assert!(stdout(&st2).contains("autostart_registered: false"));
    Ok(())
}

// --- 2. runner start / stop ------------------------------------------------

#[test]
fn runner_start_stop_releases_lock() -> TestResult {
    let f = fixture()?;
    cleanup_runner(&f);

    let (out, elapsed) = runner_captured_timed(&f, &["start"])?;
    assert!(
        elapsed < Duration::from_secs(5),
        "runner start with captured output must return within 5s, took {elapsed:?}"
    );
    assert!(out.status.success(), "start failed: {}", stderr(&out));
    assert!(wait_until(Duration::from_secs(5), || lock_held(&f.data)));

    let sp = f.data.join("runner.stop");
    let st = runner(&f, &["status"])?;
    assert!(stdout(&st).contains("runner_running: true"));

    let (stop, stop_elapsed) = runner_captured_timed(&f, &["stop"])?;
    assert!(
        stop_elapsed < Duration::from_secs(5),
        "runner stop with captured output must return within 5s, took {stop_elapsed:?}"
    );
    assert!(stop.status.success(), "stop failed: {}", stderr(&stop));
    assert!(
        wait_until(Duration::from_secs(10), || !lock_held(&f.data)),
        "stop 后 10 秒内锁应释放"
    );
    assert!(!sp.exists(), "runner.stop 应被删除");
    cleanup_runner(&f);
    Ok(())
}

// --- 3. 启动抢锁重试 -------------------------------------------------------

#[test]
fn runner_start_retries_when_lock_briefly_held() -> TestResult {
    let f = fixture()?;
    cleanup_runner(&f);
    let lock = f.data.join("runner.lock");
    use std::os::windows::fs::OpenOptionsExt;

    // 先独占持有 runner.lock 500ms（模拟另一实例的瞬时探测撞车）。
    let holder = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(&lock)?;
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        drop(holder);
    });

    // 直接启动运行器本体（queue run --watch）：它应在抢锁重试后成功拿到锁。
    // 用 Stdio::null() 避免分离进程继承测试进程的管道。
    let mut watcher = Command::new(&f.cli)
        .args(["queue", "run", "--watch", "--db"])
        .arg(&f.db)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    // RAII guard: always kill the watcher on early return / panic so no orphan
    // process is left behind under parallel `cargo test --workspace`.
    struct KillOnDrop(u32);
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = Command::new("taskkill")
                .args(["/F", "/PID", &self.0.to_string()])
                .output();
        }
    }
    let _watcher_guard = KillOnDrop(watcher.id());

    release
        .join()
        .map_err(|_| io_missing("release thread panicked"))?;
    // 运行器的抢锁重试（200ms × 3s）应在 500ms 锁释放后拿到锁。
    assert!(
        wait_until(Duration::from_secs(4), || lock_held(&f.data)),
        "锁在 500ms 后释放，运行器应在抢锁重试后成功持有锁"
    );
    assert!(
        watcher.try_wait()?.is_none(),
        "运行器应存活并持有锁，而不是因抢锁失败退出"
    );

    // 通过哨兵停止运行器并等待退出。
    std::fs::write(f.data.join("runner.stop"), b"stop")?;
    assert!(
        wait_until(Duration::from_secs(10), || !lock_held(&f.data)),
        "stop 后锁应释放"
    );
    let _ = watcher.wait()?;
    cleanup_runner(&f);
    Ok(())
}

// --- 4. global-max-speed 配置 ---------------------------------------------

#[test]
fn config_global_max_speed_roundtrip() -> TestResult {
    let f = fixture()?;
    for value in ["2M", "512K", "0"] {
        let set = cli(&f, &["config", "set", "global-max-speed", value])?;
        assert!(
            set.status.success(),
            "set {value} failed: {}",
            String::from_utf8_lossy(&set.stderr)
        );
        let get = cli(&f, &["config", "get", "global-max-speed"])?;
        assert!(get.status.success());
        assert!(
            stdout(&get).contains(&format!("global-max-speed={value}")),
            "get 应回显 {value}: {}",
            stdout(&get)
        );
    }
    // 非法值应失败。
    let bad = cli(&f, &["config", "set", "global-max-speed", "abc"])?;
    assert!(!bad.status.success(), "非法限速值应被拒绝");
    Ok(())
}

// --- helpers for task-state / speed tests -----------------------------------

/// 解析 `ENQUEUED id=N ...` 输出中的任务 id。
fn enqueue_and_get_id(f: &Fixture, url: &str, dir: &Path) -> TestResult<i64> {
    let dir_s = dir.to_string_lossy().into_owned();
    let out = cli(f, &["enqueue", url, "--dir", &dir_s])?;
    assert!(out.status.success(), "enqueue failed: {}", stderr(&out));
    let text = stdout(&out);
    let id = text
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix("id="))
        .and_then(|s| s.parse::<i64>().ok())
        .ok_or_else(|| io_missing("no task id in enqueue output"))?;
    Ok(id)
}

/// 只读查询任务的 (state, pause_reason)。
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

fn wait_task_state(db: &Path, id: i64, want: &str, timeout: Duration) -> TestResult<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let (state, _) = task_state_pause(db, id)?;
        if state == want {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("task {id} state={state}, expected {want}").into());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

// --- 5. runner stop leaves active task as Paused(User) ----------------------

#[test]
fn runner_stop_pauses_active_task() -> TestResult {
    let rt = tokio::runtime::Runtime::new()?;
    // Large file -> download lasts several seconds, so stop hits an active task.
    let cfg = ServerConfig {
        size: 64 * 1024 * 1024,
        // 服务端限速 1 MiB/s：64 MiB 需 ~64s，stop 时任务必然仍在活动。
        speed: Some(SpeedPolicy(Arc::new(|_, _| Some(1024 * 1024)))),
        ..ServerConfig::default()
    };
    let server = rt.block_on(start_server(cfg))?;
    let f = fixture()?;
    cleanup_runner(&f);
    let dl = f.data.join("dl");
    std::fs::create_dir_all(&dl)?;

    let (out, _) = runner_captured_timed(&f, &["start"])?;
    assert!(out.status.success(), "start failed: {}", stderr(&out));
    assert!(wait_until(Duration::from_secs(5), || lock_held(&f.data)));

    let id = enqueue_and_get_id(&f, &server.url("/big.bin"), &dl)?;
    // 等到任务真正进入 Downloading（否则 stop 会命中 Queued，测试无意义）。
    wait_task_state(&f.db, id, "Downloading", Duration::from_secs(15))?;

    let (stop, _) = runner_captured_timed(&f, &["stop"])?;
    assert!(stop.status.success(), "stop failed: {}", stderr(&stop));
    assert!(
        wait_until(Duration::from_secs(10), || !lock_held(&f.data)),
        "stop 后 10 秒内锁应释放"
    );
    assert!(!f.data.join("runner.stop").exists(), "runner.stop 应被删除");

    let (state, pause) = task_state_pause(&f.db, id)?;
    assert_eq!(state, "Paused", "stop 后任务应为 Paused, got {state}");
    assert_eq!(
        pause.as_deref(),
        Some("User"),
        "stop 后 pause_reason 应为 User, got {pause:?}"
    );
    cleanup_runner(&f);
    server.shutdown();
    Ok(())
}

// --- 6. global-max-speed throttles then un-throttles at runtime -------------

#[test]
fn global_max_speed_throttles_and_updates() -> TestResult {
    let rt = tokio::runtime::Runtime::new()?;
    const SIZE: u64 = 4 * 1024 * 1024;
    let cfg = ServerConfig {
        size: SIZE,
        ..ServerConfig::default()
    };
    let server = rt.block_on(start_server(cfg))?;
    let f = fixture()?;
    cleanup_runner(&f);
    let dl = f.data.join("dl");
    std::fs::create_dir_all(&dl)?;

    // 512 KiB/s -> 4 MiB 理论 8s；要求至少 6s（留测量余量）。
    let set = cli(&f, &["config", "set", "global-max-speed", "512K"])?;
    assert!(set.status.success());
    let (out, _) = runner_captured_timed(&f, &["start"])?;
    assert!(out.status.success());
    assert!(wait_until(Duration::from_secs(5), || lock_held(&f.data)));

    let id1 = enqueue_and_get_id(&f, &server.url("/throttled.bin"), &dl)?;
    let t0 = Instant::now();
    wait_task_state(&f.db, id1, "Completed", Duration::from_secs(30))?;
    let throttled = t0.elapsed();
    assert!(
        throttled >= Duration::from_secs(6),
        "512K 限速下 4MiB 应至少 6s, got {throttled:?}"
    );

    // 运行中改为 0（不限速）：runner 每 5s 重读配置，等待其生效后再入队，
    // 同样 4MiB 应明显更快（< 3s）。
    let set0 = cli(&f, &["config", "set", "global-max-speed", "0"])?;
    assert!(set0.status.success());
    std::thread::sleep(Duration::from_secs(6)); // 等待 5s 刷新周期
    let id2 = enqueue_and_get_id(&f, &server.url("/unthrottled.bin"), &dl)?;
    let t1 = Instant::now();
    wait_task_state(&f.db, id2, "Completed", Duration::from_secs(30))?;
    let fast = t1.elapsed();
    assert!(
        fast < Duration::from_secs(3),
        "解除限速后 4MiB 应 < 3s, got {fast:?} (throttled was {throttled:?})"
    );

    let (stop, _) = runner_captured_timed(&f, &["stop"])?;
    assert!(stop.status.success());
    assert!(wait_until(Duration::from_secs(10), || !lock_held(&f.data)));
    cleanup_runner(&f);
    server.shutdown();
    Ok(())
}
