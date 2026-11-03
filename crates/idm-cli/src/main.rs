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
    use std::os::windows::fs::FileExt;
    use std::path::Path;

    use idm_core::disk::{AllocationStrategy, TargetFile, disk_free_space, volume_filesystem};

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
                let target = match TargetFile::create(&file_path, size, strategy) {
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
                let file = target.file();

                // 尾部（len - 1MB）首次写入 1MB，只计 write 调用本身。
                let tail_off = size - MB;
                let buf = vec![0u8; MB as usize];
                let tw = std::time::Instant::now();
                if let Err(e) = file.seek_write(&buf, tail_off) {
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
                if let Err(e) = file.seek_write(&buf, mid_off) {
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
        // size = 可用空间 + 1GB，只做创建、不写入，观察空间不足时的行为。
        let size = available + GB;
        println!("SKIP_PRECHECK available_bytes={available} attempt_size_bytes={size} runs={runs}");
        for strategy in [
            AllocationStrategy::SetLen,
            AllocationStrategy::Reserve,
            AllocationStrategy::Sparse,
        ] {
            let file_path = path.join(format!("bench_skip_{strategy:?}.bin"));
            let r = TargetFile::create(&file_path, size, strategy);
            match r {
                Ok(_) => println!("  {strategy:?} CREATE_SUCCEEDED unexpected"),
                Err(e) => println!("  {strategy:?} CREATE_FAILED {e}"),
            }
            let _ = std::fs::remove_file(&file_path);
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
    }

    info!(
        cli_version = env!("CARGO_PKG_VERSION"),
        core_version = core_version(),
        "idm-cli started"
    );
}
