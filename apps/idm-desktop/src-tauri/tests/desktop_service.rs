#![cfg(windows)]

//! DesktopService / Throttle 的集成测试。
//! 复用 idm-core 的本地测试服务器（#[path] 引入，不新增依赖）。

#[path = "../../../../crates/idm-core/tests/support/mod.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use idm_core::store::{PauseReasonDb, Store, TaskStateDb};
use idm_desktop::service::{DesktopService, ServiceError, Settings, TaskView};
use idm_desktop::throttle::Throttle;
use support::{ServerConfig, SpeedPolicy, byte_at, start};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

const MIB: u64 = 1024 * 1024;
const KIB: u64 = 1024;

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> std::io::Result<Self> {
        let n = TMP_SEQ.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("idm-desktop-test-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

async fn seed_settings(db: &Path, download_dir: &Path, speed: Option<u64>) -> TestResult {
    let store = Store::open(db)?;
    store
        .set_setting("download-dir", &download_dir.to_string_lossy())
        .await?;
    if let Some(v) = speed {
        store
            .set_setting("global-max-speed", &v.to_string())
            .await?;
    }
    store.shutdown().await?;
    Ok(())
}

fn acquire_lock_exclusive(db: &Path) -> TestResult<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    let lock = db
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("runner.lock");
    Ok(std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(lock)?)
}

async fn wait_for<F>(svc: &DesktopService, mut pred: F, timeout: Duration) -> TestResult
where
    F: FnMut(&[TaskView]) -> bool,
{
    let deadline = Instant::now() + timeout;
    loop {
        let views = svc.list_tasks().await?;
        if pred(&views) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("timeout waiting for task condition".into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn verify_bytes(path: &Path, size: u64) -> TestResult {
    let data = std::fs::read(path)?;
    if data.len() as u64 != size {
        return Err(format!("length mismatch: {} vs {size}", data.len()).into());
    }
    for (i, &b) in data.iter().enumerate() {
        if b != byte_at(i as u64) {
            return Err(format!("byte {i} mismatch").into());
        }
    }
    Ok(())
}

fn default_settings(download_dir: &Path) -> Settings {
    Settings {
        download_dir: download_dir.to_string_lossy().into_owned(),
        global_max_speed: 0,
        max_tasks: 3,
        default_connections: 8,
        max_connections_per_host: 8,
        resume_uses_current_connections: true,
        autostart: false,
        notify_on_complete: true,
    }
}

// --- 1：添加后任务完成，文件逐字节正确 --------------------------------

#[tokio::test]
async fn add_task_downloads_correctly() -> TestResult {
    let tmp = TempDir::new()?;
    let size = 256 * KIB;
    let srv = start(ServerConfig {
        size,
        ..ServerConfig::default()
    })
    .await?;
    let db = tmp.join("idm.db");
    let download = tmp.join("downloads");
    std::fs::create_dir_all(&download)?;
    seed_settings(&db, &download, None).await?;

    let svc = DesktopService::open(db.clone()).await?;
    let id = svc.add_task(srv.url("/one.bin"), Some(1)).await?;
    wait_for(
        &svc,
        |v| v.iter().any(|t| t.id == id && t.state == "Completed"),
        Duration::from_secs(20),
    )
    .await?;
    verify_bytes(&download.join("one.bin"), size)?;
    srv.shutdown();
    svc.shutdown().await?;
    Ok(())
}

// --- 2：暂停后 Paused(User)，继续后完成且逐字节正确 --------------------

#[tokio::test]
async fn pause_and_resume() -> TestResult {
    let tmp = TempDir::new()?;
    let size = 2 * MIB;
    let srv = start(ServerConfig {
        size,
        speed: Some(SpeedPolicy(Arc::new(|_, _| Some(512 * KIB)))),
        ..ServerConfig::default()
    })
    .await?;
    let db = tmp.join("idm.db");
    let download = tmp.join("downloads");
    std::fs::create_dir_all(&download)?;
    seed_settings(&db, &download, None).await?;

    let svc = DesktopService::open(db.clone()).await?;
    let id = svc.add_task(srv.url("/two.bin"), Some(1)).await?;
    wait_for(
        &svc,
        |v| v.iter().any(|t| t.id == id && t.downloaded > 0),
        Duration::from_secs(20),
    )
    .await?;
    svc.pause_task(id).await?;
    wait_for(
        &svc,
        |v| {
            v.iter().any(|t| {
                t.id == id && t.state == "Paused" && t.pause_reason.as_deref() == Some("User")
            })
        },
        Duration::from_secs(10),
    )
    .await?;
    svc.resume_task(id).await?;
    wait_for(
        &svc,
        |v| v.iter().any(|t| t.id == id && t.state == "Completed"),
        Duration::from_secs(40),
    )
    .await?;
    verify_bytes(&download.join("two.bin"), size)?;
    srv.shutdown();
    svc.shutdown().await?;
    Ok(())
}

// --- 3：删除任务（带/不带文件）-----------------------------------------

#[tokio::test]
async fn remove_task_with_and_without_file() -> TestResult {
    let tmp = TempDir::new()?;
    let size = 512 * KIB;
    let srv = start(ServerConfig {
        size,
        ..ServerConfig::default()
    })
    .await?;
    let slow_srv = start(ServerConfig {
        size,
        speed: Some(SpeedPolicy(Arc::new(|_, _| Some(32 * KIB)))),
        ..ServerConfig::default()
    })
    .await?;
    let db = tmp.join("idm.db");
    let download = tmp.join("downloads");
    std::fs::create_dir_all(&download)?;
    seed_settings(&db, &download, None).await?;

    let svc = DesktopService::open(db.clone()).await?;

    // delete_file=false：完成后删除记录但保留文件。
    let id_a = svc.add_task(srv.url("/a.bin"), Some(1)).await?;
    wait_for(
        &svc,
        |v| v.iter().any(|t| t.id == id_a && t.state == "Completed"),
        Duration::from_secs(30),
    )
    .await?;
    svc.remove_task(id_a, false).await?;
    assert!(
        download.join("a.bin").exists(),
        "delete_file=false 应保留最终文件"
    );

    // delete_file=true：暂停中的任务有 .idm-part，删除记录时一并删除。
    let id_b = svc.add_task(slow_srv.url("/b.bin"), Some(1)).await?;
    wait_for(
        &svc,
        |v| v.iter().any(|t| t.id == id_b && t.downloaded > 0),
        Duration::from_secs(20),
    )
    .await?;
    svc.pause_task(id_b).await?;
    wait_for(
        &svc,
        |v| v.iter().any(|t| t.id == id_b && t.state == "Paused"),
        Duration::from_secs(10),
    )
    .await?;
    let part = download.join("b.bin.idm-part");
    assert!(part.exists(), "暂停任务应保留 .idm-part");
    svc.remove_task(id_b, true).await?;
    assert!(!part.exists(), "delete_file=true 应删除 .idm-part");

    srv.shutdown();
    slow_srv.shutdown();
    svc.shutdown().await?;
    Ok(())
}

// --- 4：全局限速立即生效 -----------------------------------------------

#[tokio::test]
async fn global_speed_applies_immediately() -> TestResult {
    let tmp = TempDir::new()?;
    let size = 4 * MIB;
    let srv = start(ServerConfig {
        size,
        ..ServerConfig::default()
    })
    .await?;
    let db = tmp.join("idm.db");
    let download = tmp.join("downloads");
    std::fs::create_dir_all(&download)?;
    seed_settings(&db, &download, Some(512 * KIB)).await?;

    let svc = DesktopService::open(db.clone()).await?;
    let started = Instant::now();
    let id = svc.add_task(srv.url("/speed.bin"), Some(1)).await?;
    wait_for(
        &svc,
        |v| v.iter().any(|t| t.id == id && t.state == "Completed"),
        Duration::from_secs(40),
    )
    .await?;
    let elapsed = started.elapsed().as_secs_f64();
    let avg = size as f64 / elapsed;
    assert!(
        avg <= 614.0 * KIB as f64,
        "平均速度 {avg:.0} B/s 超过 614 KiB/s"
    );
    srv.shutdown();
    svc.shutdown().await?;
    Ok(())
}

// --- 5：非法 URL 被拒且无新任务 ----------------------------------------

#[tokio::test]
async fn invalid_url_rejected() -> TestResult {
    let tmp = TempDir::new()?;
    let db = tmp.join("idm.db");
    let download = tmp.join("downloads");
    std::fs::create_dir_all(&download)?;
    seed_settings(&db, &download, None).await?;

    let svc = DesktopService::open(db.clone()).await?;
    for bad in ["file:///x", "http://user:pw@host/x", "http:///x"] {
        match svc.add_task(bad.to_string(), None).await {
            Err(ServiceError::InvalidInput(_)) => {}
            other => return Err(format!("{bad}: expected InvalidInput, got {other:?}").into()),
        }
    }
    assert!(
        svc.list_tasks().await?.is_empty(),
        "非法 URL 不应产生新任务"
    );
    svc.shutdown().await?;
    Ok(())
}

// --- 6：列表不含请求头与 Cookie ----------------------------------------

#[tokio::test]
async fn list_hides_headers_and_cookies() -> TestResult {
    let tmp = TempDir::new()?;
    let db = tmp.join("idm.db");
    let download = tmp.join("downloads");
    std::fs::create_dir_all(&download)?;

    // 在 service 打开之前，用 store 直接入队一个带 Cookie 头的暂停任务。
    {
        let store = Store::open(&db)?;
        let id = store
            .enqueue_task(
                idm_core::store::QueuedTask {
                    url: "http://127.0.0.1:9/secret.bin".to_string(),
                    dir: download.to_string_lossy().into_owned(),
                    strategy: "Auto".to_string(),
                    connections: 1,
                    max_speed: None,
                    file_name: None,
                },
                Some(vec![(
                    "Cookie".to_string(),
                    "secret_session=abcd1234xyz".to_string(),
                )]),
            )
            .await?;
        store
            .update_state(id, TaskStateDb::Paused, Some(PauseReasonDb::User), None)
            .await?;
        store.shutdown().await?;
    }
    seed_settings(&db, &download, None).await?;

    let svc = DesktopService::open(db.clone()).await?;
    let json = serde_json::to_string(&svc.list_tasks().await?)?;
    assert!(
        !json.contains("secret_session"),
        "列表 JSON 不得含 Cookie 值"
    );
    assert!(!json.contains("headers"), "列表 JSON 不得含 headers 字段");
    svc.shutdown().await?;
    Ok(())
}

// --- 7：url_display 去掉凭据/查询/片段 ----------------------------------

#[tokio::test]
async fn url_display_strips_secrets() -> TestResult {
    let tmp = TempDir::new()?;
    let db = tmp.join("idm.db");
    let download = tmp.join("downloads");
    std::fs::create_dir_all(&download)?;
    {
        let store = Store::open(&db)?;
        let id = store
            .enqueue_task(
                idm_core::store::QueuedTask {
                    url: "https://u:p@h.com:8443/a/b.zip?token=x#f".to_string(),
                    dir: download.to_string_lossy().into_owned(),
                    strategy: "Auto".to_string(),
                    connections: 1,
                    max_speed: None,
                    file_name: None,
                },
                None,
            )
            .await?;
        store
            .update_state(id, TaskStateDb::Paused, Some(PauseReasonDb::User), None)
            .await?;
        store.shutdown().await?;
    }
    seed_settings(&db, &download, None).await?;

    let svc = DesktopService::open(db.clone()).await?;
    let views = svc.list_tasks().await?;
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].url_display, "https://h.com:8443/a/b.zip");
    svc.shutdown().await?;
    Ok(())
}

// --- 8：观察模式只读 ----------------------------------------------------

#[tokio::test]
async fn observer_mode_is_read_only() -> TestResult {
    let tmp = TempDir::new()?;
    let db = tmp.join("idm.db");
    let download = tmp.join("downloads");
    std::fs::create_dir_all(&download)?;
    {
        let store = Store::open(&db)?;
        let id = store
            .enqueue_task(
                idm_core::store::QueuedTask {
                    url: "http://127.0.0.1:9/hold.bin".to_string(),
                    dir: download.to_string_lossy().into_owned(),
                    strategy: "Auto".to_string(),
                    connections: 1,
                    max_speed: None,
                    file_name: None,
                },
                None,
            )
            .await?;
        store
            .update_state(id, TaskStateDb::Downloading, None, None)
            .await?;
        store.shutdown().await?;
    }
    seed_settings(&db, &download, None).await?;

    let _guard = acquire_lock_exclusive(&db)?;
    let svc = DesktopService::open(db.clone()).await?;
    assert!(svc.is_read_only(), "持锁打开应为只读观察模式");

    assert!(matches!(
        svc.add_task("http://127.0.0.1/x".into(), None).await,
        Err(ServiceError::ReadOnly)
    ));
    assert!(matches!(
        svc.pause_task(1).await,
        Err(ServiceError::ReadOnly)
    ));
    assert!(matches!(
        svc.resume_task(1).await,
        Err(ServiceError::ReadOnly)
    ));
    assert!(matches!(
        svc.remove_task(1, false).await,
        Err(ServiceError::ReadOnly)
    ));
    assert!(matches!(
        svc.set_settings(default_settings(&download)).await,
        Err(ServiceError::ReadOnly)
    ));

    let views = svc.list_tasks().await?;
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].state, "Downloading", "观察模式不应执行 recover");
    svc.shutdown().await?;
    Ok(())
}

