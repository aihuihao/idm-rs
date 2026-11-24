//! idm-cli: 验证工具。
//!
//! 本程序仅调用 `idm-core` 的公开 API，不复制任何核心逻辑。Phase 1-3 通过它执行
//! 真实下载、续传与压力测试；本阶段额外提供 `bench-alloc` 子命令对 Windows 文件
//! 空间分配策略做实测。
//!
//! 核心库不初始化全局 tracing subscriber；由本可执行程序负责。

#[cfg(windows)]
use std::sync::Arc;

use idm_core::core_version;
#[cfg(windows)]
use idm_core::store::{Store, default_db_path};
use tracing::info;

#[cfg(windows)]
mod bench {
    use std::path::Path;

    use idm_core::disk::{
        AllocationStrategy, SpaceCheck, TargetFile, disk_free_space, volume_filesystem,
    };
    use idm_core::error::Error;

    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;

    fn usage() -> String {
        "usage: idm-cli bench-alloc --dir <DIR> --size-gb <N> [--runs 3] [--skip-precheck]"
            .to_string()
    }

    /// 返回进程退出码：0 成功，2 参数非法，1 前置检查/运行期错误。
    pub fn run(args: &[String]) -> i32 {
        let mut dir: Option<String> = None;
        let mut size_gb: Option<u64> = None;
        let mut runs: u64 = 3;
        let mut skip_precheck = false;

        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--dir" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for --dir");
                        return 2;
                    }
                    dir = Some(args[i].clone());
                }
                "--size-gb" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for --size-gb");
                        return 2;
                    }
                    match args[i].parse::<u64>() {
                        Ok(v) => size_gb = Some(v),
                        Err(_) => {
                            eprintln!("invalid --size-gb");
                            return 2;
                        }
                    }
                }
                "--runs" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for --runs");
                        return 2;
                    }
                    match args[i].parse::<u64>() {
                        Ok(v) => runs = v,
                        Err(_) => {
                            eprintln!("invalid --runs");
                            return 2;
                        }
                    }
                }
                "--skip-precheck" => skip_precheck = true,
                other => {
                    eprintln!("unknown argument: {other}\n{}", usage());
                    return 2;
                }
            }
            i += 1;
        }

        let dir = match dir {
            Some(d) => d,
            None => {
                eprintln!("missing --dir\n{}", usage());
                return 2;
            }
        };
        let size_gb = match size_gb {
            Some(s) => s,
            None => {
                eprintln!("missing --size-gb\n{}", usage());
                return 2;
            }
        };

        let path = Path::new(&dir);
        if let Err(e) = std::fs::create_dir_all(path) {
            eprintln!("cannot create dir {dir}: {e}");
            return 2;
        }

        // 运行前检查目标卷为 NTFS。
        match volume_filesystem(path) {
            Ok(fs) if fs.eq_ignore_ascii_case("NTFS") => {}
            Ok(fs) => {
                eprintln!("target volume filesystem is '{fs}', not NTFS; aborting");
                return 1;
            }
            Err(e) => {
                eprintln!("cannot query volume filesystem: {e}");
                return 1;
            }
        }

        if skip_precheck {
            run_skip_precheck(path, size_gb, runs);
            return 0;
        }
        run_normal(path, size_gb, runs);
        0
    }

    fn run_normal(path: &Path, size_gb: u64, runs: u64) {
        let size = size_gb * GB;
        for strategy in [
            AllocationStrategy::SetLen,
            AllocationStrategy::Reserve,
            AllocationStrategy::Sparse,
        ] {
            println!("STRATEGY {strategy:?} size_gb={size_gb} runs={runs}");
            for run in 0..runs {
                let file_path = path.join(format!("bench_{strategy:?}_{run}.bin"));
                let start = std::time::Instant::now();
                let target =
                    match TargetFile::create(&file_path, size, strategy, SpaceCheck::Enforce) {
                        Ok(t) => t,
                        Err(e) => {
                            println!("  run {run} CREATE_ERROR {e}");
                            continue;
                        }
                    };
                let create_ms = start.elapsed().as_secs_f64() * 1000.0;
                let info_before = match target.allocation_info() {
                    Ok(i) => i,
                    Err(e) => {
                        println!("  run {run} ALLOC_INFO_ERROR {e}");
                        continue;
                    }
                };
                // 尾部（len - 1MB）首次写入 1MB，只计 write 调用本身。
                let tail_off = size - MB;
                let buf = vec![0u8; MB as usize];
                let tw = std::time::Instant::now();
                if let Err(e) = target.write_at(tail_off, &buf) {
                    println!("  run {run} TAIL_WRITE_ERROR {e}");
                }
                let tail_write_ms = tw.elapsed().as_secs_f64() * 1000.0;

                let ts = std::time::Instant::now();
                if let Err(e) = target.sync_data() {
                    println!("  run {run} SYNC_ERROR {e}");
                }
                let tail_sync_ms = ts.elapsed().as_secs_f64() * 1000.0;

                // 中部首次写入 1MB。
                let mid_off = size / 2;
                let mw = std::time::Instant::now();
                if let Err(e) = target.write_at(mid_off, &buf) {
                    println!("  run {run} MID_WRITE_ERROR {e}");
                }
                let mid_write_ms = mw.elapsed().as_secs_f64() * 1000.0;

                let info_after = match target.allocation_info() {
                    Ok(i) => i,
                    Err(e) => {
                        println!("  run {run} ALLOC_INFO_ERROR {e}");
                        continue;
                    }
                };
                println!(
                    "  run {run} create_ms={create_ms:.2} tail_write_ms={tail_write_ms:.2} tail_sync_ms={tail_sync_ms:.2} mid_write_ms={mid_write_ms:.2}"
                );
                println!(
                    "    before logical={} alloc={} compressed={} sparse={} | after logical={} alloc={} compressed={} sparse={}",
                    info_before.logical_len,
                    info_before.allocation_size,
                    info_before.compressed_size,
                    info_before.is_sparse,
                    info_after.logical_len,
                    info_after.allocation_size,
                    info_after.compressed_size,
                    info_after.is_sparse,
                );
                drop(target);
                let _ = std::fs::remove_file(&file_path);
            }
        }
    }

    fn run_skip_precheck(path: &Path, _size_gb: u64, runs: u64) {
        let available = match disk_free_space(path) {
            Ok(a) => a,
            Err(e) => {
                println!("DISK_FREE_ERROR {e}");
                return;
            }
        };
        // size = 可用空间 + 1GB，只做创建、不写入，观察空间不足时的系统级报错。
        // 跳过 create 内部的目标卷空间预检查（skip_space_check=true），让系统调用
        // 直接暴露原始错误码（如 112 = ERROR_DISK_FULL）。
        let size = available + GB;
        println!("SKIP_PRECHECK available_bytes={available} attempt_size_bytes={size} runs={runs}");
        for strategy in [
            AllocationStrategy::SetLen,
            AllocationStrategy::Reserve,
            AllocationStrategy::Sparse,
        ] {
            let file_path = path.join(format!("bench_skip_{strategy:?}.bin"));
            let r = TargetFile::create(&file_path, size, strategy, SpaceCheck::Skip);
            match r {
                Ok(_) => {
                    if strategy == AllocationStrategy::Sparse {
                        println!(
                            "  {strategy:?} CREATE_SUCCEEDED (expected: sparse does not pre-allocate)"
                        );
                    } else {
                        println!("  {strategy:?} CREATE_SUCCEEDED UNEXPECTED (expected DiskFull)");
                    }
                }
                Err(e) => {
                    let os_code = match &e {
                        Error::Io { source, .. } => source.raw_os_error(),
                        _ => None,
                    };
                    if strategy == AllocationStrategy::Sparse {
                        println!(
                            "  {strategy:?} CREATE_FAILED UNEXPECTED os_error={os_code:?} {e}"
                        );
                    } else if os_code == Some(112) {
                        println!(
                            "  {strategy:?} CREATE_FAILED os_error=112 (expected: ERROR_DISK_FULL)"
                        );
                    } else {
                        println!(
                            "  {strategy:?} CREATE_FAILED os_error={os_code:?} (expected 112) {e}"
                        );
                    }
                }
            }
            let _ = std::fs::remove_file(&file_path);
        }
    }
}

// --- Phase 4c: standalone runner lifecycle management -----------------------

