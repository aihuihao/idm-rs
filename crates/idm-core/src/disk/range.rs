//! Chunk 区间与区间登记表。
//!
//! 登记表用 `std::sync::Mutex` 保护，持锁期间只做区间查找与更新，绝不做任何 IO。

use std::collections::HashMap;

use crate::error::{Error, Result};

/// 半开区间 `[start, end)`。
///
/// 构造时校验 `start < end` 且 `end <= 文件长度`，因此一旦构造成功即为合法的
/// 文件内区间。Phase 2 工作窃取会用到 `shrink_end` 来拆分尾部区间。
///
/// 特殊值 `end == u64::MAX` 表示**无界区间**（流式下载，长度未知），此时跳过
/// `end <= 文件长度` 的上界校验；`ChunkWriter` 据此不做尾部越界检查，数据可任意增长。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkRange {
    start: u64,
    end: u64,
}

impl ChunkRange {
    /// 构造区间并校验合法性。
    pub fn new(start: u64, end: u64, file_len: u64) -> Result<ChunkRange> {
        if start >= end {
            return Err(Error::InvalidRange {
                start,
                end,
                file_len,
            });
        }
        // 无界区间（流式，end == u64::MAX）跳过上界校验；其余要求 end <= 文件长度。
        if end != u64::MAX && end > file_len {
            return Err(Error::InvalidRange {
                start,
                end,
                file_len,
            });
        }
        Ok(ChunkRange { start, end })
    }

    /// 区间起始（含）。
    pub fn start(&self) -> u64 {
        self.start
    }

    /// 区间结束（不含）。
    pub fn end(&self) -> u64 {
        self.end
    }

    /// 该区间是否完整容纳 `[offset, offset+len)`。
    pub fn contains(&self, offset: u64, len: usize) -> bool {
        match offset.checked_add(len as u64) {
            Some(end) => offset >= self.start && end <= self.end,
            None => false,
        }
    }

    /// 是否与另一区间重叠（半开区间）。
    pub fn overlaps(&self, other: &ChunkRange) -> bool {
        self.start < other.end && other.start < self.end
    }

    /// 缩小尾部（仅内部使用，调用前已完成校验）。
    pub(crate) fn shrink(&mut self, new_end: u64) {
        self.end = new_end;
    }
}

/// 目标文件内部的区间登记表：把 `chunk_id` 映射到已登记的 `ChunkRange`。
///
/// 持锁期间只做区间查找与更新，不做任何 IO。
pub(crate) struct RangeRegistry {
    ranges: HashMap<u64, ChunkRange>,
    file_len: u64,
}

impl RangeRegistry {
    pub(crate) fn new(file_len: u64) -> Self {
        RangeRegistry {
            ranges: HashMap::new(),
            file_len,
        }
    }

    /// 登记一个新区间；若与任何已登记区间重叠则返回 [`Error::RangeOverlap`]。
    pub(crate) fn register(&mut self, chunk_id: u64, range: ChunkRange) -> Result<()> {
        for existing in self.ranges.values() {
            if existing.overlaps(&range) {
                return Err(Error::RangeOverlap {
                    start: range.start,
                    end: range.end,
                });
            }
        }
        self.ranges.insert(chunk_id, range);
        Ok(())
    }

    /// 注销一个区间（ChunkWriter 被 drop 时调用）。
    pub(crate) fn unregister(&mut self, chunk_id: u64) {
        self.ranges.remove(&chunk_id);
    }

    /// 缩小某个 chunk 的尾部区间，同步更新登记表。
    pub(crate) fn shrink_end(&mut self, chunk_id: u64, new_end: u64) -> Result<()> {
        match self.ranges.get_mut(&chunk_id) {
            Some(r) => {
                // 调用方已保证 new_end >= 已写位置 且 new_end <= 原 end。
                let updated = ChunkRange::new(r.start, new_end, self.file_len)?;
                *r = updated;
                Ok(())
            }
            None => Err(Error::RangeOverlap { start: 0, end: 0 }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_construction_validation() -> std::result::Result<(), Box<dyn std::error::Error>> {
        assert!(ChunkRange::new(0, 10, 10).is_ok());
        // end > file_len 非法
        assert!(ChunkRange::new(0, 10, 9).is_err());
        // start >= end 非法
        assert!(ChunkRange::new(5, 5, 10).is_err());
        assert!(ChunkRange::new(5, 4, 10).is_err());

        let r = ChunkRange::new(2, 8, 10)?;
        assert_eq!(r.start(), 2);
        assert_eq!(r.end(), 8);
        assert!(r.contains(2, 5));
        assert!(!r.contains(4, 5)); // 4 + 5 = 9 > 8
        let overlap_yes = ChunkRange::new(7, 9, 10)?;
        assert!(r.overlaps(&overlap_yes));
        let overlap_no = ChunkRange::new(8, 9, 10)?;
        assert!(!r.overlaps(&overlap_no));
        Ok(())
    }

    #[test]
    fn unbounded_range_skips_upper_bound() -> std::result::Result<(), Box<dyn std::error::Error>> {
        // 流式下载：end == u64::MAX 表示无界区间，即便文件长度为 0 也构造成功。
        let r = ChunkRange::new(0, u64::MAX, 0)?;
        assert_eq!(r.start(), 0);
        assert_eq!(r.end(), u64::MAX);
        // 任意有限偏移都不越界。
        assert!(r.contains(0, 1024));
        assert!(r.contains(1 << 40, 4096));
        Ok(())
    }

    #[test]
    fn registry_overlap_and_reregister_after_drop()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let mut reg = RangeRegistry::new(100);
        reg.register(1, ChunkRange::new(0, 10, 100)?)?;
        // 重叠应被拒绝
        let dup = reg.register(2, ChunkRange::new(5, 15, 100)?);
        assert!(dup.is_err());
        // drop 后（unregister）可重新登记重叠区间
        reg.unregister(1);
        assert!(reg.register(2, ChunkRange::new(5, 15, 100)?).is_ok());
        Ok(())
    }

    #[test]
    fn registry_shrink_end() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let mut reg = RangeRegistry::new(100);
        reg.register(1, ChunkRange::new(0, 50, 100)?)?;
        assert!(reg.shrink_end(1, 30).is_ok());
        let entry = reg.ranges.get(&1).ok_or("range 1 missing after register")?;
        assert_eq!(entry.end(), 30);
        // 缩到 start（空区间）非法
        assert!(reg.shrink_end(1, 0).is_err());
        Ok(())
    }
}
