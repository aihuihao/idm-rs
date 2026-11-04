#![warn(clippy::print_stdout, clippy::print_stderr)]
//! idm-core: 纯 Rust 下载引擎核心库。
//!
//! 本 crate 在 Phase 0 仅提供版本查询能力，用于验证工程骨架与工具链
//! （包括 SQLite 的 C 代码与 Reqwest 默认 TLS 后端）可在 MSVC 下编译、
//! 链接并初始化。
//!
//! Phase 1-3 将在此承载网络、分块调度、断点续传、磁盘 I/O 与 SQLite
//! 状态持久化等全部核心逻辑；它不依赖任何 GUI、浏览器或 IPC crate。

pub mod error;

/// Windows 磁盘 I/O 模块（共享句柄 + seek_write + 落盘线程池 + 空间分配）。
/// 仅 `cfg(windows)` 编译；非 Windows 平台不提供。
#[cfg(windows)]
pub mod disk;

/// 网络层：HTTP 客户端、Probe、响应头与文件名解析（依赖 reqwest）。
#[cfg(windows)]
pub mod net;

/// 下载编排、状态机与进度（依赖 `disk` 与 `net`）。
#[cfg(windows)]
pub mod task;

/// Returns the semantic version of this crate (`CARGO_PKG_VERSION`).
///
/// This is a `const fn` so it can be evaluated at compile time and embedded
/// into tracing fields without runtime allocation.
pub const fn core_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_version_is_non_empty() {
        assert!(!core_version().is_empty());
    }

    #[test]
    fn core_version_matches_pkg_version() {
        assert_eq!(core_version(), env!("CARGO_PKG_VERSION"));
    }

    // The two smoke tests below are toolchain smoke tests only: they prove that
    // the third-party dependencies (including SQLite's C code and Reqwest's
    // default TLS backend) can be compiled, linked, and initialized under MSVC.
    // They are NOT business functionality and must not be exposed as API.

    #[test]
    fn rusqlite_bundled_sqlite_initializes() -> Result<(), Box<dyn std::error::Error>> {
        let conn = rusqlite::Connection::open_in_memory()?;
        let version: String = conn.query_row("SELECT sqlite_version()", [], |row| row.get(0))?;
        assert!(
            !version.is_empty(),
            "sqlite_version() returned an empty string"
        );
        Ok(())
    }

    // NOTE: this test only verifies that the reqwest client can be built and the
    // TLS library initialized. It does NOT perform a TLS handshake and does NOT
    // validate any certificate chain. No network request is ever issued.
    #[tokio::test]
    async fn reqwest_client_builds_with_default_tls() {
        let client = reqwest::Client::builder().build();
        assert!(
            client.is_ok(),
            "reqwest client build failed: {:?}",
            client.err()
        );
    }
}
