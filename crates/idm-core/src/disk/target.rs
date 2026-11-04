//! 目标文件：创建、空间分配策略、空间信息查询与刷盘。
//!
//! 三种分配策略对应 `ARCHITECTURE.md` 第 3 节要求的实测对象：
//! `SetLen`、`Reserve`（先预留物理分配再设逻辑长度）、`Sparse`（稀疏文件）。
//! 创建前先查可用空间，空间不足直接报错且不创建文件；创建中途失败会删除半成品。

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tracing::warn;

use crate::disk::range::{ChunkRange, RangeRegistry};
use crate::disk::win32;
use crate::disk::write::ChunkWriter;
use crate::error::{Error, Result};

/// 文件空间分配策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocationStrategy {
    /// 仅调用 `set_len` 设置逻辑长度。
    SetLen,
    /// 先 `SetFileInformationByHandle(FileAllocationInfo)` 预留物理分配大小，再
    /// `set_len` 设置逻辑长度。
    Reserve,
    /// 先 `FSCTL_SET_SPARSE` 标记为稀疏文件，再 `set_len`。
    Sparse,
    /// 自动：卷支持稀疏文件时用 [`AllocationStrategy::Sparse`]，否则回退到
    /// [`AllocationStrategy::SetLen`]（与 `ARCHITECTURE.md` 第 3 节决定一致）。
    Auto,
}

/// 创建前的目标卷空间检查策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpaceCheck {
    /// 创建前检查目标卷可用空间，不足则返回 `Error::DiskFull`（默认用法）。
    Enforce,
    /// 跳过检查，仅用于 bench-alloc 观察系统原始行为。
    Skip,
}

/// 文件空间信息。
#[derive(Debug, Clone, Copy)]
pub struct AllocationInfo {
    /// 逻辑文件长度（File Size）。
    pub logical_len: u64,
    /// 物理分配大小（Allocation Size）。
    pub allocation_size: u64,
    /// 实际占用磁盘大小（GetCompressedFileSizeW，含压缩/稀疏）。
    pub compressed_size: u64,
    /// 是否为稀疏文件。
    pub is_sparse: bool,
}

/// 已创建/已打开的目标文件，内部维护区间登记表。
pub struct TargetFile {
    file: Arc<File>,
    path: PathBuf,
    len: u64,
    strategy: Option<AllocationStrategy>,
    registry: Arc<Mutex<RangeRegistry>>,
}

impl TargetFile {
    /// 创建新文件并应用分配策略。
    ///
    /// 绝不覆盖已存在的文件（`create_new`）。创建前若目标卷可用空间不足 `len`
    /// 直接返回 [`Error::DiskFull`] 且不创建文件；分配中途失败则删除半成品文件。
    pub fn create<P: AsRef<Path>>(
        path: P,
        len: u64,
        strategy: AllocationStrategy,
        space_check: SpaceCheck,
    ) -> Result<TargetFile> {
        let path = path.as_ref().to_path_buf();

        // 创建前检查目标卷可用空间（bench-alloc 的 `--skip-precheck` 模式传入
        // `SpaceCheck::Skip`，跳过此处以观察系统层面的空间不足报错时机）。
        if space_check == SpaceCheck::Enforce {
            let available = win32::get_disk_free_space(&path)?;
            if available < len {
                return Err(Error::DiskFull {
                    path: path.clone(),
                    needed: len,
                    available,
                });
            }
        }

        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| Error::Io {
                operation: "create",
                path: path.clone(),
                offset: 0,
                source: e,
            })?;

        let alloc = || -> Result<()> {
            match strategy {
                AllocationStrategy::SetLen => {
                    file.set_len(len).map_err(|e| Error::Io {
                        operation: "set_len",
                        path: path.clone(),
                        offset: len,
                        source: e,
                    })?;
                }
                AllocationStrategy::Reserve => {
                    // 先预留物理分配大小（AllocationSize = len），再扩展逻辑长度到 len。
                    // 这样逻辑长度落在已分配区间内，避免 NTFS 在扩展 EOF 时再自动分配并补零。
                    win32::set_file_allocation(&file, &path, len)?;
                    file.set_len(len).map_err(|e| Error::Io {
                        operation: "set_len",
                        path: path.clone(),
                        offset: len,
                        source: e,
                    })?;
                }
                AllocationStrategy::Sparse => {
                    // 先标记为稀疏文件（未写入区域不占物理空间），再 set_len 扩展逻辑长度。
                    win32::set_sparse(&file, &path)?;
                    file.set_len(len).map_err(|e| Error::Io {
                        operation: "set_len",
                        path: path.clone(),
                        offset: len,
                        source: e,
                    })?;
                }
                AllocationStrategy::Auto => {
                    // 自动：卷支持稀疏文件 → Sparse；否则回退 SetLen 并打 warn 日志。
                    let supports_sparse = win32::volume_supports_sparse(&path).unwrap_or(false);
                    if supports_sparse {
                        win32::set_sparse(&file, &path)?;
                    } else {
                        warn!(
                            path = ?path,
                            "volume does not support sparse files; falling back to SetLen allocation strategy"
                        );
                    }
                    file.set_len(len).map_err(|e| Error::Io {
                        operation: "set_len",
                        path: path.clone(),
                        offset: len,
                        source: e,
                    })?;
                }
            }
            Ok(())
        };

