//! 位置写入、短写循环与 `ChunkWriter`。
//!
//! 共享句柄上只允许使用 `seek_write`（显式偏移），禁止混用依赖游标的
//! `seek`/`read`/`write`。`write_all_at` 负责短写循环、0 字节报错与
//! `Interrupted` 重试。

use std::fs::File;
use std::io;
use std::os::windows::fs::FileExt;
use std::sync::{Arc, Mutex};

use tracing::debug;

use crate::disk::range::{ChunkRange, RangeRegistry};
use crate::error::{Error, Result};

/// 位置写入抽象：在给定偏移量处写入，不依赖文件当前游标。
///
/// 测试可注入模拟实现来驱动 [`write_all_at`] 的短写/0 字节/`Interrupted` 分支。
pub trait PositionalWrite {
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize>;
}

impl PositionalWrite for File {
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        // seek_write 显式指定偏移量，不改变/依赖文件游标之外的状态。
        self.seek_write(buf, offset)
    }
}

/// 从 `offset` 起循环写入，直到 `buf` 全部提交。
///
/// - 任何一次 `write_at` 返回 0 字节 → 立即报错（短写退化为 0）。
/// - 遇到 [`io::ErrorKind::Interrupted`] → 重试（不前进偏移）。
/// - 偏移量计算使用 `checked_add`，避免溢出。
pub fn write_all_at<P: PositionalWrite + ?Sized>(
    file: &P,
    mut buf: &[u8],
    mut offset: u64,
) -> io::Result<()> {
    while !buf.is_empty() {
        match file.write_at(buf, offset) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "seek_write returned 0 bytes",
                ));
            }
            Ok(n) => {
                buf = &buf[n..];
                offset = offset
                    .checked_add(n as u64)
                    .ok_or_else(|| io::Error::other("offset overflow"))?;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// 单个 Chunk 的写入器。
///
/// 持有所属区间与自己的写入位置（与文件游标无关）。越界写入会被拒绝且一个字节
/// 都不写。被 drop 时自动注销对应区间。
pub struct ChunkWriter {
    /// 通过 trait object 持有，便于单元测试注入失败的写入器（A3）。生产路径为 `Arc<File>`。
    file: Arc<dyn PositionalWrite + Send + Sync>,
    path: std::path::PathBuf,
    registry: Arc<Mutex<RangeRegistry>>,
    chunk_id: u64,
    range: ChunkRange,
    pos: u64,
    file_len: u64,
}

impl ChunkWriter {
    pub(crate) fn new(
        file: Arc<dyn PositionalWrite + Send + Sync>,
        path: std::path::PathBuf,
        registry: Arc<Mutex<RangeRegistry>>,
        chunk_id: u64,
        range: ChunkRange,
        file_len: u64,
    ) -> Self {
        let pos = range.start();
        ChunkWriter {
            file,
            path,
            registry,
            chunk_id,
            range,
            pos,
            file_len,
        }
    }

    /// 所属区间。
    pub fn range(&self) -> ChunkRange {
        self.range
    }

    /// 当前已写到的绝对偏移量。
    pub fn position(&self) -> u64 {
        self.pos
    }

    /// 该写器对应的 chunk 编号（供落盘线程池路由）。
    pub fn chunk_id(&self) -> u64 {
        self.chunk_id
    }

    /// 该写器对应的目标文件路径（供落盘线程池在错误中携带）。
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// 写入一块数据；先校验不越出区间尾部，越界则报错且一个字节都不写。
    pub fn write(&mut self, buf: &[u8]) -> Result<usize> {
        let end_offset =
            self.pos
                .checked_add(buf.len() as u64)
                .ok_or_else(|| Error::OutOfRange {
                    offset: self.pos,
                    len: buf.len(),
                    end: self.range.end(),
                })?;
        if end_offset > self.range.end() {
            return Err(Error::OutOfRange {
                offset: self.pos,
                len: buf.len(),
                end: self.range.end(),
            });
        }

        debug!(
            chunk_id = self.chunk_id,
            offset = self.pos,
            len = buf.len(),
            end = self.range.end(),
            "chunk write"
        );

        write_all_at(&*self.file, buf, self.pos).map_err(|e| self.map_io(e, "write"))?;
        self.pos = end_offset;
        Ok(buf.len())
    }

    /// 缩小区间尾部到 `new_end`（Phase 2 工作窃取需要的底层原语）。
    ///
    /// 要求：`new_end >= 当前已写位置`（已写数据不可被裁掉），且 `new_end <= 原 end`
    /// （只能缩小）。同步更新登记表。
    pub fn shrink_end(&mut self, new_end: u64) -> Result<()> {
        if new_end < self.pos {
            return Err(Error::InvalidRange {
                start: self.pos,
                end: new_end,
                file_len: self.file_len,
            });
        }
        if new_end > self.range.end() {
            return Err(Error::InvalidRange {
                start: self.range.end(),
                end: new_end,
                file_len: self.file_len,
            });
        }
        {
            let mut reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            reg.shrink_end(self.chunk_id, new_end)?;
        }
        self.range.shrink(new_end);
        Ok(())
    }

    fn map_io(&self, e: io::Error, operation: &'static str) -> Error {
        crate::error::io_error(operation, self.path.clone(), self.pos, e)
    }
}

impl Drop for ChunkWriter {
    fn drop(&mut self) {
        let mut reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        reg.unregister(self.chunk_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::io;

    #[derive(Clone, Copy)]
    enum MockMode {
        OneByte,
        Zero,
        InterruptedOnce,
    }

    /// 受控的模拟写入器：用于驱动 write_all_at 的各个分支。
    struct MockWriter {
        store: RefCell<Vec<u8>>,
        mode: Cell<MockMode>,
        interrupted: Cell<bool>,
    }

    impl MockWriter {
        fn new(capacity: usize, mode: MockMode) -> Self {
            MockWriter {
                store: RefCell::new(vec![0u8; capacity]),
                mode: Cell::new(mode),
                interrupted: Cell::new(false),
            }
        }
    }

    impl PositionalWrite for MockWriter {
        fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
            if let MockMode::Zero = self.mode.get() {
                // 返回 0 字节：write_all_at 必须据此报错。
                return Ok(0);
            }
            if matches!(self.mode.get(), MockMode::InterruptedOnce) && !self.interrupted.get() {
                self.interrupted.set(true);
                return Err(io::Error::new(io::ErrorKind::Interrupted, "interrupted"));
            }
            let off = offset as usize;
            let cap = self.store.borrow().len();
            let end = (off + buf.len()).min(cap);
            let n = match self.mode.get() {
                MockMode::OneByte => 1,
                _ => end - off,
            };
            if n == 0 {
                return Ok(0);
            }
            self.store.borrow_mut()[off..off + n].copy_from_slice(&buf[..n]);
            Ok(n)
        }
    }

    #[test]
    fn write_all_at_short_one_byte_completes() -> std::result::Result<(), Box<dyn std::error::Error>>
    {
        let w = MockWriter::new(16, MockMode::OneByte);
        let buf = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        write_all_at(&w, &buf, 0)?;
        assert_eq!(&w.store.borrow()[..10], &buf);
        Ok(())
    }

    #[test]
    fn write_all_at_zero_errors() {
        let w = MockWriter::new(16, MockMode::Zero);
        let buf = [1u8, 2, 3];
        let r = write_all_at(&w, &buf, 0);
        assert!(matches!(r, Err(e) if e.kind() == io::ErrorKind::WriteZero));
    }

    #[test]
    fn write_all_at_interrupted_then_retries() -> std::result::Result<(), Box<dyn std::error::Error>>
    {
        let w = MockWriter::new(16, MockMode::InterruptedOnce);
        let buf = [5u8, 6, 7, 8];
        write_all_at(&w, &buf, 0)?;
        assert_eq!(&w.store.borrow()[..4], &buf);
        Ok(())
    }

    /// 可切换失败/成功的注入式写入器（A3：验证写失败时位置不推进）。
    struct SwitchableWriter {
        fail: std::sync::atomic::AtomicBool,
        store: Mutex<Vec<u8>>,
    }

    impl SwitchableWriter {
        fn new(fail: bool) -> Self {
            SwitchableWriter {
                fail: std::sync::atomic::AtomicBool::new(fail),
                store: Mutex::new(Vec::new()),
            }
        }
    }

    impl PositionalWrite for SwitchableWriter {
        fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(io::Error::other("injected write failure"));
            }
            let mut s = self.store.lock().unwrap_or_else(|e| e.into_inner());
            let end = offset as usize + buf.len();
            if s.len() < end {
                s.resize(end, 0);
            }
            s[offset as usize..end].copy_from_slice(buf);
            Ok(buf.len())
        }
    }

    fn chunk_writer_with(
        file: Arc<dyn PositionalWrite + Send + Sync>,
    ) -> std::result::Result<ChunkWriter, Box<dyn std::error::Error>> {
        let registry = Arc::new(Mutex::new(RangeRegistry::new(100)));
        let range = ChunkRange::new(0, 100, 100)?;
        registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .register(0, range)?;
        Ok(ChunkWriter::new(
            file,
            std::path::PathBuf::from("mock.bin"),
            registry,
            0,
            range,
            100,
        ))
    }

    /// A3：写入失败时 `position()` 保持不变，之后重新写入同一偏移是安全的。
    #[test]
    fn write_failure_does_not_advance_position()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let file = Arc::new(SwitchableWriter::new(true));
        let mut w = chunk_writer_with(file.clone())?;
        assert_eq!(w.position(), 0);
        let failed = w.write(&[1, 2, 3, 4]);
        assert!(failed.is_err(), "injected write failure must surface");
        assert_eq!(w.position(), 0, "position must not advance on failure");

        // 关闭失败注入，重新写同一偏移，数据必须落在 0 而不是 4。
        file.fail.store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(w.write(&[9, 8, 7])?, 3);
        assert_eq!(w.position(), 3);
        let store = file.store.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(&store[..3], &[9, 8, 7]);
        Ok(())
    }
}