// --- 9：设置校验 --------------------------------------------------------

#[tokio::test]
async fn settings_validation() -> TestResult {
    let tmp = TempDir::new()?;
    let db = tmp.join("idm.db");
    let download = tmp.join("downloads");
    std::fs::create_dir_all(&download)?;
    seed_settings(&db, &download, None).await?;

    let svc = DesktopService::open(db.clone()).await?;
    let mut s = default_settings(&download);

    let mut bad = s.clone();
    bad.max_tasks = 0;
    assert!(matches!(
        svc.set_settings(bad).await,
        Err(ServiceError::InvalidInput(_))
    ));
    let mut bad = s.clone();
    bad.max_tasks = 17;
    assert!(matches!(
        svc.set_settings(bad).await,
        Err(ServiceError::InvalidInput(_))
    ));
    let mut bad = s.clone();
    bad.default_connections = 0;
    assert!(matches!(
        svc.set_settings(bad).await,
        Err(ServiceError::InvalidInput(_))
    ));
    let mut bad = s.clone();
    bad.default_connections = 33;
    assert!(matches!(
        svc.set_settings(bad).await,
        Err(ServiceError::InvalidInput(_))
    ));

    let fresh = tmp.join("fresh-downloads");
    assert!(!fresh.exists());
    s.download_dir = fresh.to_string_lossy().into_owned();
    svc.set_settings(s).await?;
    assert!(fresh.is_dir(), "不存在的 download_dir 应被创建");

    svc.shutdown().await?;
    Ok(())
}

