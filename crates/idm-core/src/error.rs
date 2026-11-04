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

    /// 写入过程中磁盘空间不足（系统错误码 112 ERROR_DISK_FULL / 39
    /// ERROR_HANDLE_DISK_FULL）。与创建前的 [`Error::DiskFull`] 区分，但
    /// [`Error::is_disk_full`] 对两者都返回 `true`。
    #[error("disk full during write on {path:?} at offset {offset}")]
    DiskFullDuringWrite { path: PathBuf, offset: u64 },

    /// 向落盘线程池写入/注销一个未注册（或已 release）的 chunk。
    #[error(
        "chunk not registered in disk writer pool: pool_id={pool_id}, chunk_id={chunk_id}, path={path:?}"
    )]
    ChunkNotRegistered {
        pool_id: u64,
        chunk_id: u64,
        path: PathBuf,
    },

    /// 落盘线程池构造参数非法（线程数或队列容量为 0）。
    #[error("invalid disk writer pool config: {message}")]
    InvalidPoolConfig { message: String },

    /// HTTP 请求失败（网络层错误，或状态码 4xx/5xx 被 reqwest 暴露）。
    #[error("http request to {url} failed: {source}")]
    Http {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    /// 参数非法（如连接数超出 1..=32、URL scheme 不支持）。
    #[error("invalid argument: {message}")]
    InvalidArgument { message: String },
    /// 响应携带非 `identity` 的 Content-Encoding，绝不能把压缩字节当作文件内容写盘。
    #[error("unsupported Content-Encoding: {encoding}")]
    UnsupportedContentEncoding { encoding: String },
    /// 带 If-Range 却返回 200（远端在下载过程中发生变化）。
    #[error("remote changed: If-Range sent but server returned 200")]
    RemoteChanged,
    /// 未带 If-Range 却返回 200（服务器不遵守 Range 请求）。
    #[error("server ignored Range request and returned 200")]
    RangeNotHonored,
    /// 返回 206 但 Content-Range 与请求区间不一致。
    #[error("Content-Range {actual:?} does not match requested {requested:?}")]
    InvalidContentRange {
        requested: (u64, u64),
        actual: String,
    },
    /// 收到的字节数少于分块长度（提前结束 / 连接被截断）。
    #[error("unexpected EOF on chunk {chunk_id}: expected {expected}, received {received}")]
    UnexpectedEof {
        chunk_id: u64,
        expected: u64,
        received: u64,
    },
}

/// 便捷 Result 别名。
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// 是否为磁盘空间不足（创建前预检查或写盘过程中）。`DiskFull` 与
    /// `DiskFullDuringWrite` 都返回 `true`，便于上层统一处理暂停/重试。
    pub fn is_disk_full(&self) -> bool {
        matches!(
            self,
            Error::DiskFull { .. } | Error::DiskFullDuringWrite { .. }
        )
    }
}

/// 把底层 IO 错误包装为强类型错误，并把系统错误码 112 / 39 统一映射为
/// [`Error::DiskFullDuringWrite`]，0 字节短写映射为 [`Error::WriteZero`]。
pub(crate) fn io_error(
    operation: &'static str,
    path: PathBuf,
    offset: u64,
    e: std::io::Error,
) -> Error {
    if is_disk_full_io(&e) {
        Error::DiskFullDuringWrite { path, offset }
    } else if e.kind() == std::io::ErrorKind::WriteZero {
        Error::WriteZero { path, offset }
    } else {
        Error::Io {
            operation,
            path,
            offset,
            source: e,
        }
    }
}

/// 系统错误码 112（ERROR_DISK_FULL）或 39（ERROR_HANDLE_DISK_FULL）视为磁盘满。
fn is_disk_full_io(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(112) | Some(39))
}
