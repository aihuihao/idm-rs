//! Phase 1d（CLI 层）：resume 在续传过程中失败时，也必须输出 CHUNK SUMMARY（修复 4）。
//!
//! 复用 `idm-core/tests/support` 的测试服务器：先用 `break_after_bytes` 制造一次
//! "部分写入 + Failed" 的下载，留下可续传的 `.idm-part`；再打开 `error_switch`，
//! 让 `resume` 的分块请求返回 500，从而令续传失败。断言 CLI 退出码非零，且 stdout
//! 仍包含 `CHUNK SUMMARY`。
//!
//! 说明：集成测试只有在本 package 内才能通过 `CARGO_BIN_EXE_idm-cli` 拿到自身
//! 二进制，因此 CLI 子进程测试放在 idm-cli 的测试目录；纯库层面的流式重试测试仍在
//! `crates/idm-core/tests/phase1d.rs`。

#![cfg(windows)]

#[path = "../../idm-core/tests/support/mod.rs"]
mod support;

use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use support::{RangeMode, ServerConfig, start};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;

fn run_cli(args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new(env!("CARGO_BIN_EXE_idm-cli"))
        .args(args)
        .output()
}

/// 修复 4：resume 在续传过程中失败时，CLI 仍须打印 CHUNK SUMMARY 并以非零码退出。
#[tokio::test]
async fn resume_failure_prints_chunk_summary() -> TestResult {
    let size = 2 * MIB;
    let err_switch = Arc::new(AtomicBool::new(false));
    let srv = start(ServerConfig {
        size,
        // 首轮：第一条非 Probe 响应写到 64 KiB 就断开；配合 --max-retries 0，
        // 令首轮下载以"部分数据 + Failed"结束，留下可续传的 .idm-part。
        break_after_bytes: Some(64 * KIB),
        break_responses: Some(1),
        // 续传阶段：开关打开后，所有非 Probe 响应返回 500。
        error_status: Some(500),
        error_switch: Some(err_switch.clone()),
        range: RangeMode::Range,
        ..Default::default()
    })
    .await?;

    let tmp = tempfile::tempdir()?;
    let dir = tmp.path().join("out");
    std::fs::create_dir_all(&dir)?;
    let dir_s = dir.to_string_lossy().into_owned();
    let db_s = tmp.path().join("idm.db").to_string_lossy().into_owned();
    let url = srv.url("/file.bin");

    // 1) 首轮下载：预期失败，并留下部分数据。
    let first = {
        let url = url.clone();
        let dir = dir_s.clone();
        let db = db_s.clone();
        tokio::task::spawn_blocking(move || {
            run_cli(&[
                "download",
                &url,
                "--dir",
                &dir,
                "--out",
                "file.bin",
                "--connections",
                "1",
                "--max-retries",
                "0",
                "--db",
                &db,
                "--checkpoint-ms",
                "20",
            ])
        })
        .await??
    };
    assert!(
        !first.status.success(),
        "initial download should fail after a partial write; stdout={} stderr={}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        dir.join("file.bin.idm-part").exists(),
        "expected a .idm-part after the partial initial download"
    );

    // 2) 打开错误开关：续传的非 Probe 请求一律 500。
    err_switch.store(true, Ordering::SeqCst);

    // 3) 续传：应当失败，但仍打印 CHUNK SUMMARY。
    let resume = {
        let db = db_s.clone();
        tokio::task::spawn_blocking(move || {
            run_cli(&["resume", "1", "--db", &db, "--max-retries", "0"])
        })
        .await??
    };

    let stdout = String::from_utf8_lossy(&resume.stdout);
    assert!(
        !resume.status.success(),
        "resume should exit non-zero; stdout=\n{stdout}\nstderr={}",
        String::from_utf8_lossy(&resume.stderr)
    );
    assert!(
        stdout.contains("CHUNK SUMMARY"),
        "resume failure must still print CHUNK SUMMARY; stdout=\n{stdout}"
    );

    srv.shutdown();
    Ok(())
}
