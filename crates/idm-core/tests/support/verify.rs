//! 逐字节校验下载结果，供需要的测试以 `mod verify;` 单独引入。
//!
//! 之所以不放在 `support/mod.rs` 里：`support/mod.rs` 会被每个测试 target 整体编译，
//! 而本项目除 `win32.rs` 的 `#![allow(unsafe_code)]` 外禁止任何 `#[allow]`，不使用
//! `verify_file` 的 target（如 `phase1d`）就会因 dead_code 触发 clippy `-D warnings`。

use std::path::Path;

use crate::support::byte_at;

/// 逐字节校验文件内容与服务器生成规则一致。
pub fn verify_file(path: &Path, size: u64) -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read(path)?;
    if data.len() as u64 != size {
        return Err(format!("file length mismatch: expected {size}, got {}", data.len()).into());
    }
    for (i, &b) in data.iter().enumerate() {
        let expected = byte_at(i as u64);
        if b != expected {
            return Err(format!("byte {i}: expected {expected}, got {b}").into());
        }
    }
    Ok(())
}
