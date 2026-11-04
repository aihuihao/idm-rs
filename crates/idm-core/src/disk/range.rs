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
/// 总长未知的顺序下载使用 [`ChunkRange::unbounded`] 构造**无界区间**，它只能经由
/// [`ChunkRange::unbounded`] 创建，`new` 一律拒绝 `end == u64::MAX`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkRange {
    start: u64,
    end: u64,
}

/// 无界区间的内部哨兵值。
const UNBOUNDED_END: u64 = u64::MAX;

impl ChunkRange {
    /// 构造有界区间并校验合法性。`end == u64::MAX` 一律视为非法（无界区间必须用
    /// [`ChunkRange::unbounded`] 构造）。
    pub fn new(start: u64, end: u64, file_len: u64) -> Result<ChunkRange> {
        if end == UNBOUNDED_END || start >= end {
            return Err(Error::InvalidRange {
                start,
                end,
                file_len,
            });
        }
        if end > file_len {
            return Err(Error::InvalidRange {
                start,
                end,
                file_len,
            });
        }
        Ok(ChunkRange { start, end })
    }

    /// 构造**无界区间** `[start, ∞)`：仅允许总长未知的单连接顺序下载路径使用。
    ///
    /// 无界区间不校验文件长度（此时文件长度未知），`contains` 因此恒为 `true`，
    /// 数据可一直增长。`RangeRegistry` 会保证它在同一文件上独占。
    pub fn unbounded(start: u64) -> ChunkRange {
        ChunkRange {
            start,
            end: UNBOUNDED_END,
        }
    }

    /// 是否为无界区间。
    pub fn is_unbounded(&self) -> bool {
        self.end == UNBOUNDED_END
    }

    /// 区间起始（含）。
    pub fn start(&self) -> u64 {
        self.start
    }

    /// 区间结束（不含）。无界区间返回 `u64::MAX`。
    pub fn end(&self) -> u64 {
        self.end
    }

    /// 该区间是否完整容纳 `[offset, offset+len)`。无界区间只受 `start` 与溢出约束。
    pub fn contains(&self, offset: u64, len: usize) -> bool {
        if self.is_unbounded() {
            return offset >= self.start && offset.checked_add(len as u64).is_some();
        }
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
    /// 已登记的无界区间所属 chunk（若存在）。同一文件上无界区间必须独占。
    unbounded_chunk: Option<u64>,
}

impl RangeRegistry {
    pub(crate) fn new(file_len: u64) -> Self {
        RangeRegistry {
            ranges: HashMap::new(),
            file_len,
            unbounded_chunk: None,
        }
    }

    /// 登记一个新区间。
    ///
    /// 独占规则：无界区间必须唯一——有它在就不能再申请其他区间，已存在任何有界
    /// 区间时也不能再申请无界区间。违反时返回
    /// [`Error::UnboundedRangeConflict`]。普通重叠返回 [`Error::RangeOverlap`]。
    pub(crate) fn register(&mut self, chunk_id: u64, range: ChunkRange) -> Result<()> {
        if range.is_unbounded() {
            if self.unbounded_chunk.is_some() || !self.ranges.is_empty() {
                return Err(Error::UnboundedRangeConflict { chunk_id });
            }
            self.ranges.insert(chunk_id, range);
            self.unbounded_chunk = Some(chunk_id);
            return Ok(());
        }
        if self.unbounded_chunk.is_some() {
            return Err(Error::UnboundedRangeConflict { chunk_id });
        }
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
        if self.unbounded_chunk == Some(chunk_id) {
            self.unbounded_chunk = None;
        }
    }

    /// 缩小某个 chunk 的尾部区间，同步更新登记表。无界区间不允许收缩。
    pub(crate) fn shrink_end(&mut self, chunk_id: u64, new_end: u64) -> Result<()> {
        match self.ranges.get_mut(&chunk_id) {
            Some(r) if r.is_unbounded() => Err(Error::UnboundedRangeConflict { chunk_id }),
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
    fn new_rejects_u64_max_end() {
        // `new` 一律拒绝 u64::MAX，无界区间只能由 `unbounded` 构造。
        let e = ChunkRange::new(0, u64::MAX, u64::MAX);
        assert!(matches!(e, Err(Error::InvalidRange { .. })));
        let e2 = ChunkRange::new(0, u64::MAX, 0);
        assert!(matches!(e2, Err(Error::InvalidRange { .. })));
    }

    #[test]
    fn unbounded_only_via_dedicated_constructor() {
        let r = ChunkRange::unbounded(7);
        assert!(r.is_unbounded());
        assert_eq!(r.start(), 7);
        assert_eq!(r.end(), u64::MAX);
        // 任意有限偏移都不越界（含很大偏移），但起点之前不行。
        assert!(r.contains(7, 1024));
        assert!(r.contains(1 << 40, 4096));
        assert!(!r.contains(0, 8));
        assert!(matches!(ChunkRange::new(0, 10, 10), Ok(ref b) if !b.is_unbounded()));
    }

    #[test]
    fn unbounded_range_must_be_the_only_range()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        // 先无界，再有界 → 拒绝。
        let mut reg = RangeRegistry::new(0);
        reg.register(0, ChunkRange::unbounded(0))?;
        assert!(matches!(
            reg.register(1, ChunkRange::new(0, 10, 100)?),
            Err(Error::UnboundedRangeConflict { chunk_id: 1 })
        ));
        // 第二个无界也拒绝。
        assert!(matches!(
            reg.register(2, ChunkRange::unbounded(10)),
            Err(Error::UnboundedRangeConflict { chunk_id: 2 })
        ));
        // 注销后可重新登记有界区间。
        reg.unregister(0);
        assert!(reg.register(1, ChunkRange::new(0, 10, 100)?).is_ok());
        Ok(())
    }

    #[test]
    fn bounded_range_blocks_unbounded() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let mut reg = RangeRegistry::new(100);
        reg.register(0, ChunkRange::new(0, 10, 100)?)?;
        assert!(matches!(
            reg.register(1, ChunkRange::unbounded(20)),
            Err(Error::UnboundedRangeConflict { chunk_id: 1 })
        ));
        reg.unregister(0);
        assert!(reg.register(1, ChunkRange::unbounded(20)).is_ok());
        // 无界区间不允许 shrink。
        assert!(matches!(
            reg.shrink_end(1, 50),
            Err(Error::UnboundedRangeConflict { chunk_id: 1 })
        ));
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
