//! idm-ipc: 浏览器通信层。
//!
//! 本 crate 在 Phase 4 才会启用，负责本地 IPC 与浏览器 Native Messaging。
//! 它将依赖 `idm-core` 提供的下载能力，但 `idm-core` 不得反向依赖本 crate
//! （依赖方向始终是 cli / ipc / desktop → core）。
//!
//! 当前阶段（Phase 0）本 crate 不含任何功能，仅保留文档注释与物理隔离结构，
//! 以便在 workspace 中验证依赖边界与 lint 规范。