/// `idm-cli runner install|uninstall|status|start|stop`：管理独立常驻队列运行器。
///
/// 运行器不属于浏览器进程树：开机启动项写入 HKCU Run，start 用脱离标志拉起，
/// stop 通过数据目录的 `runner.stop` 哨兵文件请求安全停止。只用 `reg.exe` 写 HKCU。
#[cfg(windows)]
mod runner {
    use idm_core::autostart;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant};

    const DEFAULT_NAME: &str = "idm-rs-runner";

    // Windows 进程创建标志。
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    /// Windows `ERROR_ACCESS_DENIED`(5)：作业对象拒绝 breakaway。
    const ERROR_ACCESS_DENIED: i32 = 5;
    /// Windows `ERROR_SHARING_VIOLATION`(32)：锁被另一个进程独占持有。
    const ERROR_SHARING_VIOLATION: i32 = 32;

    fn usage() -> &'static str {
        "usage: idm-cli runner <install|uninstall|status|start|stop> [--name NAME] [--db PATH]"
    }

    pub fn run(args: &[String]) -> i32 {
        let sub = args.first().map(String::as_str).unwrap_or("");
        let code = match sub {
            "install" => install(&args[1..]),
            "uninstall" => uninstall(&args[1..]),
            "status" => status(&args[1..]),
            "start" => start(&args[1..]),
            "stop" => stop(&args[1..]),
            _ => {
                eprintln!("unknown runner subcommand: {sub}\n{}", usage());
                return 2;
            }
        };
        match code {
            Ok(c) => c,
            Err(e) => {
                eprintln!("runner {sub} failed: {e}");
                1
            }
        }
    }

    struct Opts {
        name: String,
        db: PathBuf,
    }

    fn parse(args: &[String]) -> Result<Opts, String> {
        let mut name = DEFAULT_NAME.to_string();
        let mut db: Option<PathBuf> = None;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--name" => {
                    i += 1;
                    if i >= args.len() {
                        return Err("missing value for --name".to_string());
                    }
                    name = args[i].clone();
                }
                "--db" => {
                    i += 1;
                    if i >= args.len() {
                        return Err("missing value for --db".to_string());
                    }
                    db = Some(PathBuf::from(&args[i]));
                }
                other => return Err(format!("unknown argument: {other}\n{}", usage())),
            }
            i += 1;
        }
        validate_name(&name)?;
        let db_path = absolute(&match db {
            Some(p) => p,
            None => match std::env::var_os("IDM_RS_DB") {
                Some(v) if !v.is_empty() => PathBuf::from(v),
                _ => idm_core::store::default_db_path().map_err(|e| e.to_string())?,
            },
        })?;
        Ok(Opts { name, db: db_path })
    }

    /// 值名校验委托给 idm-core::autostart，防止注入 reg.exe 参数。
    fn validate_name(s: &str) -> Result<(), String> {
        autostart::validate_name(s)
    }

    fn absolute(p: &Path) -> Result<PathBuf, String> {
        if p.is_absolute() {
            Ok(p.to_path_buf())
        } else {
            let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
            Ok(cwd.join(p))
        }
    }

    fn lock_path(db: &Path) -> PathBuf {
        db.parent()
            .unwrap_or_else(|| Path::new("."))
            .join("runner.lock")
    }

    fn stop_path(db: &Path) -> PathBuf {
        db.parent()
            .unwrap_or_else(|| Path::new("."))
            .join("runner.stop")
    }

    fn current_exe() -> Result<PathBuf, String> {
        absolute(&std::env::current_exe().map_err(|e| e.to_string())?)
    }

    /// 以独占方式打开 `runner.lock` 判断运行器是否在运行。
    /// 能独占打开（后立即关闭）= 未运行；共享冲突(32) = 运行中。
    fn runner_running(db: &Path) -> bool {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(false)
            .share_mode(0)
            .open(lock_path(db))
        {
            Ok(f) => {
                drop(f);
                false
            }
            Err(e) => e.raw_os_error() == Some(ERROR_SHARING_VIOLATION),
        }
    }

    /// 开机启动项的值：`"<idm-cli 绝对路径>" queue run --watch --db "<db 绝对路径>"`。
    fn run_value(exe: &Path, db: &Path) -> String {
        format!(
            "\"{}\" queue run --watch --db \"{}\"",
            exe.display(),
            db.display()
        )
    }

    fn install(args: &[String]) -> Result<i32, String> {
        let opts = parse(args)?;
        let exe = current_exe()?;
        let value = run_value(&exe, &opts.db);
        autostart::install(&opts.name, &value)?;
        println!("runner autostart installed: {} = {}", opts.name, value);
        Ok(0)
    }

    fn uninstall(args: &[String]) -> Result<i32, String> {
        let opts = parse(args)?;
        autostart::uninstall(&opts.name)?;
        println!("runner autostart uninstalled: {}", opts.name);
        Ok(0)
    }

    fn status(args: &[String]) -> Result<i32, String> {
        let opts = parse(args)?;
        let raw = autostart::raw_query(&opts.name);
        let registered = raw.is_some();
        println!("value_name: {}", opts.name);
        println!("registry_key: {}", autostart::REG_RUN);
        println!("autostart_registered: {registered}");
        if registered {
            println!("registry_value: {}", raw.unwrap_or_default());
        }
        let lock = lock_path(&opts.db);
        println!("runner_lock: {}", lock.display());
        println!("runner_running: {}", runner_running(&opts.db));
        Ok(0)
    }

    /// 用指定标志拉起运行器。返回 spawn 的 IO 结果。
    fn spawn_with_flags(exe: &Path, db: &Path, breakaway: bool) -> std::io::Result<()> {
        let mut flags = DETACHED_PROCESS | CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
        if breakaway {
            flags |= CREATE_BREAKAWAY_FROM_JOB;
        }
        let db_str = db.to_string_lossy().into_owned();
        Command::new(exe)
            .args(["queue", "run", "--watch", "--db", &db_str])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(flags)
            .spawn()
            .map(|_| ())
    }

    fn start(args: &[String]) -> Result<i32, String> {
        let opts = parse(args)?;
        if runner_running(&opts.db) {
            println!("runner already running");
            return Ok(0);
        }
        // 切断继承链：把本进程的 stdin/stdout/stderr 标记为不可继承，
        // 否则分离的运行器会继承启动它的 shell/测试框架的捕获管道，
        // 导致 `Command::output()` / `2>&1 | Out-String` 一直等待到运行器退出。
        if let Err(e) = idm_core::disk::win32::set_std_handles_non_inheritable() {
            return Err(format!("failed to clear std handle inheritance: {e}"));
        }
        let exe = current_exe()?;
        // 先尝试带 CREATE_BREAKAWAY_FROM_JOB 脱离浏览器作业对象；若被拒绝(5)去掉重试。
        match spawn_with_flags(&exe, &opts.db, true) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
                spawn_with_flags(&exe, &opts.db, false)
                    .map_err(|e| format!("spawn without breakaway failed: {e}"))?;
            }
            Err(e) => return Err(format!("spawn failed: {e}")),
        }
        // 3 秒内确认锁被占用。
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if runner_running(&opts.db) {
                println!("runner started");
                return Ok(0);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err("runner did not acquire runner.lock within 3s".to_string())
    }

    fn stop(args: &[String]) -> Result<i32, String> {
        let opts = parse(args)?;
        if !runner_running(&opts.db) {
            println!("runner not running");
            let _ = std::fs::remove_file(stop_path(&opts.db));
            return Ok(0);
        }
        let sp = stop_path(&opts.db);
        std::fs::write(&sp, b"stop").map_err(|e| format!("write runner.stop failed: {e}"))?;
        println!("runner.stop written: {}", sp.display());
        // 最多 10 秒等待锁释放。
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if !runner_running(&opts.db) {
                println!("runner stopped");
                return Ok(0);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Err("runner did not release runner.lock within 10s".to_string())
    }
}

// --- Phase 4b: Firefox Native Messaging host registration -------------------

/// `idm-cli nm install|uninstall|status`：注册 / 注销 / 查询 Native Messaging host。
///
/// 注册表与 manifest 的读写逻辑已上移到 idm-core::nm_host；本模块只保留参数
/// 解析、路径计算、输出打印与退出码，保证 CLI 行为与输出完全不变。
#[cfg(windows)]
mod nm {
    use std::path::{Path, PathBuf};

    use idm_core::nm_host;

