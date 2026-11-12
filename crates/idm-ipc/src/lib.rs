//! idm-ipc: 浏览器通信层。
//!
//! 本 crate 负责 Firefox Native Messaging 通信，不监听任何本地网络端口。
//! 它依赖 `idm-core` 提供的下载能力，但 `idm-core` 不得反向依赖本 crate
//! （依赖方向始终是 cli / ipc / desktop → core）。
//!
//! Phase 4a 的实际内容：
//! - [`frame`]：4 字节长度前缀 + UTF-8 JSON 的帧编解码（含 64 KiB 上限，读负载前先校验长度）。
//! - `idm-nmhost` 二进制：只做不可信输入的校验与入队，stdout 只写协议帧；
//!   日志与 panic 写入数据目录下的 `nmhost.log`。下载由独立常驻运行器执行，
//!   两者只通过 SQLite 队列通信。
//! - 浏览器接管不在本 crate 内实现下载；扩展（4b）与注册表清单不在本阶段范围。

pub mod frame;
