//! F.11 崩溃测试：启动 `idm-cli download` 子进程下载本地慢速文件，等数据库显示进度
//! 达到一个随机比例后用 `Child::kill` 强杀；先校验 [start, committed) 范围内的字节，
//! 再执行 `idm-cli resume`，最终文件必须逐字节正确。重复 20 次。
//!
//! 注意：强制结束进程不会丢失操作系统缓存里的数据，所以本测试验证不了断电场景。
//! 断电安全由 `phase1c_store.rs` 的 F.4 / F.5 检查点协议顺序测试保证。

#![cfg(windows)]

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::tempdir;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

const SIZE: u64 = 4 * 1024 * 1024;
const BLOCK: usize = 8192;
const ROUNDS: u64 = 20;

/// Phase 2 切分场景的文件大小：32 MiB / 4 连接 = 每个初始分块 8 MiB，
/// 被切分后每段 4 MiB，慢分块可以被连切两次（8→4→2 MiB）。
const SPLIT_SIZE: u64 = 32 * 1024 * 1024;

/// 测试服务器的行为：文件大小、慢速前缀、慢速块间隔。
///
/// 起点落在 `[0, slow_prefix)` 内的请求按 `slow_delay` 每 8 KiB 发一块，
/// 其余请求全速发送——这样"某一个分块很慢"可以被稳定复现。
#[derive(Clone, Copy)]
struct Spec {
    size: u64,
    slow_prefix: u64,
    slow_delay: Duration,
}

/// Phase 1 崩溃测试：整份文件都慢（约 533 KiB/s），让强杀时机落在下载过程中。
const SPEC_SLOW: Spec = Spec {
    size: SIZE,
    slow_prefix: SIZE,
    slow_delay: Duration::from_millis(15),
};

/// Phase 2 切分测试：只有开头 1 MiB 内的请求慢（约 2 MiB/s），其余全速。
const SPEC_SPLIT: Spec = Spec {
    size: SPLIT_SIZE,
    slow_prefix: 1024 * 1024,
    slow_delay: Duration::from_millis(4),
};

fn byte_at(offset: u64) -> u8 {
    let x = offset ^ (offset >> 8) ^ (offset >> 16) ^ (offset >> 24) ^ (offset >> 32);
    (x & 0xff) as u8
}

struct Server {
    url: String,
    stop: Arc<AtomicBool>,
    accept_thread: Option<thread::JoinHandle<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // 等 accept 循环真正退出，避免旧服务器与下一轮叠加。
        if let Some(h) = self.accept_thread.take() {
            let _ = h.join();
        }
    }
}

fn handle_conn(mut sock: TcpStream, stop: &AtomicBool, spec: Spec) -> std::io::Result<()> {
    // Winsock 的 accept 会继承监听 socket 的非阻塞属性，若不显式改回阻塞，
    // 请求头还没到达时 read_line 会立即 WouldBlock 并关闭连接，客户端只能看到
    // "error sending request for url"（多轮连跑时的偶发失败即由此产生）。
    sock.set_nonblocking(false)?;
    let mut reader = BufReader::new(sock.try_clone()?);
    let mut first = String::new();
    reader.read_line(&mut first)?;
    let mut range: Option<String> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':')
            && k.eq_ignore_ascii_case("range")
        {
            range = Some(v.trim().to_string());
        }
    }
    let is_range = range.is_some();
    let (start, end) = match range.as_deref() {
        Some(raw) => {
            let raw = raw.strip_prefix("bytes=").unwrap_or("");
            let mut parts = raw.split('-');
            let a = parts
                .next()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            let b = parts
                .next()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(spec.size - 1)
                .min(spec.size - 1);
            (a, b)
        }
        None => (0, spec.size - 1),
    };
    let end = end.min(spec.size - 1).max(start);
    let len = end - start + 1;
    let status = if is_range {
        "206 Partial Content"
    } else {
        "200 OK"
    };
    let mut head = format!("HTTP/1.1 {status}\r\nAccept-Ranges: bytes\r\n");
    if is_range {
        head.push_str(&format!(
            "Content-Range: bytes {start}-{end}/{}\r\n",
            spec.size
        ));
    }
    head.push_str(&format!(
        "Content-Length: {len}\r\nConnection: close\r\n\r\n"
    ));
    sock.write_all(head.as_bytes())?;
    sock.flush()?;

    let mut pos = start;
    while pos <= end && !stop.load(Ordering::Relaxed) {
        let n = ((end - pos + 1) as usize).min(BLOCK);
        let mut buf = vec![0u8; n];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = byte_at(pos + i as u64);
        }
        sock.write_all(&buf)?;
        sock.flush()?;
        pos += n as u64;
        // 慢速发送：让强杀时机落在下载过程中；慢速前缀之外的请求全速发送。
        if start < spec.slow_prefix {
            thread::sleep(spec.slow_delay);
        }
    }
    Ok(())
}