    fn usage() -> &'static str {
        "usage: idm-cli nm <install|uninstall|status> [--host-name NAME] [--extension-id ID] [--db PATH]"
    }

    pub fn run(args: &[String]) -> i32 {
        let sub = args.first().map(String::as_str).unwrap_or("");
        let code = match sub {
            "install" => install(&args[1..]),
            "uninstall" => uninstall(&args[1..]),
            "status" => status(&args[1..]),
            _ => {
                eprintln!("unknown nm subcommand: {sub}\n{}", usage());
                return 2;
            }
        };
        match code {
            Ok(c) => c,
            Err(e) => {
                eprintln!("nm {sub} failed: {e}");
                1
            }
        }
    }

    struct Opts {
        host: String,
        ext_id: Option<String>,
        db: Option<PathBuf>,
    }

    fn parse(args: &[String]) -> Result<Opts, String> {
        let mut host = nm_host::DEFAULT_HOST.to_string();
        let mut ext_id: Option<String> = None;
        let mut db: Option<PathBuf> = None;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--host-name" => {
                    i += 1;
                    if i >= args.len() {
                        return Err("missing value for --host-name".to_string());
                    }
                    host = args[i].clone();
                }
                "--extension-id" => {
                    i += 1;
                    if i >= args.len() {
                        return Err("missing value for --extension-id".to_string());
                    }
                    ext_id = Some(args[i].clone());
                }
                "--db" => {
                    i += 1;
                    if i >= args.len() {
                        return Err("missing value for --db".to_string());
                    }
                    db = Some(PathBuf::from(&args[i]));
                }
                other => return Err(format!("unknown argument: {other}\n{}", usage())),
            }
            i += 1;
        }
        nm_host::validate_host_name(&host)?;
        if let Some(id) = ext_id.as_deref() {
            nm_host::validate_ext_id(id)?;
        }
        Ok(Opts { host, ext_id, db })
    }

    fn absolute(p: &Path) -> Result<PathBuf, String> {
        if p.is_absolute() {
            Ok(p.to_path_buf())
        } else {
            let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
            Ok(cwd.join(p))
        }
    }

    /// 数据目录 = 数据库文件所在目录（`--db` > `IDM_RS_DB` > 默认数据库路径）。
    fn data_dir(db: Option<&Path>) -> Result<PathBuf, String> {
        let db_path = match db {
            Some(p) => p.to_path_buf(),
            None => match std::env::var_os("IDM_RS_DB") {
                Some(v) if !v.is_empty() => PathBuf::from(v),
                _ => idm_core::store::default_db_path().map_err(|e| e.to_string())?,
            },
        };
        let parent = db_path
            .parent()
            .ok_or_else(|| "db path has no parent directory".to_string())?;
        absolute(parent)
    }

    /// host 二进制与 idm-cli.exe 同目录；install 前必须真实存在。
    fn host_exe() -> Result<PathBuf, String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let dir = exe
            .parent()
            .ok_or_else(|| "cannot resolve exe directory".to_string())?;
        absolute(&dir.join("idm-nmhost.exe"))
    }

    fn install(args: &[String]) -> Result<i32, String> {
        let opts = parse(args)?;
        let id = opts
            .ext_id
            .clone()
            .ok_or_else(|| "install requires --extension-id <ID>".to_string())?;
        let exe = host_exe()?;
        let dir = data_dir(opts.db.as_deref())?;

        nm_host::install(&opts.host, &exe, &[id], &dir)?;

        let manifest = nm_host::manifest_path(&dir, &opts.host);
        let key = nm_host::reg_key(&opts.host);
        println!("installed native messaging host '{}'", opts.host);
        println!("manifest: {}", manifest.display());
        println!("registry: {key}");
        println!("next: run 'idm-cli runner install' then 'idm-cli runner start'");
        Ok(0)
    }

    fn uninstall(args: &[String]) -> Result<i32, String> {
        let opts = parse(args)?;
        let dir = data_dir(opts.db.as_deref())?;
        let manifest = nm_host::manifest_path(&dir, &opts.host);

        nm_host::uninstall(&opts.host, &dir)?;

        println!("uninstalled native messaging host '{}'", opts.host);
        println!("manifest_removed: {}", !manifest.exists());
        Ok(0)
    }

    fn status(args: &[String]) -> Result<i32, String> {
        let opts = parse(args)?;
        let dir = data_dir(opts.db.as_deref())?;
        let exe = host_exe()?;

        let s = nm_host::status_in(&opts.host, &dir, &exe)?;
        println!("host_name: {}", s.host_name);
        println!("manifest: {}", s.manifest_path.display());
        println!("manifest_exists: {}", s.manifest_exists);
        println!("host_path: {}", s.host_path.display());
        println!("host_path_exists: {}", s.host_path_exists);
        println!("registry_key: {}", s.registry_key);
        println!("registry_exists: {}", s.registered);
        if let Some(ref value) = s.registry_value {
            println!("registry_value: {value}");
        }
        println!("allowed_extensions: {}", s.allowed_extensions);
        Ok(0)
    }
}

#[cfg(windows)]
mod dl {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use idm_core::disk::{AllocationStrategy, DiskWriterPool, volume_supports_sparse};
    use idm_core::net::Url;
    use idm_core::net::client::{DownloadConfig, ProxyConfig};
    use idm_core::net::filename::{resolve_conflict, sanitize_windows_filename};
    use idm_core::net::host_limit::{DEFAULT_MAX_PER_HOST, MAX_PER_HOST_CEILING};
    use idm_core::net::parse_max_speed;
    use idm_core::net::probe::Probe;
    use idm_core::queue::{DownloadQueue, QueueConfig, QueueEvent, QueueMove};
    use idm_core::runner::{Runner, RunnerConfig, RunnerStartError};
    use idm_core::store::{NewTask, PauseReasonDb, QueuedTask, Store, TaskStateDb};
    use idm_core::task::{
        CancelSignal, ChunkState, DownloadContext, DownloadOptions, DownloadOutcome,
        ProgressSnapshot, download_with_store, resume,
    };
    use tokio::sync::watch;

    fn print_progress(snap: &ProgressSnapshot) {
        let total = snap.total_size.unwrap_or(0);
        let pct = if total > 0 {
            snap.downloaded as f64 * 100.0 / total as f64
        } else {
            0.0
        };
        let active = snap
            .chunks
            .iter()
            .filter(|c| c.state == ChunkState::Downloading)
            .count();
        println!(
            "PROGRESS {}/{} bytes ({pct:.1}%) {:.0} B/s active_chunks={active}",
            snap.downloaded, total, snap.speed
        );
    }

    /// 生成每个分块的汇总表：编号、区间、已写入字节数、耗时、平均速度、最终状态、
    /// 重试次数（Phase 1d）、来源（Phase 2：初始分块 / 从第几号分块切出）。
    /// 下载成功、取消、失败都会输出（数据源是外部进度通道的最后一份快照）。
    fn format_chunk_summary(snap: &ProgressSnapshot) -> String {
        let mut text = String::from(
            "CHUNK SUMMARY: id | range | written | elapsed | avg_speed | state | retries | source",
        );
        for c in snap.chunks.iter() {
            let source = match c.parent {
                // Phase 2.2：汇总表的 source 列区分"接管"与"普通对半切"。
                Some(parent) if c.takeover => format!("takeover_from_{parent}"),
                Some(parent) => format!("split_from_{parent}"),
                None => "initial".to_string(),
            };
            text.push_str(&format!(
                "\nCHUNK id={} range=[{}-{}] written={} elapsed={:.2}s avg_speed={:.0} B/s state={:?} retries={} source={}",
                c.db_index,
                c.start,
                c.end.saturating_sub(1),
                c.current.saturating_sub(c.start),
                c.elapsed_secs,
                c.avg_speed,
                c.state,
                c.retries,
                source
            ));
        }
        text
    }

    fn print_chunk_summary(snap: &ProgressSnapshot) {
        println!("{}", format_chunk_summary(snap));
    }