// --- 9a：每主机连接上限设置：默认值、校验与重启后重新读取 ----------------

#[tokio::test]
async fn host_limit_setting_roundtrip() -> TestResult {
    let tmp = TempDir::new()?;
    let db = tmp.join("idm.db");
    let download = tmp.join("downloads");
    std::fs::create_dir_all(&download)?;
    seed_settings(&db, &download, None).await?;

    let svc = DesktopService::open(db.clone()).await?;
    // 未设置时默认为 8。
    assert_eq!(
        svc.get_settings().await?.max_connections_per_host,
        8,
        "每主机连接上限默认应为 8"
    );

    // 0 与 33 越界，应被拒绝。
    let mut bad = default_settings(&download);
    bad.max_connections_per_host = 0;
    assert!(matches!(
        svc.set_settings(bad).await,
        Err(ServiceError::InvalidInput(_))
    ));
    let mut bad = default_settings(&download);
    bad.max_connections_per_host = 33;
    assert!(matches!(
        svc.set_settings(bad).await,
        Err(ServiceError::InvalidInput(_))
    ));

    // 保存合法值后重新打开，open 时应重新读取到新值。
    let mut s = default_settings(&download);
    s.max_connections_per_host = 4;
    svc.set_settings(s).await?;
    svc.shutdown().await?;

    let reopened = DesktopService::open(db.clone()).await?;
    assert_eq!(
        reopened.get_settings().await?.max_connections_per_host,
        4,
        "重新 open 后应读取到保存的每主机连接上限"
    );
    reopened.shutdown().await?;
    Ok(())
}

// --- 9b：未设置 download-dir 时使用默认下载目录 -------------------------

#[tokio::test]
async fn default_download_dir_when_unset() -> TestResult {
    let tmp = TempDir::new()?;
    let db = tmp.join("idm.db");
    // 刻意不写入 download-dir 设置。
    let svc = DesktopService::open(db.clone()).await?;
    let settings = svc.get_settings().await?;
    let normalized = settings.download_dir.replace('/', "\\");
    assert!(
        normalized.ends_with("Downloads\\idm-rs"),
        "默认下载目录应以 Downloads\\idm-rs 结尾，实际: {}",
        settings.download_dir
    );
    svc.shutdown().await?;
    Ok(())
}