fn start_server(spec: Spec) -> TestResult<Server> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    listener.set_nonblocking(true)?;
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = Arc::clone(&stop);
    let accept_thread = thread::spawn(move || {
        while !stop2.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((sock, _)) => {
                    let s = Arc::clone(&stop2);
                    thread::spawn(move || {
                        let _ = handle_conn(sock, &s, spec);
                    });
                }
                Err(_) => {
                    // 任何暂时性错误都重试，避免 accept 循环意外退出。
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
    });
    Ok(Server {
        url: format!("http://127.0.0.1:{port}/file.bin"),
        stop,
        accept_thread: Some(accept_thread),
    })
}

/// 一条分块记录（Phase 2 起还包括 end 与切分来源）。
#[derive(Debug, Clone, Copy)]
struct ChunkRow {
    index: i64,
    start: u64,
    end: u64,
    committed: u64,
    parent: Option<i64>,
}

fn chunk_rows(db: &Path) -> TestResult<Vec<ChunkRow>> {
    let conn = rusqlite::Connection::open(db)?;
    conn.busy_timeout(Duration::from_millis(5000))?;
    let mut stmt = conn.prepare(
        "SELECT chunk_index, start, committed, end, parent_index FROM chunks ORDER BY chunk_index",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(ChunkRow {
                index: r.get::<_, i64>(0)?,
                start: r.get::<_, i64>(1)? as u64,
                committed: r.get::<_, i64>(2)? as u64,
                end: r.get::<_, i64>(3)? as u64,
                parent: r.get::<_, Option<i64>>(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn committed_sum(db: &Path) -> u64 {
    match chunk_rows(db) {
        Ok(rows) => rows
            .iter()
            .map(|r| r.committed.saturating_sub(r.start))
            .sum(),
        Err(_) => 0, // 表还没建好
    }
}

/// 数据库中已存在的切分次数（`parent_index` 非空的分块个数）。
fn split_count(db: &Path) -> u64 {
    match chunk_rows(db) {
        Ok(rows) => rows.iter().filter(|r| r.parent.is_some()).count() as u64,
        Err(_) => 0,
    }
}

fn verify_committed(db: &Path, part: &Path) -> TestResult<u64> {
    let rows = chunk_rows(db)?;
    let data = std::fs::read(part)?;
    let mut checked = 0u64;
    for row in rows {
        for off in row.start..row.committed {
            assert_eq!(
                data[off as usize],
                byte_at(off),
                "byte {off} in chunk {} [start, committed) does not match expected content",
                row.index
            );
            checked += 1;
        }
    }
    Ok(checked)
}

/// Phase 2 10.4：分块记录必须互不重叠、正好覆盖 `[0, size)`，且
/// `start ≤ committed ≤ end`；切分出来的分块必须记录来源。
fn assert_chunk_invariants(db: &Path, size: u64) -> TestResult {
    let rows = chunk_rows(db)?;
    assert!(rows.iter().all(|r| r.start < r.end), "empty chunk range");
    assert!(
        rows.iter()
            .all(|r| r.start <= r.committed && r.committed <= r.end),
        "committed out of [start, end]: {rows:?}"
    );
    let mut sorted: Vec<_> = rows.iter().collect();
    sorted.sort_by_key(|r| r.start);
    let mut expected = 0u64;
    for r in sorted {
        assert_eq!(
            r.start, expected,
            "chunk {} starts at {} but previous end is {expected}",
            r.index, r.start
        );
        expected = r.end;
    }
    assert_eq!(expected, size, "chunk records must cover [0, {size})");
    // 至少有一条切分记录，否则这个场景没有验证到 Phase 2 的切分。
    assert!(
        rows.iter().any(|r| r.parent.is_some()),
        "expected at least one split chunk record"
    );
    Ok(())
}

fn verify_final(path: &Path, size: u64) -> TestResult {
    let data = std::fs::read(path)?;
    assert_eq!(data.len() as u64, size, "final file length mismatch");
    for (i, &b) in data.iter().enumerate() {
        assert_eq!(b, byte_at(i as u64), "final file byte {i} mismatch");
    }
    Ok(())
}

fn one_round_attempt(bin: &str, seed: u64) -> TestResult<Option<u64>> {
    // 每轮独立服务器，避免上一轮被强杀的连接影响后续轮次。
    let srv = start_server(SPEC_SLOW)?;
    let url = srv.url.as_str();
    let tmp = tempdir()?;
    let db = tmp.path().join("idm.db");
    let dir = tmp.path().join("out");
    std::fs::create_dir_all(&dir)?;
    let db_str = db.to_string_lossy().into_owned();
    let dir_str = dir.to_string_lossy().into_owned();

    let mut child = Command::new(bin)
        .args([
            "download",
            url,
            "--dir",
            &dir_str,
            "--connections",
            "4",
            "--max-speed",
            "3M",
            "--db",
            &db_str,
            "--checkpoint-ms",
            "100",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    // 10% ~ 59% 的随机强杀目标；不同 seed 得到不同时机。
    let target = SIZE * (10 + (seed * 7 + 3) % 50) / 100;
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut deadline_hit = false;
    loop {
        if committed_sum(&db) >= target {
            break;
        }
        if Instant::now() > deadline {
            deadline_hit = true;
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }

    // 在强杀之前先记录子进程是否已自然退出：高负载下轮询可能滞后，下载可能
    // 已在我们动手前跑完。只有"自然且成功"的退出才是时序无效样本；自然退出但
    // 非零说明是真实的下载失败，必须让测试失败。
    match child.try_wait()? {
        Some(status) if status.success() => {
            println!("CRASH_RETRY seed={seed} reason=download_finished_before_kill");
            return Ok(None);
        }
        Some(status) => {
            return Err(format!(
                "seed={seed}: download exited with {status} before kill (real failure, not a crash sample)"
            )
            .into());
        }
        None => {}
    }

    // 强杀：不会执行任何收尾代码，也不发送取消信号。
    let _ = child.kill();
    let _ = child.wait();

    // deadline 到点仍未达到 target：下载太慢或卡死，明确失败而不是静默跳过。
    if deadline_hit {
        let got = committed_sum(&db);
        return Err(format!(
            "seed={seed}: deadline reached before target progress (committed={got}, target={target}); download too slow to produce a crash sample"
        )
        .into());
    }

    let part = dir.join("file.bin.idm-part");
    if !part.exists() {
        // 进程是被强杀的却没有留下 .idm-part：说明失败发生在建文件之前
        // （连接失败 / Probe 失败等），这是真实错误，不能当作"下载已完成"。
        return Err(format!(
            "seed={seed}: process killed but no .idm-part exists; failure occurred before file creation"
        )
        .into());
    }
    let checked = verify_committed(&db, &part)?;
    // 有效崩溃门禁：必须有部分进度，且远未完成。
    if checked == 0 || checked >= SIZE {
        return Err(format!(
            "seed={seed}: invalid committed range checked={checked} (expected 0 < checked < {SIZE})"
        )
        .into());
    }

    // 启动恢复由 CLI 在 resume 时自动执行；Downloading → Paused(Interrupted)。
    let out = Command::new(bin)
        .args(["resume", "1", "--db", &db_str, "--max-speed", "3M"])
        .output()?;
    if !out.status.success() {
        return Err(format!(
            "seed={seed}: resume failed; status={}; stdout={}; stderr={}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    // 确认下载成功且最终文件逐字节正确，才算本轮有效。
    verify_final(&dir.join("file.bin"), SIZE)?;
    Ok(Some(checked))
}

/// 取到一个有效的强杀样本；仅当确认是"下载在强杀前自然完成"的时序无效样本时才
/// 重试（最多 6 次）。真实错误（resume 失败、校验失败等）经 `?` 直接让测试失败。
fn one_round(bin: &str, seed: u64) -> TestResult<u64> {
    for _ in 0..6 {
        if let Some(checked) = one_round_attempt(bin, seed)? {
            return Ok(checked);
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(format!("round {seed}: no valid crash sample after 6 attempts").into())
}

/// Phase 2 测试 11：慢速分块 + 开启切分，**发生过切分之后**强制结束进程，再 resume。
///
/// 第 0 号分块很慢（起点 < 1 MiB 的请求都慢），其余三个分块全速；它们完成后会把
/// 慢分块切掉一半，如此反复。等数据库里出现切分记录并且已经有落盘进度后强杀。
fn one_round_split_attempt(bin: &str, seed: u64) -> TestResult<Option<(u64, u64)>> {
    let srv = start_server(SPEC_SPLIT)?;
    let url = srv.url.as_str();
    let tmp = tempdir()?;
    let db = tmp.path().join("idm.db");
    let dir = tmp.path().join("out");
    std::fs::create_dir_all(&dir)?;
    let db_str = db.to_string_lossy().into_owned();
    let dir_str = dir.to_string_lossy().into_owned();

    let mut child = Command::new(bin)
        .args([
            "download",
            url,
            "--dir",
            &dir_str,
            "--connections",
            "4",
            "--db",
            &db_str,
            "--checkpoint-ms",
            "50",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    // 强杀条件：已经发生过切分，并且至少有一个分块的进度已经落盘。
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut deadline_hit = false;
    while split_count(&db) < 2 || committed_sum(&db) == 0 {
        if Instant::now() > deadline {
            deadline_hit = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    match child.try_wait()? {
        Some(status) if status.success() => {
            println!("CRASH_SPLIT_RETRY seed={seed} reason=download_finished_before_kill");
            return Ok(None);
        }
        Some(status) => {
            return Err(format!(
                "split seed={seed}: download exited with {status} before kill (real failure)"
            )
            .into());
        }
        None => {}
    }

    let splits = split_count(&db);
    let checked_before = committed_sum(&db);
    let _ = child.kill();
    let _ = child.wait();

    if deadline_hit {
        return Err(format!(
            "split seed={seed}: deadline reached before any split/progress (splits={splits}, committed={checked_before})"
        )
        .into());
    }
    assert!(
        splits >= 2,
        "split seed={seed}: expected at least one split record in db"
    );

    let part = dir.join("file.bin.idm-part");
    if !part.exists() {
        return Err(format!("split seed={seed}: process killed but no .idm-part exists").into());
    }
    // 强杀瞬间：每条分块的 [start, committed) 必须逐字节正确，且分块记录满足 10.4。
    let checked = verify_committed(&db, &part)?;
    assert_chunk_invariants(&db, SPLIT_SIZE)?;
    assert!(
        checked > 0 && checked < SPLIT_SIZE,
        "split seed={seed}: checked={checked} not a partial progress"
    );

    // resume 必须读取切分出来的分块记录，并完成整份文件。
    let out = Command::new(bin)
        .args(["resume", "1", "--db", &db_str])
        .output()?;
    if !out.status.success() {
        return Err(format!(
            "split seed={seed}: resume failed; status={}; stdout={}; stderr={}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    verify_final(&dir.join("file.bin"), SPLIT_SIZE)?;
    Ok(Some((checked, splits)))
}

fn one_round_split(bin: &str, seed: u64) -> TestResult<(u64, u64)> {
    for _ in 0..6 {
        if let Some(v) = one_round_split_attempt(bin, seed)? {
            return Ok(v);
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(format!("split round {seed}: no valid crash sample after 6 attempts").into())
}

#[test]
#[ignore = "20 subprocess kill/resume rounds; run via: cargo test -p idm-cli --test crash_resume -- --ignored --nocapture"]
fn crash_resume_twenty_rounds() -> TestResult {
    let bin = env!("CARGO_BIN_EXE_idm-cli");
    // 诊断用：CRASH_ONLY_SEED=<n> 只跑一轮，便于定位单个时机。
    if let Ok(only) = std::env::var("CRASH_ONLY_SEED") {
        let seed: u64 = only.parse()?;
        let checked = one_round(bin, seed)?;
        println!("CRASH_ROUND seed={seed} verified_committed_bytes={checked} total={SIZE}");
        return Ok(());
    }
    let mut totals = Vec::new();
    for seed in 0..ROUNDS {
        let checked = one_round(bin, seed)?;
        // 让上一轮残留的连接线程有机会退出（它们检查 stop 后最多再睡 10ms）。
        thread::sleep(Duration::from_millis(80));
        totals.push(checked);
        println!("CRASH_ROUND seed={seed} verified_committed_bytes={checked} total={SIZE}");
    }
    assert_eq!(totals.len(), ROUNDS as usize);
    println!(
        "CRASH_ROUNDS_OK count={} min={} max={}",
        totals.len(),
        totals.iter().min().unwrap_or(&0),
        totals.iter().max().unwrap_or(&0)
    );
    Ok(())
}

#[test]
#[ignore = "20 subprocess kill/resume rounds with dynamic split; run via: cargo test -p idm-cli --test crash_resume -- --ignored --nocapture"]
fn crash_resume_with_split_twenty_rounds() -> TestResult {
    let bin = env!("CARGO_BIN_EXE_idm-cli");
    if let Ok(only) = std::env::var("CRASH_ONLY_SEED") {
        let seed: u64 = only.parse()?;
        let (checked, splits) = one_round_split(bin, seed)?;
        println!(
            "CRASH_SPLIT_ROUND seed={seed} splits={splits} verified_committed_bytes={checked} total={SPLIT_SIZE}"
        );
        return Ok(());
    }
    let mut rounds = Vec::new();
    for seed in 0..ROUNDS {
        let (checked, splits) = one_round_split(bin, seed)?;
        // 让上一轮残留的连接线程有机会退出（它们检查 stop 后最多再睡一个块间隔）。
        thread::sleep(Duration::from_millis(80));
        rounds.push((checked, splits));
        println!(
            "CRASH_SPLIT_ROUND seed={seed} splits={splits} verified_committed_bytes={checked} total={SPLIT_SIZE}"
        );
    }
    assert_eq!(rounds.len(), ROUNDS as usize);
    println!(
        "CRASH_SPLIT_ROUNDS_OK count={} min_checked={} max_checked={} max_splits={}",
        rounds.len(),
        rounds.iter().map(|r| r.0).min().unwrap_or(0),
        rounds.iter().map(|r| r.0).max().unwrap_or(0),
        rounds.iter().map(|r| r.1).max().unwrap_or(0)
    );
    Ok(())
}
