//! idm-cli: 验证工具。
//!
//! 本程序仅调用 `idm-core` 的公开 API，不复制任何核心逻辑。Phase 1-3 通过它执行
//! 真实下载、续传与压力测试；本阶段额外提供 `bench-alloc` 子命令对 Windows 文件
//! 空间分配策略做实测。
//!
//! 核心库不初始化全局 tracing subscriber；由本可执行程序负责。

use idm_core::core_version;
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

#[cfg(windows)]
mod dl {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use idm_core::disk::{AllocationStrategy, DiskWriterPool, volume_supports_sparse};
    use idm_core::net::Url;
    use idm_core::net::client::DownloadConfig;
    use idm_core::net::filename::{resolve_conflict, sanitize_windows_filename};
    use idm_core::net::probe::Probe;
    use idm_core::task::{
        CancelSignal, ChunkState, DownloadOptions, DownloadOutcome, ProgressSnapshot, download,
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

    pub async fn run(args: &[String]) -> i32 {
        let mut url: Option<String> = None;
        let mut dir: Option<String> = None;
        let mut connections: usize = 8;
        let mut out_name: Option<String> = None;
        let mut cancel_after: Option<u64> = None;
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
                    "usage: idm-cli download <URL> [--dir DIR] [--connections N] [--out NAME] [--strategy sparse|reserve|setlen|auto] [--cancel-after SECONDS]"
                );
                return 2;
            }
        };

        let dir = match dir {
            Some(d) => PathBuf::from(d),
            None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        };
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("cannot create dir {}: {e}", dir.display());
            return 1;
        }

        let parsed_url = match Url::parse(&url) {
            Ok(u) => u,
            Err(e) => {
                eprintln!("invalid URL: {e}");
                return 2;
            }
        };
        if let Err(e) = DownloadConfig::check_scheme(&parsed_url) {
            eprintln!("{e}");
            return 2;
        }

        let config = DownloadConfig {
            connections,
            ..DownloadConfig::default()
        };
        let client = match config.build_client() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("cannot build HTTP client: {e}");
                return 1;
            }
        };

        // B5：先执行 Probe 并打印结果，再开始下载。
        let probe_result = match Probe::new(&client, &config).probe(&parsed_url).await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("probe failed: {e}");
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
            config,
            strategy,
            dir: dir.clone(),
            out_name,
            progress_tx: Some(progress_tx),
        };

        let pool = match DiskWriterPool::new(4, 16) {
            Ok(p) => Arc::new(p),
            Err(e) => {
                eprintln!("disk writer pool init failed: {e}");
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
        let result = download(&url, pool.clone(), &opts, Some(signal.as_ref())).await;
        let elapsed = started.elapsed();
        progress_task.abort();
        let snap = final_rx.borrow().clone();
        let (outcome, _rx) = match result {
            Ok(v) => v,
            Err(e) => {
                eprintln!("download failed: {e}");
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
            "RESULT status={outcome:?} path={} size={size} elapsed={:.2}s avg_speed={avg_speed:.0} B/s chunks={} strategy={actual_strategy}",
            final_path.display(),
            elapsed.as_secs_f64(),
            snap.chunks.len()
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
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_thread_ids(true)
        .with_target(true)
        .with_ansi(false)
        .init();
}

#[tokio::main]
async fn main() {
    init_tracing();

    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && args[1] == "bench-alloc" {
        #[cfg(windows)]
        {
            std::process::exit(bench::run(&args[2..]));
        }
        #[cfg(not(windows))]
        {
            eprintln!("bench-alloc is only supported on Windows");
            std::process::exit(2);
        }
    } else if args.len() > 1 && args[1] == "download" {
        #[cfg(windows)]
        {
            std::process::exit(dl::run(&args[2..]).await);
        }
        #[cfg(not(windows))]
        {
            eprintln!("download is only supported on Windows");
            std::process::exit(2);
        }
    }

    info!(
        cli_version = env!("CARGO_PKG_VERSION"),
        core_version = core_version(),
        "idm-cli started"
    );
}