    pub async fn run(args: &[String], store: Arc<Store>) -> i32 {
        let mut url: Option<String> = None;
        let mut dir: Option<String> = None;
        let mut connections: usize = 8;
        let mut out_name: Option<String> = None;
        let mut cancel_after: Option<u64> = None;
        let mut max_speed: Option<u64> = None;
        let mut max_retries: Option<u32> = None;
        let mut checkpoint_ms: Option<u64> = None;
        // Phase 2：动态切分与工作窃取默认开启，`--no-split` 关闭。
        let mut dynamic_split = true;
        // Phase 3a：按 Host 连接上限 / 代理配置。
        let mut max_host_connections: usize = DEFAULT_MAX_PER_HOST;
        let mut proxy_url: Option<String> = None;
        let mut no_proxy = false;
        // B5：`--strategy` 默认 Auto（卷支持稀疏文件则 Sparse，否则 SetLen）。
        let mut strategy = AllocationStrategy::Auto;

        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--dir" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for --dir");
                        return 2;
                    }
                    dir = Some(args[i].clone());
                }
                "--connections" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for --connections");
                        return 2;
                    }
                    match args[i].parse::<usize>() {
                        Ok(v) if (1..=32).contains(&v) => connections = v,
                        _ => {
                            eprintln!("--connections must be in 1..=32");
                            return 2;
                        }
                    }
                }
                "--out" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for --out");
                        return 2;
                    }
                    out_name = Some(args[i].clone());
                }
                "--strategy" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for --strategy");
                        return 2;
                    }
                    strategy = match args[i].as_str() {
                        "sparse" => AllocationStrategy::Sparse,
                        "reserve" => AllocationStrategy::Reserve,
                        "setlen" => AllocationStrategy::SetLen,
                        "auto" => AllocationStrategy::Auto,
                        other => {
                            eprintln!("unknown --strategy: {other}");
                            return 2;
                        }
                    };
                }
                "--cancel-after" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for --cancel-after");
                        return 2;
                    }
                    match args[i].parse::<u64>() {
                        Ok(v) => cancel_after = Some(v),
                        _ => {
                            eprintln!("--cancel-after must be a non-negative number of seconds");
                            return 2;
                        }
                    };
                }
                "--max-speed" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for --max-speed");
                        return 2;
                    }
                    match parse_max_speed(&args[i]) {
                        Some(v) => max_speed = Some(v),
                        None => {
                            eprintln!(
                                "invalid --max-speed: {} (expected e.g. 500K, 3M, or a plain byte count)",
                                args[i]
                            );
                            return 2;
                        }
                    }
                }
                "--checkpoint-ms" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for --checkpoint-ms");
                        return 2;
                    }
                    match args[i].parse::<u64>() {
                        Ok(v) if v > 0 => checkpoint_ms = Some(v),
                        _ => {
                            eprintln!("--checkpoint-ms must be a positive integer");
                            return 2;
                        }
                    }
                }
                "--max-retries" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for --max-retries");
                        return 2;
                    }
                    match args[i].parse::<u32>() {
                        Ok(v) => max_retries = Some(v),
                        Err(_) => {
                            eprintln!("--max-retries must be a non-negative integer");
                            return 2;
                        }
                    }
                }
                "--no-split" => {
                    // Phase 2：关闭动态切分与工作窃取。
                    dynamic_split = false;
                }
                "--max-host-connections" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for --max-host-connections");
                        return 2;
                    }
                    match args[i].parse::<usize>() {
                        Ok(v) if (1..=MAX_PER_HOST_CEILING).contains(&v) => {
                            max_host_connections = v;
                        }
                        _ => {
                            eprintln!(
                                "--max-host-connections must be in 1..={MAX_PER_HOST_CEILING}"
                            );
                            return 2;
                        }
                    }
                }
                "--proxy" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for --proxy");
                        return 2;
                    }
                    proxy_url = Some(args[i].clone());
                }
                "--no-proxy" => {
                    no_proxy = true;
                }
                "--log-file" | "--db" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for {}", args[i - 1]);
                        return 2;
                    }
                    // 值已由 main() 预扫描并用于初始化 tracing / 打开数据库；这里只消费参数。
                }
                other if url.is_none() && !other.starts_with("--") => url = Some(other.to_string()),
                other => {
                    eprintln!("unknown argument: {other}");
                    return 2;
                }
            }
            i += 1;
        }

        if proxy_url.is_some() && no_proxy {
            eprintln!("usage error: --proxy and --no-proxy are mutually exclusive");
            return 2;
        }
        let url = match url {
            Some(u) => u,
            None => {
                eprintln!(
                    "usage: idm-cli download <URL> [--dir DIR] [--connections N] [--max-host-connections N] [--out NAME] [--strategy sparse|reserve|setlen|auto] [--cancel-after SECONDS] [--max-speed 500K|3M] [--proxy URL] [--no-proxy] [--log-file PATH]"
                );
                return 2;
            }
        };

        let dir = match dir {
            Some(d) => PathBuf::from(d),
            None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        };
        if let Err(e) = std::fs::create_dir_all(&dir) {
            crate::print_error(&format!("cannot create dir {}", dir.display()), &e);
            return 1;
        }

        // 任务记录在创建时只写 url / dir / strategy / connections / state=Pending；
        // 文件名、总长、ETag 等字段在 Probe 成功后由下载流程回填。
        let task_id = match store
            .create_task(NewTask {
                url: url.clone(),
                dir: dir.to_string_lossy().into_owned(),
                strategy: format!("{strategy:?}"),
                connections,
            })
            .await
        {
            Ok(id) => id,
            Err(e) => {
                crate::print_error("cannot create task", &e);
                return 1;
            }
        };
        println!("TASK id={task_id}");

        let parsed_url = match Url::parse(&url) {
            Ok(u) => u,
            Err(e) => {
                crate::print_error("invalid URL", &e);
                return 2;
            }
        };
        if let Err(e) = DownloadConfig::check_scheme(&parsed_url) {
            crate::print_error("invalid URL", &e);
            return 2;
        }

        let proxy = if no_proxy {
            ProxyConfig::Disabled
        } else {
            proxy_url.map_or(ProxyConfig::System, ProxyConfig::Custom)
        };
        let config = DownloadConfig {
            connections,
            max_retries: max_retries.unwrap_or(5),
            dynamic_split,
            max_connections_per_host: max_host_connections,
            proxy,
            ..DownloadConfig::default()
        };
        let client = match config.build_client() {
            Ok(c) => c,
            Err(e) => {
                crate::print_error("cannot build HTTP client", &e);
                return 1;
            }
        };

        // B5：先执行 Probe 并打印结果，再开始下载。
        let probe_result = match Probe::new(&client, &config).probe(&parsed_url).await {
            Ok(p) => p,
            Err(e) => {
                crate::print_error("probe failed", &e);
                return 1;
            }
        };
        println!(
            "PROBE final_url={} total_size={:?} supports_range={} etag={:?} last_modified={:?} filename={} content_type={:?}",
            probe_result.final_url,
            probe_result.total_size,
            probe_result.supports_range,
            probe_result.etag,
            probe_result.last_modified,
            probe_result.filename,
            probe_result.content_type
        );

        // 外部进度通道：下载开始前就订阅，实现每秒刷新。
        let (progress_tx, final_rx) = watch::channel(ProgressSnapshot::default());
        let mut live_rx = progress_tx.subscribe();
        let progress_task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    biased;
                    _ = ticker.tick() => print_progress(&live_rx.borrow()),
                    r = live_rx.changed() => {
                        if r.is_err() { break; }
                    }
                }
            }
        });

        let raw_name = match &out_name {
            Some(n) => sanitize_windows_filename(n),
            None => probe_result.filename.clone(),
        };
        let final_path = resolve_conflict(&dir, &raw_name);
        let opts = DownloadOptions {
            global_limiter: None,
            host_limiter: None,
            config,
            strategy,
            dir: dir.clone(),
            out_name,
            progress_tx: Some(progress_tx),
            max_speed,
        };

        let pool = match DiskWriterPool::new(4, 16) {
            Ok(p) => Arc::new(p),
            Err(e) => {
                crate::print_error("disk writer pool init failed", &e);
                return 1;
            }
        };

        // Ctrl+C → `CancelSignal`（watch 状态，取消不会漏掉）。
        let signal = Arc::new(CancelSignal::new());
        let signal_task = signal.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            signal_task.cancel();
        });

        // `--cancel-after <秒>`（用于测试）：到达指定秒数后，触发与 Ctrl+C 完全相同的
        // 取消路径（同一个 CancelSignal、同样的退出码、同样的输出）。不得另写一套取消逻辑。
        if let Some(secs) = cancel_after {
            let signal_timer = signal.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(secs)).await;
                signal_timer.cancel();
            });
        }

        let started = Instant::now();
        let mut ctx = DownloadContext::new(Arc::clone(&store), task_id);
        if let Some(ms) = checkpoint_ms {
            ctx.checkpoint_interval = Duration::from_millis(ms);
        }
        let result =
            download_with_store(&url, pool.clone(), &opts, Some(signal.as_ref()), Some(ctx)).await;
        let elapsed = started.elapsed();
        progress_task.abort();
        let snap = final_rx.borrow().clone();
        // A：无论成功还是失败，都打印每个分块的汇总表。
        print_chunk_summary(&snap);
        let (outcome, _rx) = match result {
            Ok(v) => v,
            Err(e) => {
                crate::print_error("download failed", &e);
                return 1;
            }
        };

        let actual_strategy = match strategy {
            AllocationStrategy::Auto => match volume_supports_sparse(&dir) {
                Ok(true) => "Sparse".to_string(),
                Ok(false) => "SetLen".to_string(),
                Err(e) => format!("Auto(undetermined: {e})"),
            },
            other => format!("{other:?}"),
        };
        let size = snap.downloaded;
        let avg_speed = if elapsed.as_secs_f64() > 0.0 {
            size as f64 / elapsed.as_secs_f64()
        } else {
            0.0
        };
        println!(
            "RESULT status={outcome:?} path={} size={size} elapsed={:.2}s avg_speed={avg_speed:.0} B/s chunks={} splits={} takeovers={} strategy={actual_strategy}",
            final_path.display(),
            elapsed.as_secs_f64(),
            snap.chunks.len(),
            snap.split_count,
            snap.takeover_count
        );

        match outcome {
            DownloadOutcome::Completed => 0,
            DownloadOutcome::Cancelled => {
                eprintln!("download cancelled; partial file retained");
                1
            }
            DownloadOutcome::Paused(reason) => {
                eprintln!("download paused: {reason:?}; partial file retained");
                1
            }
        }
    }

    /// `resume <id>`：续传（Paused / Failed / Finalizing）。
    pub async fn run_resume(args: &[String], store: Arc<Store>) -> i32 {
        let mut id: Option<i64> = None;
        let mut max_speed: Option<u64> = None;
        let mut max_retries: Option<u32> = None;
        // Phase 2：续传默认允许动态切分，`--no-split` 关闭。
        let mut dynamic_split = true;
        // Phase 3a：按 Host 连接上限 / 代理配置。
        let mut max_host_connections: usize = DEFAULT_MAX_PER_HOST;
        let mut proxy_url: Option<String> = None;
        let mut no_proxy = false;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--max-speed" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for --max-speed");
                        return 2;
                    }
                    match parse_max_speed(&args[i]) {
                        Some(v) => max_speed = Some(v),
                        None => {
                            eprintln!("invalid --max-speed: {}", args[i]);
                            return 2;
                        }
                    }
                }
                "--max-retries" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for --max-retries");
                        return 2;
                    }
                    match args[i].parse::<u32>() {
                        Ok(v) => max_retries = Some(v),
                        Err(_) => {
                            eprintln!("--max-retries must be a non-negative integer");
                            return 2;
                        }
                    }
                }
                "--no-split" => {
                    dynamic_split = false;
                }
                "--max-host-connections" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for --max-host-connections");
                        return 2;
                    }
                    match args[i].parse::<usize>() {
                        Ok(v) if (1..=MAX_PER_HOST_CEILING).contains(&v) => {
                            max_host_connections = v;
                        }
                        _ => {
                            eprintln!(
                                "--max-host-connections must be in 1..={MAX_PER_HOST_CEILING}"
                            );
                            return 2;
                        }
                    }
                }
                "--proxy" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for --proxy");
                        return 2;
                    }
                    proxy_url = Some(args[i].clone());
                }
                "--no-proxy" => {
                    no_proxy = true;
                }
                "--db" | "--log-file" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for {}", args[i - 1]);
                        return 2;
                    }
                }
                other if id.is_none() && !other.starts_with("--") => match other.parse::<i64>() {
                    Ok(v) => id = Some(v),
                    Err(_) => {
                        eprintln!("invalid task id: {other}");
                        return 2;
                    }
                },
                other => {
                    eprintln!("unknown argument: {other}");
                    return 2;
                }
            }
            i += 1;
        }
        if proxy_url.is_some() && no_proxy {
            eprintln!("usage error: --proxy and --no-proxy are mutually exclusive");
            return 2;
        }
        let id = match id {
            Some(v) => v,
            None => {
                eprintln!(
                    "usage: idm-cli resume <id> [--max-speed 3M] [--max-host-connections N] [--no-split] [--proxy URL] [--no-proxy] [--db PATH] [--log-file PATH]"
                );
                return 2;
            }
        };
        let rec = match store.get_task(id).await {
            Ok(r) => r,
            Err(e) => {
                crate::print_error("cannot read task", &e);
                return 1;
            }
        };
        let strategy = match rec.strategy.as_str() {
            "SetLen" => AllocationStrategy::SetLen,
            "Reserve" => AllocationStrategy::Reserve,
            "Sparse" => AllocationStrategy::Sparse,
            _ => AllocationStrategy::Auto,
        };
        // 外部进度通道：参照 download 命令，在续传开始前就建好并传入，
        // 以便成功或失败时都能读取最后一次快照并打印分块汇总表。
        let (progress_tx, _) = watch::channel(ProgressSnapshot::default());
        let proxy = if no_proxy {
            ProxyConfig::Disabled
        } else {
            proxy_url.map_or(ProxyConfig::System, ProxyConfig::Custom)
        };
        // Phase 4a：从数据库读取该任务的请求头（Referer / Cookie / User-Agent）并注入配置。
        let extra_headers = store.get_task_headers(rec.id).await.unwrap_or_default();
        let opts = DownloadOptions {
            global_limiter: None,
            host_limiter: None,
            config: DownloadConfig {
                connections: (rec.connections as usize).clamp(1, 32),
                max_retries: max_retries.unwrap_or(5),
                dynamic_split,
                max_connections_per_host: max_host_connections,
                proxy,
                extra_headers,
                ..DownloadConfig::default()
            },
            strategy,
            dir: PathBuf::from(&rec.dir),
            out_name: None,
            progress_tx: Some(progress_tx.clone()),
            max_speed,
        };
        let pool = match DiskWriterPool::new(4, 16) {
            Ok(p) => Arc::new(p),
            Err(e) => {
                crate::print_error("disk writer pool init failed", &e);
                return 1;
            }
        };
        let signal = Arc::new(CancelSignal::new());
        let signal_task = Arc::clone(&signal);
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            signal_task.cancel();
        });

        let started = Instant::now();
        let result = resume(id, Arc::clone(&store), pool, &opts, Some(signal.as_ref())).await;
        let elapsed = started.elapsed();
        // 无论成功还是失败，都先读取最后一次进度快照并打印分块汇总表
        // （参照 download 命令：续传期间网络失败时也必须看到 summary）。
        let snap = progress_tx.subscribe().borrow().clone();
        print_chunk_summary(&snap);
        let (outcome, _rx) = match result {
            Ok(v) => v,
            Err(e) => {
                crate::print_error("resume failed", &e);
                return 1;
            }
        };
        println!(
            "RESULT status={outcome:?} task_id={id} total={:?} downloaded_this_run={} elapsed={:.2}s chunks={} splits={} takeovers={}",
            rec.total_size,
            snap.downloaded,
            elapsed.as_secs_f64(),
            snap.chunks.len(),
            snap.split_count,
            snap.takeover_count
        );
        match outcome {
            DownloadOutcome::Completed => 0,
            DownloadOutcome::Cancelled => {
                eprintln!("resume cancelled; partial file retained");
                1
            }
            DownloadOutcome::Paused(reason) => {
                eprintln!("resume paused: {reason:?}; partial file retained");
                1
            }
        }
    }

    /// `list`：列出所有任务（ID、文件名、状态、进度、最后错误）。
    pub async fn run_list(args: &[String], store: Arc<Store>) -> i32 {
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--db" | "--log-file" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for {}", args[i - 1]);
                        return 2;
                    }
                }
                other => {
                    eprintln!("unknown argument: {other}");
                    return 2;
                }
            }
            i += 1;
        }
        let tasks = match store.list_tasks().await {
            Ok(t) => t,
            Err(e) => {
                crate::print_error("cannot list tasks", &e);
                return 1;
            }
        };
        println!("ID | QUEUE_ORDER | FILE | STATE | PAUSE | PROGRESS | LAST_ERROR");
        for t in tasks {
            let progress = match t.total_size {
                Some(n) => format!("{}/{}", t.committed, n),
                None => t.committed.to_string(),
            };
            let pause = t
                .pause_reason
                .map(|p| format!("{p:?}"))
                .unwrap_or_else(|| "-".to_string());
            println!(
                "{} | {} | {} | {:?} | {} | {} | {}",
                t.id,
                t.queue_order,
                t.file_name.as_deref().unwrap_or("-"),
                t.state,
                pause,
                progress,
                t.last_error.as_deref().unwrap_or("-")
            );
        }
        0
    }

    /// `remove <id> [--delete-file]`：删除任务记录，可选删除 .idm-part。
    pub async fn run_remove(args: &[String], store: Arc<Store>) -> i32 {
        let mut id: Option<i64> = None;
        let mut delete_file = false;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--delete-file" => delete_file = true,
                "--db" | "--log-file" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for {}", args[i - 1]);
                        return 2;
                    }
                }
                other if id.is_none() && !other.starts_with("--") => match other.parse::<i64>() {
                    Ok(v) => id = Some(v),
                    Err(_) => {
                        eprintln!("invalid task id: {other}");
                        return 2;
                    }
                },
                other => {
                    eprintln!("unknown argument: {other}");
                    return 2;
                }
            }
            i += 1;
        }
        let id = match id {
            Some(v) => v,
            None => {
                eprintln!("usage: idm-cli remove <id> [--delete-file] [--db PATH]");
                return 2;
            }
        };
        let part = match store.delete_task(id).await {
            Ok(p) => p,
            Err(e) => {
                crate::print_error("cannot delete task", &e);
                return 1;
            }
        };
        if delete_file && let Some(p) = part {
            let path = PathBuf::from(&p);
            if path.exists() {
                match std::fs::remove_file(&path) {
                    Ok(()) => println!("deleted {}", path.display()),
                    Err(e) => {
                        crate::print_error("cannot delete .idm-part", &e);
                        return 1;
                    }
                }
            }
        }
        println!("removed task {id}");
        0
    }

    /// `enqueue <URL> [--dir DIR] [--connections N] [--out NAME] [--max-speed 500K|3M] [--db PATH]`
    /// 以 `Queued` 状态入队，打印任务 ID 与 `queue_order` 后退出（不立即下载）。
    pub async fn run_enqueue(args: &[String], store: Arc<Store>) -> i32 {
        let mut url: Option<String> = None;
        let mut dir: Option<String> = None;
        let mut connections: usize = 8;
        let mut out_name: Option<String> = None;
        let mut max_speed: Option<u64> = None;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--dir" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for --dir");
                        return 2;
                    }
                    dir = Some(args[i].clone());
                }
                "--connections" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for --connections");
                        return 2;
                    }
                    match args[i].parse::<usize>() {
                        Ok(v) if (1..=32).contains(&v) => connections = v,
                        _ => {
                            eprintln!("--connections must be in 1..=32");
                            return 2;
                        }
                    }
                }
                "--out" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for --out");
                        return 2;
                    }
                    out_name = Some(args[i].clone());
                }
                "--max-speed" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for --max-speed");
                        return 2;
                    }
                    match parse_max_speed(&args[i]) {
                        Some(v) => max_speed = Some(v),
                        None => {
                            eprintln!("invalid --max-speed: {}", args[i]);
                            return 2;
                        }
                    }
                }
                "--db" | "--log-file" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for {}", args[i - 1]);
                        return 2;
                    }
                }
                other if url.is_none() && !other.starts_with("--") => url = Some(other.to_string()),
                other => {
                    eprintln!("unknown argument: {other}");
                    return 2;
                }
            }
            i += 1;
        }
        let url = match url {
            Some(u) => u,
            None => {
                eprintln!(
                    "usage: idm-cli enqueue <URL> [--dir DIR] [--connections N] \
                     [--out NAME] [--max-speed 500K|3M] [--db PATH]"
                );
                return 2;
            }
        };
        let dir = match dir {
            Some(d) => PathBuf::from(d),
            None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        };
        if let Err(e) = std::fs::create_dir_all(&dir) {
            crate::print_error("cannot create dir", &e);
            return 1;
        }
        let file_name = match out_name {
            Some(raw) => {
                // 只取路径最后一段，再交给 core 做 Windows 合法化。
                let last = match raw.rfind(['/', '\\']) {
                    Some(idx) => &raw[idx + 1..],
                    None => raw.as_str(),
                };
                if last.is_empty() {
                    eprintln!("invalid --out: empty file name");
                    return 2;
                }
                let sanitized = sanitize_windows_filename(last);
                if sanitized.is_empty() {
                    eprintln!("invalid --out: empty file name after sanitize");
                    return 2;
                }
                Some(sanitized)
            }
            None => None,
        };
        let id = match store
            .enqueue_task(
                QueuedTask {
                    url: url.clone(),
                    dir: dir.to_string_lossy().into_owned(),
                    strategy: "Auto".to_string(),
                    connections,
                    max_speed,
                    file_name,
                },
                None,
            )
            .await
        {
            Ok(id) => id,
            Err(e) => {
                crate::print_error("cannot enqueue task", &e);
                return 1;
            }
        };
        let queue_order = store.get_task(id).await.map(|r| r.queue_order).unwrap_or(0);
        println!("ENQUEUED id={id} queue_order={queue_order}");
        0
    }

    /// `queue run [--max-tasks N] [--global-max-speed 3M] [--max-host-connections N]
    /// [--proxy URL] [--no-proxy] [--db PATH]`：恢复并调度所有 `Queued` 任务直至空闲，打印
    /// 汇总后退出；支持 Ctrl+C 安全停止所有活动任务。
    pub async fn run_queue_run(args: &[String], store: Arc<Store>) -> i32 {
        let mut max_tasks: usize = 3;
        let mut global_max_speed: Option<u64> = None;
        let mut max_host_connections: usize = DEFAULT_MAX_PER_HOST;
        let mut proxy_url: Option<String> = None;
        let mut no_proxy = false;
        let mut watch = false;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--max-tasks" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for --max-tasks");
                        return 2;
                    }
                    match args[i].parse::<usize>() {
                        Ok(v) if (1..=16).contains(&v) => max_tasks = v,
                        _ => {
                            eprintln!("--max-tasks must be in 1..=16");
                            return 2;
                        }
                    }
                }
                "--global-max-speed" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for --global-max-speed");
                        return 2;
                    }
                    match parse_max_speed(&args[i]) {
                        Some(v) => global_max_speed = Some(v),
                        None => {
                            eprintln!("invalid --global-max-speed: {}", args[i]);
                            return 2;
                        }
                    }
                }
                "--max-host-connections" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for --max-host-connections");
                        return 2;
                    }
                    match args[i].parse::<usize>() {
                        Ok(v) if (1..=MAX_PER_HOST_CEILING).contains(&v) => {
                            max_host_connections = v
                        }
                        _ => {
                            eprintln!(
                                "--max-host-connections must be in 1..={MAX_PER_HOST_CEILING}"
                            );
                            return 2;
                        }
                    }
                }
                "--proxy" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for --proxy");
                        return 2;
                    }
                    proxy_url = Some(args[i].clone());
                }
                "--no-proxy" => no_proxy = true,
                "--watch" => watch = true,
                "--db" | "--log-file" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for {}", args[i - 1]);
                        return 2;
                    }
                }
                other => {
                    eprintln!("unknown argument: {other}");
                    return 2;
                }
            }
            i += 1;
        }
        if proxy_url.is_some() && no_proxy {
            eprintln!("usage error: --proxy and --no-proxy are mutually exclusive");
            return 2;
        }
        if watch {
            // Phase 5a-1：常驻运行器逻辑已抽到 idm_core::runner::Runner。
            // CLI 只负责参数解析、等待退出信号（wait_stopped 或 Ctrl+C，二者先到者）
            // 与退出码；Runner 模块本身不处理 Ctrl+C，信号监听留在这里。
            let cfg = RunnerConfig {
                db_path: store.db_path().to_path_buf(),
                max_tasks,
                host_max_connections: max_host_connections,
                proxy: if no_proxy {
                    Some(ProxyConfig::Disabled)
                } else {
                    proxy_url.map(ProxyConfig::Custom)
                },
            };
            let runner = match Runner::start(Arc::clone(&store), cfg).await {
                Ok(r) => r,
                Err(RunnerStartError::AlreadyRunning) => {
                    eprintln!("already running");
                    return 3;
                }
                Err(RunnerStartError::Other(e)) => {
                    crate::print_error("runner start failed", &e);
                    return 1;
                }
            };
            // wait_stopped()（runner.stop 哨兵）与 Ctrl+C，二者先到者触发安全停止。
            tokio::select! {
                _ = runner.wait_stopped() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
            if let Err(e) = runner.stop().await {
                crate::print_error("runner stop failed", &e);
                return 1;
            }
            println!("QUEUE WATCH STOPPED");
            0
        } else {
            let proxy = if no_proxy {
                ProxyConfig::Disabled
            } else {
                proxy_url.map_or(ProxyConfig::System, ProxyConfig::Custom)
            };
            // 单实例锁：在数据目录（数据库所在目录）下创建 runner.lock，独占打开。
            // Phase 4c：若被占用（可能与 host 的瞬时探测撞车），每 200ms 重试、共 3 秒，
            // 仍占用才以退出码 3 退出。锁句柄保持到进程退出。
            let lock_dir = store
                .db_path()
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."));
            let lock_path = lock_dir.join("runner.lock");
            let mut lock_opt = std::fs::OpenOptions::new();
            lock_opt.read(true).write(true).create(true).truncate(false);
            std::os::windows::fs::OpenOptionsExt::share_mode(&mut lock_opt, 0);
            let lock_deadline = std::time::Instant::now() + Duration::from_secs(3);
            let acquired = loop {
                match lock_opt.open(&lock_path) {
                    Ok(f) => break f,
                    Err(_) => {
                        if std::time::Instant::now() >= lock_deadline {
                            eprintln!("already running");
                            return 3;
                        }
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
            };
            let _runner_lock = acquired;
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let config = QueueConfig {
                max_concurrent_tasks: max_tasks,
                global_max_speed,
                default_download_config: DownloadConfig {
                    max_connections_per_host: max_host_connections,
                    proxy,
                    ..DownloadConfig::default()
                },
                default_strategy: AllocationStrategy::Auto,
            };
            let pool = match DiskWriterPool::new(8, 64) {
                Ok(p) => Arc::new(p),
                Err(e) => {
                    crate::print_error("disk writer pool init failed", &e);
                    return 1;
                }
            };
            let queue = DownloadQueue::new(store.clone(), pool, config);
            match queue.adopt_queued().await {
                Ok(_n) => {}
                Err(e) => {
                    crate::print_error("adopt failed", &e);
                    return 1;
                }
            }
            let q2 = queue.clone();
            let stop_ctrl = stop.clone();
            let ctrl_c_task = tokio::spawn(async move {
                let _ = tokio::signal::ctrl_c().await;
                stop_ctrl.store(true, std::sync::atomic::Ordering::Relaxed);
                let _ = q2.shutdown().await;
            });
            // Phase 3c：恢复 CLI 对单个任务进度的访问。订阅事件维护活动任务 id 集合，
            // 每秒通过 `DownloadQueue::progress(task_id)` 读取每个活动任务的最新快照。
            let q_progress = queue.clone();
            let progress_task = tokio::spawn(async move {
                let mut events = q_progress.subscribe_events();
                let mut active = std::collections::HashSet::<i64>::new();
                let mut ticker = tokio::time::interval(Duration::from_secs(1));
                loop {
                    tokio::select! {
                        ev = events.recv() => match ev {
                            Ok(QueueEvent::TaskStarted { id }) => { active.insert(id); }
                            Ok(QueueEvent::TaskCompleted { id })
                            | Ok(QueueEvent::TaskPaused { id })
                            | Ok(QueueEvent::TaskFailed { id }) => { active.remove(&id); }
                            Err(_) => break,
                            _ => {}
                        },
                        _ = ticker.tick() => {
                            let ids: Vec<i64> = active.iter().copied().collect();
                            for id in ids {
                                if let Some(snap) = q_progress.progress(id).await {
                                    println!(
                                        "TASK PROGRESS id={id} downloaded={} total={:?} speed={:.0} B/s",
                                        snap.downloaded, snap.total_size, snap.speed
                                    );
                                }
                            }
                        }
                    }
                }
            });
            progress_task.abort();
            let result = match queue.wait_for_idle().await {
                Ok(s) => {
                    let secs = s.total_elapsed.as_secs_f64();
                    println!(
                        "QUEUE DONE completed={} failed={} paused={} elapsed={secs:.2}s",
                        s.completed, s.failed, s.paused
                    );
                    0
                }
                Err(e) => {
                    crate::print_error("queue run failed", &e);
                    1
                }
            };
            ctrl_c_task.abort();
            result
        }
    }

    /// `config <set|get> <key> [<value>]`：读写全局设置。当前仅支持 key `download-dir`
    /// （目录必须存在或能创建）。
    pub async fn run_config(args: &[String], store: Arc<Store>) -> i32 {
        if args.is_empty() {
            eprintln!("usage: config <set|get> <key> [<value>]");
            return 2;
        }
        match args[0].as_str() {
            "set" => {
                if args.len() < 3 {
                    eprintln!("usage: config set <key> <value>");
                    return 2;
                }
                let key = &args[1];
                let value = &args[2];
                match key.as_str() {
                    "download-dir" => {
                        let p = std::path::Path::new(value);
                        if !p.exists()
                            && let Err(e) = std::fs::create_dir_all(p)
                        {
                            eprintln!("cannot create download-dir {value}: {e}");
                            return 1;
                        }
                    }
                    "global-max-speed" => {
                        // 支持 K/M 后缀；"0" 表示不限速（合法）。
                        if value.trim() == "0" {
                            // 合法：不限速。
                        } else if parse_max_speed(value).is_none() {
                            eprintln!("invalid global-max-speed: {value}");
                            return 2;
                        }
                    }
                    _ => {
                        eprintln!(
                            "unknown config key: {key} (only 'download-dir' and 'global-max-speed' are supported)"
                        );
                        return 2;
                    }
                }
                if let Err(e) = store.set_setting(key, value).await {
                    crate::print_error("config set failed", &e);
                    return 1;
                }
                println!("CONFIG SET {key}={value}");
                0
            }
            "get" => {
                if args.len() < 2 {
                    eprintln!("usage: config get <key>");
                    return 2;
                }
                let key = &args[1];
                match store.get_setting(key).await {
                    Ok(Some(v)) => {
                        println!("CONFIG GET {key}={v}");
                        0
                    }
                    Ok(None) => {
                        println!("CONFIG GET {key}=<unset>");
                        0
                    }
                    Err(e) => {
                        crate::print_error("config get failed", &e);
                        1
                    }
                }
            }
            other => {
                eprintln!("unknown config subcommand: {other}");
                2
            }
        }
    }

    /// `queue move <id> <top|bottom|up|down> [--db PATH]`：调整排队任务顺序。
    pub async fn run_queue_move(args: &[String], store: Arc<Store>) -> i32 {
        let mut id: Option<i64> = None;
        let mut pos: Option<QueueMove> = None;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--db" | "--log-file" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for {}", args[i - 1]);
                        return 2;
                    }
                }
                other if id.is_none() && !other.starts_with("--") => match other.parse::<i64>() {
                    Ok(v) => id = Some(v),
                    Err(_) => {
                        eprintln!("invalid task id: {other}");
                        return 2;
                    }
                },
                other if pos.is_none() && !other.starts_with("--") => match other {
                    "top" => pos = Some(QueueMove::Top),
                    "bottom" => pos = Some(QueueMove::Bottom),
                    "up" => pos = Some(QueueMove::Up),
                    "down" => pos = Some(QueueMove::Down),
                    other => {
                        eprintln!("unknown position: {other} (expected top|bottom|up|down)");
                        return 2;
                    }
                },
                other => {
                    eprintln!("unknown argument: {other}");
                    return 2;
                }
            }
            i += 1;
        }
        let id = match id {
            Some(v) => v,
            None => {
                eprintln!("usage: idm-cli queue move <id> <top|bottom|up|down> [--db PATH]");
                return 2;
            }
        };
        let pos = match pos {
            Some(p) => p,
            None => {
                eprintln!("missing position (top|bottom|up|down)");
                return 2;
            }
        };
        match store.move_in_queue(id, pos).await {
            Ok(()) => {
                println!("MOVED id={id} pos={pos:?}");
                0
            }
            Err(e) => {
                crate::print_error("move failed", &e);
                1
            }
        }
    }

    /// 解析 `queue pause/resume <id> [--db PATH]` 的位置参数。
    fn parse_queue_id_arg(args: &[String], usage: &str) -> std::result::Result<i64, i32> {
        let mut id: Option<i64> = None;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--db" | "--log-file" => {
                    i += 1;
                    if i >= args.len() {
                        eprintln!("missing value for {}", args[i - 1]);
                        return Err(2);
                    }
                }
                other if id.is_none() && !other.starts_with("--") => match other.parse::<i64>() {
                    Ok(v) => id = Some(v),
                    Err(_) => {
                        eprintln!("invalid task id: {other}");
                        return Err(2);
                    }
                },
                other => {
                    eprintln!("unknown argument: {other}");
                    return Err(2);
                }
            }
            i += 1;
        }
        id.ok_or_else(|| {
            eprintln!("{usage}");
            2
        })
    }

    /// `queue pause <id>`：Queued → Paused(User)；下载中 → 退出码 3；其他 → 退出码 2。
    pub async fn run_queue_pause(args: &[String], store: Arc<Store>) -> i32 {
        let id = match parse_queue_id_arg(args, "usage: idm-cli queue pause <id> [--db PATH]") {
            Ok(v) => v,
            Err(code) => return code,
        };
        let rec = match store.get_task(id).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("task {id} not found: {e}");
                return 2;
            }
        };
        match rec.state {
            TaskStateDb::Queued => {
                match store
                    .update_state(id, TaskStateDb::Paused, Some(PauseReasonDb::User), None)
                    .await
                {
                    Ok(()) => {
                        println!("paused {id}");
                        0
                    }
                    Err(e) => {
                        crate::print_error("pause failed", &e);
                        1
                    }
                }
            }
            TaskStateDb::Downloading | TaskStateDb::Finalizing | TaskStateDb::Probing => {
                eprintln!("任务正在下载，请在桌面程序中暂停，或执行 idm-cli runner stop");
                3
            }
            other => {
                eprintln!("task {id} cannot be paused in state {}", other.as_str());
                2
            }
        }
    }

    /// `queue resume <id>`：Paused / Failed → Queued；其他 → 退出码 2。
    pub async fn run_queue_resume(args: &[String], store: Arc<Store>) -> i32 {
        let id = match parse_queue_id_arg(args, "usage: idm-cli queue resume <id> [--db PATH]") {
            Ok(v) => v,
            Err(code) => return code,
        };
        let rec = match store.get_task(id).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("task {id} not found: {e}");
                return 2;
            }
        };
        match rec.state {
            TaskStateDb::Paused | TaskStateDb::Failed => {
                match store.requeue_task(id, false).await {
                    Ok(()) => {
                        println!("resumed {id}");
                        0
                    }
                    Err(e) => {
                        crate::print_error("resume failed", &e);
                        1
                    }
                }
            }
            other => {
                eprintln!("task {id} cannot be resumed in state {}", other.as_str());
                2
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use idm_core::store::Store;
        use idm_core::task::progress::ChunkProgress;

        use super::{ChunkState, ProgressSnapshot, format_chunk_summary};

        /// Phase 3a：`--proxy` 与 `--no-proxy` 同时传入必须以退出码 2 拒绝。
        #[tokio::test]
        async fn proxy_and_no_proxy_are_mutually_exclusive() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let store = Store::open(tmp.path().join("idm.db")).expect("store");
            for args in [
                vec![
                    "http://127.0.0.1/f".to_string(),
                    "--proxy".to_string(),
                    "http://127.0.0.1:3128".to_string(),
                    "--no-proxy".to_string(),
                ],
                vec![
                    "1".to_string(),
                    "--proxy".to_string(),
                    "socks5://127.0.0.1:1080".to_string(),
                    "--no-proxy".to_string(),
                ],
            ] {
                assert_eq!(super::run(&args, store.clone()).await, 2);
                assert_eq!(super::run_resume(&args[1..], store.clone()).await, 2);
            }
        }

        #[test]
        fn chunk_summary_contains_all_columns() {
            let snap = ProgressSnapshot {
                chunks: vec![ChunkProgress {
                    db_index: 0,
                    parent: None,
                    takeover: false,
                    start: 0,
                    current: 1024,
                    end: 1024,
                    state: ChunkState::Done,
                    elapsed_secs: 2.0,
                    avg_speed: 512.0,
                    retries: 3,
                    waiting_for_permit: false,
                }],
                ..ProgressSnapshot::default()
            };
            let text = format_chunk_summary(&snap);
            assert!(text.contains("id=0"), "{text}");
            assert!(text.contains("range=[0-1023]"), "{text}");
            assert!(text.contains("written=1024"), "{text}");
            assert!(text.contains("elapsed=2.00s"), "{text}");
            assert!(text.contains("avg_speed=512 B/s"), "{text}");
            assert!(text.contains("state=Done"), "{text}");
            assert!(text.contains("retries=3"), "{text}");
        }

        /// Phase 2.2：汇总表必须区分"接管"与"普通对半切"。
        #[test]
        fn chunk_summary_distinguishes_takeover() {
            let chunk = ChunkProgress {
                db_index: 0,
                parent: None,
                takeover: false,
                start: 0,
                current: 1024,
                end: 1024,
                state: ChunkState::Done,
                elapsed_secs: 1.0,
                avg_speed: 1024.0,
                retries: 0,
                waiting_for_permit: false,
            };
            let half = ChunkProgress {
                db_index: 1,
                parent: Some(0),
                ..chunk.clone()
            };
            let takeover = ChunkProgress {
                db_index: 2,
                parent: Some(0),
                takeover: true,
                ..chunk.clone()
            };
            let text = format_chunk_summary(&ProgressSnapshot {
                chunks: vec![chunk, half, takeover],
                ..ProgressSnapshot::default()
            });
            assert!(text.contains("source=initial"), "{text}");
            assert!(text.contains("source=split_from_0"), "{text}");
            assert!(text.contains("source=takeover_from_0"), "{text}");
        }
    }
}