// --- 10：节流规则 -------------------------------------------------------

#[tokio::test]
async fn throttle_rules() -> TestResult {
    let mut throttle = Throttle::new(Duration::from_millis(250));
    let base = Instant::now();

    assert!(throttle.should_emit(base, b"same"), "首次应推送");
    assert!(
        !throttle.should_emit(base + Duration::from_millis(10), b"same"),
        "相同内容不推送"
    );
    assert!(
        !throttle.should_emit(base + Duration::from_millis(100), b"changed"),
        "间隔不足不推送"
    );
    assert!(
        throttle.should_emit(base + Duration::from_millis(300), b"changed"),
        "内容变化且超间隔应推送"
    );

    let mut t = Throttle::new(Duration::from_millis(250));
    let mut emitted = 0;
    for i in 0..10 {
        let now = base + Duration::from_millis(i * 100);
        if t.should_emit(now, format!("s{i}").as_bytes()) {
            emitted += 1;
        }
    }
    assert!(emitted <= 4, "1 秒内快速变化推送次数 {emitted} 应 <= 4");
    Ok(())
}

// --- 11：shutdown 暂停活动任务并释放锁 ---------------------------------

#[tokio::test]
async fn shutdown_pauses_active_task() -> TestResult {
    let tmp = TempDir::new()?;
    let size = 8 * MIB;
    let srv = start(ServerConfig {
        size,
        speed: Some(SpeedPolicy(Arc::new(|_, _| Some(256 * KIB)))),
        ..ServerConfig::default()
    })
    .await?;
    let db = tmp.join("idm.db");
    let download = tmp.join("downloads");
    std::fs::create_dir_all(&download)?;
    seed_settings(&db, &download, None).await?;

    let svc = DesktopService::open(db.clone()).await?;
    let id = svc.add_task(srv.url("/long.bin"), Some(1)).await?;
    wait_for(
        &svc,
        |v| v.iter().any(|t| t.id == id && t.downloaded > 0),
        Duration::from_secs(20),
    )
    .await?;
    svc.shutdown().await?;

    let store = Store::open(&db)?;
    let rec = store.get_task(id).await?;
    assert_eq!(rec.state, TaskStateDb::Paused);
    assert_eq!(rec.pause_reason, Some(PauseReasonDb::User));
    store.shutdown().await?;

    let _guard = acquire_lock_exclusive(&db)?;
    srv.shutdown();
    Ok(())
}

// --- 12：删除正在下载的任务（Phase 5a-4 第 1 步）-------------------------

#[tokio::test]
async fn remove_active_task() -> TestResult {
    let tmp = TempDir::new()?;
    let size = 256 * KIB;
    let slow_srv = start(ServerConfig {
        size,
        speed: Some(SpeedPolicy(Arc::new(|_, _| Some(16 * KIB)))),
        ..ServerConfig::default()
    })
    .await?;
    let fast_srv = start(ServerConfig {
        size,
        ..ServerConfig::default()
    })
    .await?;
    let db = tmp.join("idm.db");
    let download = tmp.join("downloads");
    std::fs::create_dir_all(&download)?;
    seed_settings(&db, &download, None).await?;

    let svc = DesktopService::open(db.clone()).await?;
    let id = svc.add_task(slow_srv.url("/active.bin"), Some(1)).await?;
    wait_for(
        &svc,
        |v| v.iter().any(|t| t.id == id && t.downloaded > 0),
        Duration::from_secs(20),
    )
    .await?;

    svc.remove_task(id, true).await?;

    assert!(
        svc.list_tasks().await?.iter().all(|t| t.id != id),
        "删除后任务应从列表消失"
    );
    assert!(
        !download.join("active.bin.idm-part").exists(),
        "删除活动任务应移除 .idm-part"
    );
    assert!(
        !download.join("active.bin").exists(),
        "delete_file=true 应移除最终文件"
    );

    // 删除活动任务后，再添加新任务仍能正常下载完成。
    let id2 = svc.add_task(fast_srv.url("/after.bin"), Some(1)).await?;
    wait_for(
        &svc,
        |v| v.iter().any(|t| t.id == id2 && t.state == "Completed"),
        Duration::from_secs(30),
    )
    .await?;
    verify_bytes(&download.join("after.bin"), size)?;

    slow_srv.shutdown();
    fast_srv.shutdown();
    svc.shutdown().await?;
    Ok(())
}

// --- 13：reveal_args 纯函数（空格/中文/引号）-----------------------------

#[test]
fn reveal_args_builds_select_argument() -> TestResult {
    use idm_desktop::service::reveal_args;

    let spaced = PathBuf::from("D:\\my downloads\\a b.bin");
    assert_eq!(
        reveal_args(&spaced)?,
        "/select,\"D:\\my downloads\\a b.bin\""
    );
    let chinese = PathBuf::from("D:\\下载\\文件 一.bin");
    assert_eq!(reveal_args(&chinese)?, "/select,\"D:\\下载\\文件 一.bin\"");
    let quoted = PathBuf::from("D:\\a\"b.bin");
    match reveal_args(&quoted) {
        Err(ServiceError::InvalidInput(_)) => {}
        other => {
            return Err(format!("expected InvalidInput for quoted path, got {other:?}").into());
        }
    }
    Ok(())
}

// --- 14：清除已完成只删记录不删文件 -------------------------------------

