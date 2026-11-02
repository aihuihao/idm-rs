//! idm-cli: 验证工具。
//!
//! 本程序仅调用 `idm-core` 的公开 API（当前为 `core_version`），不复制任何
//! 核心逻辑。Phase 1-3 将通过它执行真实下载、续传与压力测试。
//!
//! 核心库不初始化全局 tracing subscriber；由本可执行程序负责。

use idm_core::core_version;
use tracing::info;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_thread_ids(true)
        .with_target(true)
        .with_ansi(false)
        .init();

    info!(
        cli_version = env!("CARGO_PKG_VERSION"),
        core_version = core_version(),
        "idm-cli started"
    );
}