/// 把错误本身与逐层 `source()` 展开成多行文本，供失败时打印完整错误链。
fn format_error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut text = format!("{e}");
    let mut src = e.source();
    let mut level = 1usize;
    while let Some(cause) = src {
        text.push_str(&format!("\n  caused by [{level}]: {cause}"));
        src = cause.source();
        level += 1;
    }
    text
}

/// 打印错误链（前缀 + 错误本身 + 逐层 source）。
fn print_error(prefix: &str, e: &(dyn std::error::Error + 'static)) {
    eprintln!("{prefix}: {}", format_error_chain(e));
}

/// 启动时预扫描 `--log-file`：tracing subscriber 必须在子命令参数解析之前初始化。
fn find_option(args: &[String], name: &str) -> Option<std::path::PathBuf> {
    let mut i = 0;
    while i + 1 < args.len() {
        if args[i] == name {
            return Some(std::path::PathBuf::from(&args[i + 1]));
        }
        i += 1;
    }
    None
}

fn find_log_file(args: &[String]) -> Option<std::path::PathBuf> {
    find_option(args, "--log-file")
}

fn find_db_path(args: &[String]) -> Option<std::path::PathBuf> {
    find_option(args, "--db")
}

/// 初始化全局 tracing：stderr 保持原有级别，`--log-file` 额外以 debug 级别写文件。
fn init_tracing(log_file: Option<&std::path::Path>) -> Result<(), Box<dyn std::error::Error>> {
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{EnvFilter, fmt};

    let stderr_layer = fmt::layer()
        .with_thread_ids(true)
        .with_target(true)
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .with_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")));

    let file_layer = match log_file {
        Some(path) => {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)?;
            }
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            Some(
                fmt::layer()
                    .with_thread_ids(true)
                    .with_target(true)
                    .with_ansi(false)
                    .with_writer(std::sync::Mutex::new(file))
                    .with_filter(EnvFilter::new("debug")),
            )
        }
        None => None,
    };

    tracing_subscriber::registry()
        .with(stderr_layer)
        .with(file_layer)
        .try_init()
        .map_err(|e| -> Box<dyn std::error::Error> {
            format!("tracing init failed: {e}").into()
        })?;
    Ok(())
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let log_file = find_log_file(&args);
    if let Err(e) = init_tracing(log_file.as_deref()) {
        eprintln!("cannot init tracing: {e}");
        std::process::exit(1);
    }

    let sub = args.get(1).map(String::as_str).unwrap_or("");
    if sub.is_empty() {
        info!(
            cli_version = env!("CARGO_PKG_VERSION"),
            core_version = core_version(),
            "idm-cli started"
        );
        return;
    }
    if sub == "bench-alloc" {
        #[cfg(windows)]
        {
            std::process::exit(bench::run(&args[2..]));
        }
        #[cfg(not(windows))]
        {
            eprintln!("bench-alloc is only supported on Windows");
            std::process::exit(2);
        }
    }
    // 注册命令只操作注册表与清单文件，不需要打开数据库。
    if sub == "nm" {
        #[cfg(windows)]
        {
            std::process::exit(nm::run(&args[2..]));
        }
        #[cfg(not(windows))]
        {
            eprintln!("nm is only supported on Windows");
            std::process::exit(2);
        }
    }
    // 运行器生命周期命令：start/stop 需要拉起或停止子进程，但不打开数据库本身。
    if sub == "runner" {
        #[cfg(windows)]
        {
            std::process::exit(runner::run(&args[2..]));
        }
        #[cfg(not(windows))]
        {
            eprintln!("runner is only supported on Windows");
            std::process::exit(2);
        }
    }

    #[cfg(windows)]
    {
        // 所有持久化子命令都支持 --db；未指定时用默认路径。
        let db_path = match find_db_path(&args) {
            Some(p) => p,
            None => match default_db_path() {
                Ok(p) => p,
                Err(e) => {
                    crate::print_error("cannot resolve default db path", &e);
                    std::process::exit(1);
                }
            },
        };
        let store = match Store::open(&db_path) {
            Ok(s) => s,
            Err(e) => {
                crate::print_error("cannot open database", &e);
                std::process::exit(1);
            }
        };
        // 启动时自动执行一次启动恢复（9.4）。
        // Phase 5a-1：仅 `queue run --watch` 跳过这里的恢复——它的 recover 已搬进
        // Runner::start，且在拿到 runner.lock 之后才执行；其余所有子命令（包括不带
        // --watch 的 `queue run`）保持原样，仍在这里恢复。
        let is_watch_run = sub == "queue"
            && args.get(2).map(String::as_str) == Some("run")
            && args.iter().skip(3).any(|a| a == "--watch");
        if !is_watch_run && let Err(e) = store.recover().await {
            crate::print_error("startup recovery failed", &e);
            std::process::exit(1);
        }
        let code = match sub {
            "download" => dl::run(&args[2..], Arc::clone(&store)).await,
            "resume" => dl::run_resume(&args[2..], Arc::clone(&store)).await,
            "list" => dl::run_list(&args[2..], Arc::clone(&store)).await,
            "remove" => dl::run_remove(&args[2..], Arc::clone(&store)).await,
            "enqueue" => dl::run_enqueue(&args[2..], Arc::clone(&store)).await,
            "config" => dl::run_config(&args[2..], Arc::clone(&store)).await,
            "queue" => {
                let sub2 = args.get(2).map(String::as_str).unwrap_or("");
                match sub2 {
                    "run" => dl::run_queue_run(&args[3..], Arc::clone(&store)).await,
                    "move" => dl::run_queue_move(&args[3..], Arc::clone(&store)).await,
                    "pause" => dl::run_queue_pause(&args[3..], Arc::clone(&store)).await,
                    "resume" => dl::run_queue_resume(&args[3..], Arc::clone(&store)).await,
                    other => {
                        eprintln!("unknown queue subcommand: {other}");
                        2
                    }
                }
            }
            other => {
                eprintln!("unknown subcommand: {other}");
                2
            }
        };
        // 显式关闭数据库（触发 WAL checkpoint），再退出，避免 `std::process::exit`
        // 跳过 Store 析构导致 WAL 未被回写、其它进程读不到数据。
        let _ = store.shutdown().await;
        drop(store);
        std::process::exit(code);
    }
    #[cfg(not(windows))]
    {
        eprintln!("{sub} is only supported on Windows");
        std::process::exit(2);
    }
}

