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
    use idm_core::net::probe::Probe;
    use idm_core::store::{NewTask, Store};
    use idm_core::task::{
        CancelSignal, ChunkState, DownloadContext, DownloadOptions, DownloadOutcome,
        ProgressSnapshot, download_with_store, resume,
    };
    use tokio::sync::watch;

    /// 解析限速值：纯字节数，或带 `K`/`k`（1024）、`M`/`m`（1024*1024）后缀。
    /// 支持小数写法（如 `1.5M`、`0.5K`），使用整数定点运算（不经 f64）：
    /// 拆成整数部分与小数部分，用 `checked_mul`/`checked_add` 计算，溢出即报错。
    /// 小数部分换算成字节后若不能整除，按**向下取整**处理（截断）。
    /// 格式非法、溢出或值为 0 时返回 `None`（调用方以退出码 2 报错）。
    fn parse_max_speed(raw: &str) -> Option<u64> {
        let s = raw.trim();
        if s.is_empty() {
            return None;
        }
        let (body, mult) = match s.as_bytes().last().copied()? {
            b'K' | b'k' => (&s[..s.len() - 1], 1024u64),
            b'M' | b'm' => (&s[..s.len() - 1], 1024 * 1024),
            _ => (s, 1u64),
        };
        // 拆成整数部分与可选的小数部分（只允许一个小数点）。
        let (int_part, frac_part) = match body.split_once('.') {
            Some((i, f)) => (i, Some(f)),
            None => (body, None),
        };
        if int_part.is_empty() || !int_part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let int_val = int_part.parse::<u64>().ok()?;
        let mut total = int_val.checked_mul(mult)?;
        if let Some(frac) = frac_part {
            // 小数部分必须全部是数字（允许为空，如 `1.`）。
            if !frac.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            if !frac.is_empty() {
                let frac_val = frac.parse::<u64>().ok()?;
                let scale = 10u64.checked_pow(frac.len() as u32)?;
                // 小数 → 字节；不能整除时向下取整（checked_div 截断）。
                let frac_bytes = frac_val.checked_mul(mult)?.checked_div(scale)?;
                total = total.checked_add(frac_bytes)?;
            }
        }
        if total == 0 {
            return None;
        }
        Some(total)
    }

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
        let opts = DownloadOptions {
            global_limiter: None,
            host_limiter: None,
            config: DownloadConfig {
                connections: (rec.connections as usize).clamp(1, 32),
                max_retries: max_retries.unwrap_or(5),
                dynamic_split,
                max_connections_per_host: max_host_connections,
                proxy,
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
        println!("ID | FILE | STATE | PAUSE | PROGRESS | LAST_ERROR");
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
                "{} | {} | {:?} | {} | {} | {}",
                t.id,
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

    #[cfg(test)]
    mod tests {
        use idm_core::store::Store;
        use idm_core::task::progress::ChunkProgress;

        use super::{ChunkState, ProgressSnapshot, format_chunk_summary, parse_max_speed};

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

        #[test]
        fn parses_plain_and_suffixed_speeds() {
            assert_eq!(parse_max_speed("500K"), Some(500 * 1024));
            assert_eq!(parse_max_speed("3M"), Some(3 * 1024 * 1024));
            assert_eq!(parse_max_speed("1m"), Some(1024 * 1024));
            assert_eq!(parse_max_speed("1048576"), Some(1048576));
            assert_eq!(parse_max_speed("1572864"), Some(1572864));
            // 小数 K/M 字面量：整数定点解析。
            assert_eq!(parse_max_speed("1.5M"), Some(1572864));
            assert_eq!(parse_max_speed("0.5K"), Some(512));
            assert_eq!(parse_max_speed("3.5M"), Some(3670016));
        }

        #[test]
        fn rejects_invalid_speeds() {
            for bad in [
                "",
                "0",
                "3G",
                "-1",
                "M",
                "1MB",
                "abc",
                ".5K",
                "1.2.3M",
                "999999999999999999999M",
            ] {
                assert_eq!(parse_max_speed(bad), None, "should reject {bad:?}");
            }
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

    #[cfg(windows)]
    {
        // 所有持久化子命令都支持 --db；未指定时用默认路径。
        let db_path = match find_db_path(&args) {
            Some(p) => p,
            None => match default_db_path() {
                Ok(p) => p,
                Err(e) => {
                    print_error("cannot resolve default db path", &e);
                    std::process::exit(1);
                }
            },
        };
        let store = match Store::open(&db_path) {
            Ok(s) => s,
            Err(e) => {
                print_error("cannot open database", &e);
                std::process::exit(1);
            }
        };
        // 启动时自动执行一次启动恢复（9.4）。
        if let Err(e) = store.recover().await {
            print_error("startup recovery failed", &e);
            std::process::exit(1);
        }
        let code = match sub {
            "download" => dl::run(&args[2..], Arc::clone(&store)).await,
            "resume" => dl::run_resume(&args[2..], Arc::clone(&store)).await,
            "list" => dl::run_list(&args[2..], Arc::clone(&store)).await,
            "remove" => dl::run_remove(&args[2..], Arc::clone(&store)).await,
            other => {
                eprintln!("unknown subcommand: {other}");
                2
            }
        };
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