        if let Err(e) = alloc() {
            // 分配中途失败：删除半成品文件，绝不残留。
            let _ = std::fs::remove_file(&path);
            return Err(e);
        }

        Ok(TargetFile {
            file: Arc::new(file),
            path,
            len,
            strategy: Some(strategy),
            registry: Arc::new(Mutex::new(RangeRegistry::new(len))),
        })
    }

    /// 打开已有文件；长度与预期不符返回 [`Error::LengthMismatch`]
    /// （供 Phase 1c 断点续传校验文件未被破坏）。
    pub fn open_existing<P: AsRef<Path>>(path: P, expected_len: u64) -> Result<TargetFile> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| Error::Io {
                operation: "open",
                path: path.clone(),
                offset: 0,
                source: e,
            })?;
        let actual = file
            .metadata()
            .map_err(|e| Error::Io {
                operation: "metadata",
                path: path.clone(),
                offset: 0,
                source: e,
            })?
            .len();
        if actual != expected_len {
            return Err(Error::LengthMismatch {
                path: path.clone(),
                expected: expected_len,
                actual,
            });
        }
        Ok(TargetFile {
            file: Arc::new(file),
            path,
            len: actual,
            strategy: None,
            registry: Arc::new(Mutex::new(RangeRegistry::new(actual))),
        })
    }

    /// 申请一个**有界**写入区间，返回 [`ChunkWriter`]。与已登记区间重叠则返回
    /// [`Error::RangeOverlap`]。`ChunkWriter` 被 drop 时自动注销该区间。
    pub fn allocate_chunk(&self, chunk_id: u64, start: u64, end: u64) -> Result<ChunkWriter> {
        let range = ChunkRange::new(start, end, self.len)?;
        self.register_chunk(chunk_id, range)
    }

    /// 申请一个**无界**写入区间 `[start, ∞)`，仅供总长未知的单连接顺序下载使用。
    ///
    /// 登记表保证它在同一文件上独占：已有任何区间（或已有另一个无界区间）时返回
    /// [`Error::UnboundedRangeConflict`]。
    pub fn allocate_unbounded_chunk(&self, chunk_id: u64, start: u64) -> Result<ChunkWriter> {
        self.register_chunk(chunk_id, ChunkRange::unbounded(start))
    }

    fn register_chunk(&self, chunk_id: u64, range: ChunkRange) -> Result<ChunkWriter> {
        {
            let mut reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            reg.register(chunk_id, range)?;
        }
        Ok(ChunkWriter::new(
            self.file.clone(),
            self.path.clone(),
            self.registry.clone(),
            chunk_id,
            range,
            self.len,
        ))
    }

    /// 读取文件空间信息（逻辑长度、分配大小、实际占用、是否稀疏）。
    pub fn allocation_info(&self) -> Result<AllocationInfo> {
        let logical_len = self
            .file
            .metadata()
            .map_err(|e| Error::Io {
                operation: "metadata",
                path: self.path.clone(),
                offset: 0,
                source: e,
            })?
            .len();
        let allocation_size = win32::get_allocation_size(&self.file, &self.path)?;
        let compressed_size = win32::get_compressed_file_size(&self.path)?;
        let is_sparse = win32::is_sparse(&self.path)?;
        Ok(AllocationInfo {
            logical_len,
            allocation_size,
            compressed_size,
            is_sparse,
        })
    }

    /// 刷盘（FlushFileBuffers）。写入顺序：数据写入 → 刷盘 → 再记录进度。
    pub fn sync_data(&self) -> Result<()> {
        self.file
            .sync_data()
            .map_err(|e| crate::error::io_error("sync_data", self.path.clone(), 0, e))
    }

    /// 在给定偏移量写入（诊断/基准用，绕过区间登记与位置追踪）。生产写入路径请使用
    /// `allocate_chunk` + `ChunkWriter` 或落盘线程池。
    pub fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        crate::disk::write::write_all_at(&*self.file, buf, offset)
            .map_err(|e| crate::error::io_error("write_at", self.path.clone(), offset, e))
    }

    /// 文件逻辑长度。
    pub fn len(&self) -> u64 {
        self.len
    }

    /// 文件是否为空（长度为 0）。与 [`TargetFile::len`] 配套，满足 clippy 约定。
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 共享文件句柄（供落盘线程池使用）。
    pub(crate) fn file(&self) -> Arc<File> {
        self.file.clone()
    }

    /// 文件路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 创建时使用的分配策略（打开已有文件时为 `None`）。
    pub fn strategy(&self) -> Option<AllocationStrategy> {
        self.strategy
    }
}