#[cfg(test)]
mod cli_tests {
    use super::{find_log_file, format_error_chain};

    #[derive(Debug)]
    struct Wrapped(std::io::Error);

    impl std::fmt::Display for Wrapped {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "outer layer")
        }
    }

    impl std::error::Error for Wrapped {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn error_chain_lists_each_source() {
        let inner = std::io::Error::other("root cause");
        let outer = Wrapped(inner);
        let text = format_error_chain(&outer);
        assert!(text.starts_with("outer layer"), "{text}");
        assert!(text.contains("caused by [1]: root cause"), "{text}");
    }

    #[test]
    fn error_chain_without_source_has_no_caused_by_line() {
        let e = std::io::Error::from_raw_os_error(5);
        let text = format_error_chain(&e);
        assert!(!text.contains("caused by"), "{text}");
    }

    #[test]
    fn finds_log_file_from_args() {
        let args = vec![
            "download".to_string(),
            "--log-file".to_string(),
            "D:\\logs\\x.log".to_string(),
            "--max-speed".to_string(),
            "3M".to_string(),
        ];
        assert_eq!(
            find_log_file(&args),
            Some(std::path::PathBuf::from("D:\\logs\\x.log"))
        );
        assert_eq!(find_log_file(&args[..1]), None);
        assert_eq!(find_log_file(&[]), None);
    }
}
