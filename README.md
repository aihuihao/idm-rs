# idm-rs

对标 IDM（Internet Download Manager）核心机制的下载管理器。目标平台为 Windows 10/11 x64，
核心能力包括：多连接动态分块下载、断点续传、浏览器接管（Native Messaging）。

## 技术栈

- 语言：Rust（edition 2024，MSVC 工具链 `x86_64-pc-windows-msvc`）
- 异步运行时：Tokio
- HTTP 客户端：Reqwest（开启 `stream`，保持默认 TLS，禁用自动解压）
- 状态持久化：rusqlite（`bundled` 内嵌 SQLite）
- 错误：thiserror；日志：tracing / tracing-subscriber
- 桌面端（Phase 5）：Tauri 2 + Vue 3

## 目录与三个 crate 的职责

- `crates/idm-core`（lib）：纯 Rust 下载引擎。网络、分块调度、断点续传、磁盘 I/O、
  SQLite 状态持久化全部在此。不依赖任何 GUI、浏览器或 IPC crate。
- `crates/idm-cli`（bin）：验证工具，只调用 `idm-core` 公开 API，不复制核心逻辑。
  Phase 1-3 通过它执行真实下载、续传与压力测试。
- `crates/idm-ipc`（lib）：浏览器通信层，Phase 4 启用，负责本地 IPC 与浏览器
  Native Messaging。依赖 `idm-core`，禁止反向依赖。本阶段仅含文档注释。
- `scripts/verify-workspace.ps1`：Phase 0 自动验收脚本。

## 编译与测试

```powershell
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo run -p idm-cli --locked
```

一键验收（含全部门禁）：

```powershell
pwsh -File scripts/verify-workspace.ps1   # 或 powershell -File ...
```

## 当前阶段

**当前处于 Phase 0**：仅完成工程骨架、依赖管理、架构规范文档、最小可编译代码与自动
验收脚本。未编写任何网络请求、下载、数据库或磁盘写入逻辑。

## 后续阶段方向

- **Phase 1**：单连接下载 + SQLite 状态机 + Windows 并发文件写入（seek_write）与
  空间分配实测，附真实并发写入测试。
- **Phase 2**：多连接动态分块与工作窃取，Chunk 区间可拆分的数据结构。
- **Phase 3**：断点续传一致性、远端变化校验、崩溃恢复。
- **Phase 4**：`idm-ipc` 本地通信与浏览器 Native Messaging。
- **Phase 5**：`idm-desktop`（Tauri 2 + Vue 3）作为 `idm-core` 的展示与控制层。