#[tokio::test]
async fn clear_completed_removes_only_completed() -> TestResult {
    let tmp = TempDir::new()?;
    let db = tmp.join("idm.db");
    let download = tmp.join("downloads");
    std::fs::create_dir_all(&download)?;

    let completed_file = download.join("done.bin");
    std::fs::write(&completed_file, b"already downloaded")?;

    {
        let store = Store::open(&db)?;
        let id_done = store
            .create_task(idm_core::store::NewTask {
                url: "http://example.com/done.bin".to_string(),
                dir: download.to_string_lossy().into_owned(),
                strategy: "Auto".to_string(),
                connections: 1,
            })
            .await?;
        store.set_final_name(id_done, "done.bin").await?;
        store
            .update_state(id_done, TaskStateDb::Completed, None, None)
            .await?;
        let id_paused = store
            .create_task(idm_core::store::NewTask {
                url: "http://example.com/paused.bin".to_string(),
                dir: download.to_string_lossy().into_owned(),
                strategy: "Auto".to_string(),
                connections: 1,
            })
            .await?;
        store
            .update_state(id_paused, TaskStateDb::Paused, None, None)
            .await?;
        store.shutdown().await?;
    }
    seed_settings(&db, &download, None).await?;

    let svc = DesktopService::open(db.clone()).await?;
    let removed = svc.clear_completed().await?;
    assert_eq!(removed, 1, "只应删除 1 条已完成任务");

    let views = svc.list_tasks().await?;
    assert_eq!(views.len(), 1, "只剩已暂停任务");
    assert_eq!(views[0].state, "Paused");
    assert!(completed_file.exists(), "清除记录不得删除已下载文件");

    svc.shutdown().await?;
    Ok(())
}

// --- 15：开机启动开关（随机测试值名 + Drop 守卫）------------------------

struct RunValueGuard {
    names: Vec<String>,
}

impl RunValueGuard {
    fn cleanup(&self) {
        for name in &self.names {
            let _ = std::process::Command::new("reg")
                .args([
                    "delete",
                    "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run",
                    "/v",
                    name,
                    "/f",
                ])
                .output();
        }
    }
}

