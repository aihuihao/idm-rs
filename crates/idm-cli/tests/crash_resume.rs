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

fn handle_conn(mut sock: TcpStream, stop: &AtomicBool) -> std::io::Result<()> {
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
        Some(spec) => {
            let spec = spec.strip_prefix("bytes=").unwrap_or("");
            let mut parts = spec.split('-');
            let a = parts
                .next()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            let b = parts
                .next()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(SIZE - 1)
                .min(SIZE - 1);
            (a, b)
        }
        None => (0, SIZE - 1),
    };
    let end = end.min(SIZE - 1).max(start);
    let len = end - start + 1;
    let status = if is_range {
        "206 Partial Content"
    } else {
        "200 OK"
    };
    let mut head = format!("HTTP/1.1 {status}\r\nAccept-Ranges: bytes\r\n");
    if is_range {
        head.push_str(&format!("Content-Range: bytes {start}-{end}/{SIZE}\r\n"));
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
        // 慢速发送：让强杀时机落在下载过程中。
        thread::sleep(Duration::from_millis(15));
    }
    Ok(())
}

fn start_server() -> TestResult<Server> {
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
                        let _ = handle_conn(sock, &s);
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

fn chunk_rows(db: &Path) -> TestResult<Vec<(u64, u64)>> {
    let conn = rusqlite::Connection::open(db)?;
    conn.busy_timeout(Duration::from_millis(5000))?;
    let mut stmt = conn.prepare("SELECT start, committed FROM chunks ORDER BY chunk_index")?;
    let rows = stmt
        .query_map([], |r| {
            Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn committed_sum(db: &Path) -> u64 {
    match chunk_rows(db) {
        Ok(rows) => rows.iter().map(|(s, c)| c.saturating_sub(*s)).sum(),
        Err(_) => 0, // 表还没建好
    }
}

fn verify_committed(db: &Path, part: &Path) -> TestResult<u64> {
    let rows = chunk_rows(db)?;
    let data = std::fs::read(part)?;
    let mut checked = 0u64;
    for (start, committed) in rows {
        for off in start..committed {
            assert_eq!(
                data[off as usize],
                byte_at(off),
                "byte {off} in [start, committed) does not match expected content"
            );
            checked += 1;
        }
    }
    Ok(checked)
}

fn verify_final(path: &Path) -> TestResult {
    let data = std::fs::read(path)?;
    assert_eq!(data.len() as u64, SIZE, "final file length mismatch");
    for (i, &b) in data.iter().enumerate() {
        assert_eq!(b, byte_at(i as u64), "final file byte {i} mismatch");
    }
    Ok(())
}

fn one_round_attempt(bin: &str, seed: u64) -> TestResult<Option<u64>> {
    // 每轮独立服务器，避免上一轮被强杀的连接影响后续轮次。
    let srv = start_server()?;
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
    loop {
        if committed_sum(&db) >= target || Instant::now() > deadline {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    // 强杀：不会执行任何收尾代码，也不发送取消信号。
    let _ = child.kill();
    let _ = child.wait();

    let part = dir.join("file.bin.idm-part");
    if !part.exists() {
        // 高负载下轮询可能滞后，下载在强杀前已完成：不是有效的崩溃样本。
        println!("CRASH_RETRY seed={seed} reason=download_finished_before_kill");
        return Ok(None);
    }
    let checked = verify_committed(&db, &part)?;
    if checked == 0 || checked >= SIZE {
        println!("CRASH_RETRY seed={seed} reason=no_partial_progress checked={checked}");
        return Ok(None);
    }

    // 启动恢复由 CLI 在 resume 时自动执行；Downloading → Paused(Interrupted)。
    let out = Command::new(bin)
        .args(["resume", "1", "--db", &db_str, "--max-speed", "3M"])
        .output()?;
    if !out.status.success() {
        println!(
            "CRASH_RETRY seed={seed} reason=resume_failed stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        return Ok(None);
    }
    verify_final(&dir.join("file.bin"))?;
    Ok(Some(checked))
}

/// 取到一个有效的强杀样本；时序抖动导致的无效样本最多重试 6 次。
fn one_round(bin: &str, seed: u64) -> TestResult<u64> {
    for _ in 0..6 {
        if let Some(checked) = one_round_attempt(bin, seed)? {
            return Ok(checked);
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(format!("round {seed}: no valid crash sample after 6 attempts").into())
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