/// 查询目标卷的文件系统类型名（公开封装，供 CLI 实测前判断是否为 NTFS）。
pub fn volume_filesystem(path: &Path) -> Result<String> {
    win32::get_volume_filesystem(path)
}

/// 查询目标卷可用空间（公开封装，供 CLI 计算 skip-precheck 的目标大小）。
pub fn disk_free_space(path: &Path) -> Result<u64> {
    win32::get_disk_free_space(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::tempdir;

    #[test]
    fn create_rejects_existing() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("exists.bin");
        let _t = TargetFile::create(&path, 4096, AllocationStrategy::SetLen, SpaceCheck::Enforce)?;
        // 已存在则绝不覆盖，必须失败。
        assert!(
            TargetFile::create(&path, 4096, AllocationStrategy::SetLen, SpaceCheck::Enforce)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn create_huge_size_disk_full_and_no_file()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("huge.bin");
        let huge = u64::MAX / 2;
        let r = TargetFile::create(&path, huge, AllocationStrategy::SetLen, SpaceCheck::Enforce);
        assert!(matches!(r, Err(Error::DiskFull { .. })));
        assert!(!path.exists(), "no file should be left behind on disk-full");
        Ok(())
    }

    #[test]
    fn out_of_range_write_rejected_and_no_change()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("oor.bin");
        let target =
            TargetFile::create(&path, 1024, AllocationStrategy::SetLen, SpaceCheck::Enforce)?;
        let mut w = target.allocate_chunk(1, 0, 512)?;
        let buf = vec![7u8; 100];
        w.write(&buf)?;
        // 越界：从 pos=100 再写 500 字节 -> 600 > 512
        let big = vec![9u8; 500];
        assert!(matches!(w.write(&big), Err(Error::OutOfRange { .. })));

        drop(w);
        drop(target);
        // 文件内容：0..100 为 7，100..600 保持 0（越界写入未生效）。
        let mut f = File::open(&path)?;
        let mut content = vec![0u8; 1024];
        f.read_exact(&mut content)?;
        for (i, &b) in content.iter().enumerate() {
            if i < 100 {
                assert_eq!(b, 7, "byte at {i}");
            } else {
                assert_eq!(b, 0, "out-of-range write changed byte at {i}");
            }
        }
        Ok(())
    }

    #[test]
    fn shrink_end_normal_and_invalid() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("shrink.bin");
        let target =
            TargetFile::create(&path, 100, AllocationStrategy::SetLen, SpaceCheck::Enforce)?;
        let mut w = target.allocate_chunk(1, 0, 50)?;
        let buf = vec![1u8; 20];
        w.write(&buf)?; // pos = 20
        assert_eq!(w.position(), 20);
        assert!(w.shrink_end(30).is_ok()); // 30 >= 20 且 <= 50
        assert_eq!(w.range().end(), 30);
        assert!(w.shrink_end(10).is_err()); // 小于已写位置
        assert!(w.shrink_end(60).is_err()); // 大于原 end
        drop(w);
        Ok(())
    }

    #[test]
    fn allocation_info_three_strategies() -> std::result::Result<(), Box<dyn std::error::Error>> {
        const SIZE: u64 = 1024 * 1024 * 1024; // 1GB
        for strategy in [
            AllocationStrategy::SetLen,
            AllocationStrategy::Reserve,
            AllocationStrategy::Sparse,
        ] {
            let dir = tempdir()?;
            let path = dir.path().join("alloc.bin");
            let target = TargetFile::create(&path, SIZE, strategy, SpaceCheck::Enforce)?;
            let info = target.allocation_info()?;
            assert_eq!(info.logical_len, SIZE, "logical length must match");
            if strategy == AllocationStrategy::Sparse {
                assert!(info.is_sparse, "sparse strategy must mark file sparse");
                assert!(
                    info.compressed_size < SIZE,
                    "sparse file should occupy far less than logical size"
                );
            }
            drop(target);
        }
        Ok(())
    }
}
