//! 核心错误类型。
//!
//! 仅定义本阶段（磁盘 I/O 与空间分配）实际用到的变体。错误信息必须能定位
//! 问题：哪个文件、哪个偏移、要写多少字节。

use std::path::PathBuf;

/// idm-core 强类型错误。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// 带上下文的 IO 错误：操作名、路径、偏移量，以及底层 [`std::io::Error`]。
    #[error("IO error during {operation} on {path:?} at offset {offset}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        offset: u64,
        #[source]
        source: std::io::Error,
    },

    /// 写入返回 0 字节（seek_write 短写退化为 0）。
    #[error("write returned 0 bytes on {path:?} at offset {offset}")]
    WriteZero { path: PathBuf, offset: u64 },

    /// 磁盘空间不足：需要的字节数与可用字节数。
    #[error(
        "insufficient disk space on {path:?}: need {needed} bytes, available {available} bytes"
    )]
    DiskFull {
        path: PathBuf,
        needed: u64,
        available: u64,
    },

    /// 区间重叠：与已登记区间冲突。
    #[error("chunk range overlap: attempted {start}..{end} overlaps an existing range")]
    RangeOverlap { start: u64, end: u64 },

    /// 写入越出所分配区间的结束位置。
    #[error("write out of range: at offset {offset} writing {len} bytes exceeds chunk end {end}")]
    OutOfRange { offset: u64, len: usize, end: u64 },

    /// 非法区间：`start >= end` 或 `end > 文件长度`。
    #[error("invalid chunk range: start {start} >= end {end} (file length {file_len})")]
    InvalidRange { start: u64, end: u64, file_len: u64 },

    /// 打开已有文件时长度与预期不符（供 Phase 1c 断点续传使用）。
    #[error("file length mismatch on {path:?}: expected {expected}, actual {actual}")]
    LengthMismatch {
        path: PathBuf,
        expected: u64,
        actual: u64,
    },

    /// 落盘线程池已关闭，无法再提交写请求。
    #[error("disk writer pool is closed")]
    PoolClosed,
}

/// 便捷 Result 别名。
pub type Result<T> = std::result::Result<T, Error>;
