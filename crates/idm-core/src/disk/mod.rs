//! Windows 磁盘 I/O 模块（仅 `cfg(windows)` 编译）。
//!
//! 设计约束来自 `ARCHITECTURE.md` 第 2、3 节：
//!
//! - 多个 Chunk 共享同一个 `Arc<std::fs::File>`，所有写入显式指定偏移量，
//!   使用 `std::os::windows::fs::FileExt::seek_write`，禁止依赖文件游标的
//!   `seek`/`read`/`write`。
//! - `seek_write` 可能短写：必须按返回值推进偏移、循环写入直到全部提交；
//!   写入 0 字节视为错误。
//! - 阻塞磁盘写入不在 Tokio 工作线程上执行：异步侧通过 [`pool::DiskWriterPool`]
//!   把写请求交给专用落盘线程（有界通道形成背压）。
//! - 不同 Chunk 的写入区间由 [`range::ChunkRange`] 登记表保证不重叠，落盘层做断言校验。
//!
//! 该模块如需调用 Win32 API，全部封装在 [`win32`] 子模块中；其它子模块不得
//! 直接调用 windows-sys。

pub mod pool;
pub mod range;
pub mod target;
pub mod win32;
pub mod write;

pub use pool::DiskWriterPool;
pub use range::ChunkRange;
pub use target::{
    AllocationInfo, AllocationStrategy, TargetFile, disk_free_space, volume_filesystem,
};
pub use write::{ChunkWriter, PositionalWrite};