impl Drop for RunValueGuard {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn unique_run_name(prefix: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    format!("{prefix}-{}-{nanos}-{n}", std::process::id())
}

fn reg_query(name: &str) -> bool {
    std::process::Command::new("reg")
        .args([
            "query",
            "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run",
            "/v",
            name,
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn autostart_toggle() -> TestResult {
    let desktop_name = unique_run_name("idm-desktop-test");
    let runner_name = unique_run_name("idm-runner-test");
    let guard = RunValueGuard {
        names: vec![desktop_name.clone(), runner_name.clone()],
    };
    guard.cleanup();

    let tmp = TempDir::new()?;
    let db = tmp.join("idm.db");
    let download = tmp.join("downloads");
    std::fs::create_dir_all(&download)?;
    seed_settings(&db, &download, None).await?;

    // 预置旧的 CLI runner 值，验证开启桌面启动时会删除它。
    let seeded = std::process::Command::new("reg")
        .args([
            "add",
            "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run",
            "/v",
            &runner_name,
            "/t",
            "REG_SZ",
            "/d",
            "old-runner-value",
            "/f",
        ])
        .output()?;
    if !seeded.status.success() {
        return Err("failed to seed runner test value".into());
    }
    assert!(reg_query(&runner_name), "预置 runner 值应存在");

    let svc = DesktopService::open_with_autostart_names(
        db.clone(),
        desktop_name.clone(),
        runner_name.clone(),
    )
    .await?;

    let mut settings = default_settings(&download);
    settings.autostart = true;
    svc.set_settings(settings.clone()).await?;
    assert!(reg_query(&desktop_name), "开启后应写入桌面启动值");
    assert!(!reg_query(&runner_name), "开启后应删除旧 runner 值");
    assert!(
        svc.get_settings().await?.autostart,
        "get_settings 应反映已开启"
    );

    settings.autostart = false;
    svc.set_settings(settings).await?;
    assert!(!reg_query(&desktop_name), "关闭后应删除桌面启动值");

    svc.shutdown().await?;
    Ok(())
}

// --- finished_since：完成/失败通知的纯函数判定 ------------------------

fn view(id: i64, state: &str, name: &str) -> TaskView {
    TaskView {
        id,
        kind: idm_desktop::service::classify(name).to_string(),
        file_name: Some(name.to_string()),
        url_display: format!("https://example.com/{name}"),
        total_size: None,
        downloaded: 0,
        speed_bps: 0,
        state: state.to_string(),
        pause_reason: None,
        last_error: None,
    }
}

#[test]
fn finished_since_detects_new_terminal_states() {
    use idm_desktop::service::finished_since;

    // prev：1 排队、2 下载中、3 已完成、4 暂停；5/6 尚不存在。
    let prev = vec![
        view(1, "Queued", "a.bin"),
        view(2, "Downloading", "b.bin"),
        view(3, "Completed", "c.bin"),
        view(4, "Paused", "d.bin"),
    ];
    // now：1 完成、2 失败、3 仍完成（不重复报）、4 仍暂停（不报）、
    //      5 新出现且完成（不报）、6 新出现失败（不报）。
    let now = vec![
        view(1, "Completed", "a.bin"),
        view(2, "Failed", "b.bin"),
        view(3, "Completed", "c.bin"),
        view(4, "Paused", "d.bin"),
        view(5, "Completed", "e.bin"),
        view(6, "Failed", "f.bin"),
    ];
    let got = finished_since(&prev, &now);
    assert_eq!(got.len(), 2, "只应报两条：{got:?}");
    assert_eq!(got[0], (1, "a.bin".to_string(), true), "排队→完成");
    assert_eq!(got[1], (2, "b.bin".to_string(), false), "下载中→失败");

    // 空列表场景。
    assert!(finished_since(&[], &[]).is_empty(), "两空列表应为空");
    assert!(finished_since(&prev, &[]).is_empty(), "now 为空应为空");
}

#[tokio::test]
async fn notify_setting_roundtrip() -> TestResult {
    let tmp = TempDir::new()?;
    let db = tmp.join("idm.db");
    let download = tmp.join("downloads");
    std::fs::create_dir_all(&download)?;
    seed_settings(&db, &download, None).await?;

    let svc = DesktopService::open(db.clone()).await?;
    // 默认值为真。
    assert!(
        svc.get_settings().await?.notify_on_complete,
        "notify-on-complete 默认为真"
    );

    // 写 false 读回 false，再写 true 读回 true。
    let mut settings = default_settings(&download);
    settings.notify_on_complete = false;
    svc.set_settings(settings.clone()).await?;
    assert!(
        !svc.get_settings().await?.notify_on_complete,
        "写入 false 后应读回 false"
    );

    settings.notify_on_complete = true;
    svc.set_settings(settings).await?;
    assert!(
        svc.get_settings().await?.notify_on_complete,
        "写入 true 后应读回 true"
    );

    svc.shutdown().await?;
    Ok(())
}

// --- classify：任务类型分类 ------------------------------------------

#[test]
fn classify_groups_by_extension() {
    use idm_desktop::service::classify;

    let cases: &[(&str, &str)] = &[
        // archive
        ("setup.zip", "archive"),
        ("disc.iso", "archive"),
        ("bash-5.2.tar.gz", "archive"),
        ("data.TAR.GZ", "archive"),
        // video
        ("movie.mp4", "video"),
        ("clip.WEBM", "video"),
        // audio
        ("song.mp3", "audio"),
        ("tune.FLAC", "audio"),
        // document
        ("report.pdf", "document"),
        ("notes.DOCX", "document"),
        // program
        ("app.exe", "program"),
        ("pkg.MSI", "program"),
        // other
        ("README", "other"),
        ("image.png", "other"),
    ];
    for (name, expected) in cases {
        assert_eq!(classify(name), *expected, "name={name}");
    }
}

// --- task_detail：分块详情（只读，慢速 4 连接）--------------------------

#[tokio::test]
async fn task_detail_reports_chunks() -> TestResult {
    let size = 8 * MIB;
    let srv = start(ServerConfig {
        size,
        speed: Some(SpeedPolicy(Arc::new(|_, _| Some(512 * KIB)))),
        ..ServerConfig::default()
    })
    .await?;

    let tmp = TempDir::new()?;
    let db = tmp.join("idm.db");
    let download = tmp.join("downloads");
    std::fs::create_dir_all(&download)?;
    seed_settings(&db, &download, None).await?;

    let svc = DesktopService::open(db.clone()).await?;
    let id = svc.add_task(srv.url("/detail.bin"), Some(4)).await?;

    // 等待下载真正开始且有一定进度。
    wait_for(
        &svc,
        |v| {
            v.iter()
                .any(|t| t.id == id && t.state == "Downloading" && t.downloaded > 0)
        },
        Duration::from_secs(20),
    )
    .await?;

    let detail = svc.task_detail(id).await?;
    assert_eq!(detail.id, id);
    assert!(
        detail.chunks.len() >= 4,
        "4 连接应至少 4 个分块：{}",
        detail.chunks.len()
    );

    // 分块范围首尾相接、覆盖整个文件且不重叠（end 为包含式）。
    let mut ranges: Vec<(u64, u64)> = detail
        .chunks
        .iter()
        .map(|c| (c.start, c.end.saturating_add(1)))
        .collect();
    ranges.sort();
    let mut expected = 0u64;
    for (start, end_exclusive) in ranges {
        assert_eq!(start, expected, "分块应首尾相接");
        assert!(end_exclusive >= start, "分块区间不得倒置");
        expected = end_exclusive;
    }
    assert_eq!(expected, size, "分块应覆盖整个文件");

    // 各块 downloaded 之和与列表里的 downloaded 相差不超过 1 MiB。
    let chunk_sum: u64 = detail.chunks.iter().map(|c| c.downloaded).sum();
    let listed = svc
        .list_tasks()
        .await?
        .into_iter()
        .find(|t| t.id == id)
        .map(|t| t.downloaded)
        .ok_or("task missing in list")?;
    let diff = chunk_sum.abs_diff(listed);
    assert!(diff <= MIB, "分块之和与列表下载量相差过大: {diff}");

    // 完成后所有块为 done。
    wait_for(
        &svc,
        |v| v.iter().any(|t| t.id == id && t.state == "Completed"),
        Duration::from_secs(60),
    )
    .await?;
    let final_detail = svc.task_detail(id).await?;
    assert!(
        final_detail.chunks.iter().all(|c| c.state == "done"),
        "完成后所有块应为 done"
    );

    srv.shutdown();
    svc.shutdown().await?;
    Ok(())
}

/// Phase 5b-4c：暂停任务的分块详情不得显示 active/queued。
/// 暂停后队列残留的旧快照或直接读 DB，都不应把未下完的块显示为下载中。
#[tokio::test]
async fn paused_task_chunks_not_shown_downloading() -> TestResult {
    let tmp = TempDir::new()?;
    let size = 8 * MIB;
    // 慢速 + 4 连接：暂停时多个块处于部分下载（committed > start）。
    let srv = start(ServerConfig {
        size,
        speed: Some(SpeedPolicy(Arc::new(|_, _| Some(256 * KIB)))),
        ..ServerConfig::default()
    })
    .await?;
    let db = tmp.join("idm.db");
    let download = tmp.join("downloads");
    std::fs::create_dir_all(&download)?;
    seed_settings(&db, &download, None).await?;

    let svc = DesktopService::open(db.clone()).await?;
    let id = svc.add_task(srv.url("/paused.bin"), Some(4)).await?;
    wait_for(
        &svc,
        |v| v.iter().any(|t| t.id == id && t.downloaded > 0),
        Duration::from_secs(20),
    )
    .await?;
    svc.pause_task(id).await?;
    wait_for(
        &svc,
        |v| {
            v.iter().any(|t| {
                t.id == id && t.state == "Paused" && t.pause_reason.as_deref() == Some("User")
            })
        },
        Duration::from_secs(10),
    )
    .await?;

    // 暂停确认后再取详情：不应有任何分块为 active 或 queued。
    let detail = svc.task_detail(id).await?;
    println!(
        "PAUSED_CHUNKS states={:?}",
        detail
            .chunks
            .iter()
            .map(|c| c.state.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        detail
            .chunks
            .iter()
            .all(|c| c.state != "active" && c.state != "queued"),
        "暂停任务的分块不得显示 active/queued，实际 {:?}",
        detail
            .chunks
            .iter()
            .map(|c| c.state.as_str())
            .collect::<Vec<_>>()
    );

    srv.shutdown();
    svc.shutdown().await?;
    Ok(())
}

/// Phase 5b-4c：全局限速 2 MiB/s 时，界面报告的平均速度与瞬时速度都不得超额。
#[tokio::test]
async fn reported_speed_respects_global_limit() -> TestResult {
    let tmp = TempDir::new()?;
    const MIB: u64 = 1024 * 1024;
    let limit = 2 * MIB;
    let size = 24 * MIB;
    let srv = start(ServerConfig {
        size,
        ..ServerConfig::default()
    })
    .await?;
    let db = tmp.join("idm.db");
    let download = tmp.join("downloads");
    std::fs::create_dir_all(&download)?;
    seed_settings(&db, &download, Some(limit)).await?;

    let svc = DesktopService::open(db.clone()).await?;
    let id = svc.add_task(srv.url("/speed.bin"), Some(8)).await?;

    let started = Instant::now();
    let mut samples: Option<(u64, u64)> = None;
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let tasks = svc.list_tasks().await?;
        let Some(view) = tasks.iter().find(|t| t.id == id) else {
            continue;
        };
        samples = Some(match samples {
            None => (view.downloaded, view.downloaded),
            Some((first, _)) => (first, view.downloaded),
        });
        let elapsed = started.elapsed().as_secs_f64();
        if elapsed >= 2.0 && view.speed_bps > 3 * MIB {
            return Err(format!(
                "第 2 秒后瞬时速度 {} B/s 超过 3 MiB/s（t={elapsed:.2}s）",
                view.speed_bps
            )
            .into());
        }
        if view.state == "Completed" || elapsed > 40.0 {
            break;
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    let (first_downloaded, last_downloaded) = samples.unwrap_or((0, 0));
    let avg = (last_downloaded.saturating_sub(first_downloaded)) as f64 / elapsed.max(0.001);
    let limit_f = limit as f64;
    println!(
        "SPEED avg={avg:.0} B/s limit={limit} downloaded={last_downloaded} elapsed={elapsed:.2}s"
    );
    assert!(
        avg <= limit_f * 1.2,
        "全程平均速度 {avg:.0} B/s 超过 2.4 MiB/s"
    );

    srv.shutdown();
    svc.shutdown().await?;
    Ok(())
}

// --- Phase 5b-4d：继续下载时使用当前默认连接数 --------------------------

/// 设置项持久化：默认为真，显式写假后读回为假。
#[tokio::test]
async fn resume_connections_setting_roundtrip() -> TestResult {
    let tmp = TempDir::new()?;
    let db = tmp.join("idm.db");
    let download = tmp.join("downloads");
    std::fs::create_dir_all(&download)?;
    seed_settings(&db, &download, None).await?;

    let svc = DesktopService::open(db).await?;
    let initial = svc.get_settings().await?;
    assert!(
        initial.resume_uses_current_connections,
        "resume-uses-current-connections 默认为真"
    );

    let mut changed = initial;
    changed.resume_uses_current_connections = false;
    svc.set_settings(changed).await?;

    let reread = svc.get_settings().await?;
    assert!(
        !reread.resume_uses_current_connections,
        "写入假后读回应为假"
    );
    svc.shutdown().await?;
    Ok(())
}

/// 调小：以 8 连接创建并暂停，把默认连接数改为 2 后继续。
/// DB connections 应变为 2；继续后同时下载的分段数 ≤ 2；分段总数不变；文件正确。
#[tokio::test]
async fn resume_lowers_connections() -> TestResult {
    let tmp = TempDir::new()?;
    let size = 8 * MIB;
    let srv = start(ServerConfig {
        size,
        // 每连接 256 KiB/s：暂停前 8 路 ≈2 MiB/s，继续后 2 路 ≈512 KiB/s，
        // 使继续阶段有足够长的窗口采样活跃分段数。
        speed: Some(SpeedPolicy(Arc::new(|_, _| Some(256 * KIB)))),
        ..ServerConfig::default()
    })
    .await?;
    let db = tmp.join("idm.db");
    let download = tmp.join("downloads");
    std::fs::create_dir_all(&download)?;
    seed_settings(&db, &download, None).await?;

    let svc = DesktopService::open(db).await?;
    let id = svc.add_task(srv.url("/lower.bin"), Some(8)).await?;
    wait_for(
        &svc,
        |v| v.iter().any(|t| t.id == id && t.downloaded > 0),
        Duration::from_secs(20),
    )
    .await?;

    let segments_before = svc.task_detail(id).await?.chunks.len();
    assert!(
        segments_before >= 8,
        "初始应至少 8 段，实际 {segments_before}"
    );

    svc.pause_task(id).await?;
    wait_for(
        &svc,
        |v| v.iter().any(|t| t.id == id && t.state == "Paused"),
        Duration::from_secs(10),
    )
    .await?;

    let mut settings = svc.get_settings().await?;
    settings.default_connections = 2;
    svc.set_settings(settings).await?;

    svc.resume_task(id).await?;

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut samples = 0usize;
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let detail = svc.task_detail(id).await?;
        assert_eq!(
            detail.connections, 2,
            "继续后 DB 中任务 connections 应变为 2"
        );
        let active = detail.chunks.iter().filter(|c| c.state == "active").count();
        println!(
            "LOWER_CONN active={active} segments={} connections={}",
            detail.chunks.len(),
            detail.connections
        );
        assert!(active <= 2, "同时下载的分段数 {active} 不得超过 2");
        assert_eq!(
            detail.max_concurrent, 2,
            "详情面板 max_concurrent 应为 min(任务 connections=2, 每主机上限=8)"
        );
        assert!(
            detail.downloading <= 2,
            "详情面板 downloading 不得超过 2，实际 {}",
            detail.downloading
        );
        assert_eq!(
            detail.segments, segments_before,
            "详情面板分段总数应与暂停前相同"
        );
        assert_eq!(
            detail.chunks.len(),
            segments_before,
            "分段总数不得变化（不得重新切分）"
        );
        samples += 1;
        let completed = svc
            .list_tasks()
            .await?
            .iter()
            .any(|t| t.id == id && t.state == "Completed");
        if completed {
            break;
        }
    }
    assert!(samples >= 2, "观测窗口内应至少有 2 次采样");
    wait_for(
        &svc,
        |v| v.iter().any(|t| t.id == id && t.state == "Completed"),
        Duration::from_secs(120),
    )
    .await?;
    verify_bytes(&download.join("lower.bin"), size)?;

    srv.shutdown();
    svc.shutdown().await?;
    Ok(())
}

/// 调大：以 2 连接创建并暂停，把默认连接数改为 6 后继续。
/// DB connections 应变为 6；8 秒内同时下载的分段数达到 3 以上且不超过 6；文件正确。
#[tokio::test]
async fn resume_raises_connections() -> TestResult {
    let tmp = TempDir::new()?;
    let size = 64 * MIB;
    let srv = start(ServerConfig {
        size,
        speed: Some(SpeedPolicy(Arc::new(|_, _| Some(512 * KIB)))),
        ..ServerConfig::default()
    })
    .await?;
    let db = tmp.join("idm.db");
    let download = tmp.join("downloads");
    std::fs::create_dir_all(&download)?;
    seed_settings(&db, &download, None).await?;

    let svc = DesktopService::open(db).await?;
    let id = svc.add_task(srv.url("/raise.bin"), Some(2)).await?;
    wait_for(
        &svc,
        |v| v.iter().any(|t| t.id == id && t.downloaded > 0),
        Duration::from_secs(20),
    )
    .await?;
    svc.pause_task(id).await?;
    wait_for(
        &svc,
        |v| v.iter().any(|t| t.id == id && t.state == "Paused"),
        Duration::from_secs(10),
    )
    .await?;

    let mut settings = svc.get_settings().await?;
    settings.default_connections = 6;
    svc.set_settings(settings).await?;

    svc.resume_task(id).await?;

    let mut saw_above_three = false;
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let detail = svc.task_detail(id).await?;
        assert_eq!(
            detail.connections, 6,
            "继续后 DB 中任务 connections 应变为 6"
        );
        let active = detail.chunks.iter().filter(|c| c.state == "active").count();
        println!(
            "RAISE_CONN active={active} segments={}",
            detail.chunks.len()
        );
        assert!(active <= 6, "同时下载的分段数 {active} 不得超过 6");
        if active >= 3 {
            saw_above_three = true;
        }
    }
    assert!(saw_above_three, "调大后同时下载的分段数必须曾达到 3 以上");
    wait_for(
        &svc,
        |v| v.iter().any(|t| t.id == id && t.state == "Completed"),
        Duration::from_secs(180),
    )
    .await?;
    verify_bytes(&download.join("raise.bin"), size)?;

    srv.shutdown();
    svc.shutdown().await?;
    Ok(())
}

/// 关闭选项：继续时不得改动任务的 connections。
#[tokio::test]
async fn resume_keeps_connections_when_disabled() -> TestResult {
    let tmp = TempDir::new()?;
    let size = 32 * MIB;
    let srv = start(ServerConfig {
        size,
        speed: Some(SpeedPolicy(Arc::new(|_, _| Some(512 * KIB)))),
        ..ServerConfig::default()
    })
    .await?;
    let db = tmp.join("idm.db");
    let download = tmp.join("downloads");
    std::fs::create_dir_all(&download)?;
    seed_settings(&db, &download, None).await?;

    let svc = DesktopService::open(db).await?;
    let mut settings = svc.get_settings().await?;
    settings.resume_uses_current_connections = false;
    svc.set_settings(settings).await?;

    let id = svc.add_task(srv.url("/keep.bin"), Some(8)).await?;
    wait_for(
        &svc,
        |v| v.iter().any(|t| t.id == id && t.downloaded > 0),
        Duration::from_secs(20),
    )
    .await?;
    svc.pause_task(id).await?;
    wait_for(
        &svc,
        |v| v.iter().any(|t| t.id == id && t.state == "Paused"),
        Duration::from_secs(10),
    )
    .await?;

    let mut settings = svc.get_settings().await?;
    settings.default_connections = 2;
    svc.set_settings(settings).await?;

    svc.resume_task(id).await?;

    let detail = svc.task_detail(id).await?;
    assert_eq!(
        detail.connections, 8,
        "选项关闭时继续下载不得改动任务 connections"
    );

    wait_for(
        &svc,
        |v| v.iter().any(|t| t.id == id && t.state == "Completed"),
        Duration::from_secs(120),
    )
    .await?;
    verify_bytes(&download.join("keep.bin"), size)?;

    srv.shutdown();
    svc.shutdown().await?;
    Ok(())
}
