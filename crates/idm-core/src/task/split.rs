//! 动态切分与工作窃取（ARCHITECTURE 第 10 节）。
//!
//! 本模块只负责"切分"这件事：每个分块的共享状态（10.1）、切分点计算与竞争处理
//! （10.3 第 2~4 步）、按预计剩余时间选择被切分分块（10.3 第 1 步），以及执行
//! 切分（10.3 第 5~7 步）。分块任务的网络/写盘逻辑仍在 [`crate::task::download`]。

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::task::JoinSet;
use tracing::{info, warn};

use crate::disk::PoolChunk;
use crate::error::{Error, Result};
use crate::task::download::{AckedList, ChunkResult, ChunkSpawner, DownloadContext};
use crate::task::progress::ChunkInit;

/// 速度滑动窗口长度：只统计最近 4 秒的写入速度。
pub(super) const SPEED_WINDOW: Duration = Duration::from_secs(4);
/// 切分点对齐粒度（ARCHITECTURE 10.3 第 2 步：向下对齐到 64 KiB）。
pub(super) const SPLIT_ALIGN: u64 = 64 * 1024;

/// 单个分块的共享状态（ARCHITECTURE 10.1）。
///
/// 用 `std::sync::Mutex` 而不是 tokio 的异步锁：锁内只有几个整数与采样操作，
/// **持锁期间绝不做 IO，也不 `.await`**，因此不会阻塞运行时，也不会与写盘线程池
/// 形成锁序依赖。
pub(super) struct ChunkShared {
    /// 分块起点（不变）。
    pub(super) start: u64,
    /// 数据库中的 `chunk_index`（切分事务与汇总表使用）。
    pub(super) db_index: i64,
    /// 已写入并落盘的绝对位置，只在 `pool.write` 成功返回后推进。
    pub(super) acked: Arc<AtomicU64>,
    /// reserved / end / 速度采样。
    inner: StdMutex<SharedInner>,
}

struct SharedInner {
    /// 已预留的写入位置：`acked ≤ reserved ≤ end`。
    reserved: u64,
    /// 当前区间终点，可被切分缩小。
    end: u64,
    /// 速度采样：(采样时间, 当时的 acked 位置)。
    samples: VecDeque<(Instant, u64)>,
}

impl ChunkShared {
    pub(super) fn new(start: u64, end: u64, db_index: i64) -> Self {
        let acked = Arc::new(AtomicU64::new(start));
        let mut samples = VecDeque::with_capacity(8);
        samples.push_back((Instant::now(), start));
        ChunkShared {
            start,
            db_index,
            acked,
            inner: StdMutex::new(SharedInner {
                reserved: start,
                end,
                samples,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SharedInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 10.2 第 1 步：在锁内预留，返回本次可写长度。
    ///
    /// 返回 0 表示区间已被缩小到当前位置（10.2 第 2 步）。
    pub(super) fn reserve(&self, len: u64) -> u64 {
        let mut g = self.lock();
        let writable = len.min(g.end.saturating_sub(g.reserved));
        g.reserved += writable;
        writable
    }

    /// 10.2 第 4 步：写入失败时把 reserved 退回 acked（锁内只做整数运算）。
    pub(super) fn rollback_reserved(&self) {
        let acked = self.acked.load(Ordering::Acquire);
        let mut g = self.lock();
        if g.reserved > acked {
            g.reserved = acked;
        }
    }

    /// 当前区间终点。
    pub(super) fn end(&self) -> u64 {
        self.lock().end
    }

    /// 当前 (reserved, end) 快照。
    fn reserved_end(&self) -> (u64, u64) {
        let g = self.lock();
        (g.reserved, g.end)
    }

    /// 记录一次已落盘的位置，用于最近速度（滑动窗口）。
    pub(super) fn record(&self, pos: u64) {
        let now = Instant::now();
        let mut g = self.lock();
        g.samples.push_back((now, pos));
        prune_samples(&mut g.samples, now);
    }

    /// 最近 [`SPEED_WINDOW`] 内的平均速度（字节/秒）；样本不足时返回 0。
    fn recent_speed(&self) -> f64 {
        let now = Instant::now();
        let mut g = self.lock();
        prune_samples(&mut g.samples, now);
        let (Some(&(t0, p0)), Some(&(t1, p1))) = (g.samples.front(), g.samples.back()) else {
            return 0.0;
        };
        let dt = t1.saturating_duration_since(t0).as_secs_f64();
        if dt <= 0.0 {
            0.0
        } else {
            p1.saturating_sub(p0) as f64 / dt
        }
    }

    /// 剩余未落盘字节数。
    fn remaining(&self) -> u64 {
        let acked = self.acked.load(Ordering::Acquire);
        self.lock().end.saturating_sub(acked)
    }

    /// ARCHITECTURE 10.3 第 4 步：再次确认 `reserved < split`（被切分方可能在此期间
    /// 又前进了），满足则把 end 改为 split；返回是否切分成功。
    fn commit_split(&self, split: u64, expected_end: u64) -> bool {
        let mut g = self.lock();
        if g.reserved >= split || g.end != expected_end {
            return false;
        }
        g.end = split;
        true
    }
}

/// 计算切分点（ARCHITECTURE 10.3 第 2、3 步）。
///
/// `split = reserved + (end - reserved) / 2`，向下对齐到 64 KiB；
/// 只有切分后的两段（被切分方剩余 `[reserved, split)` 与新分块 `[split, end)`）
/// 都不小于 `min_split` 时才返回 `Some`。
pub(super) fn compute_split_point(reserved: u64, end: u64, min_split: u64) -> Option<u64> {
    if end <= reserved {
        return None;
    }
    let half = reserved + (end - reserved) / 2;
    let split = half - half % SPLIT_ALIGN;
    if split.saturating_sub(reserved) < min_split || end - split < min_split {
        return None;
    }
    Some(split)
}

/// 丢弃窗口之外的采样，但至少保留两个样本（否则无法计算速度）。
fn prune_samples(samples: &mut VecDeque<(Instant, u64)>, now: Instant) {
    while samples.len() > 2 {
        let Some(&(t, _)) = samples.front() else {
            break;
        };
        if now.saturating_duration_since(t) > SPEED_WINDOW {
            samples.pop_front();
        } else {
            break;
        }
    }
}

/// 一个在跑的静态分块：共享状态 + 写器句柄（切分时需要 shrink）。
pub(super) struct ChunkSlot {
    shared: Arc<ChunkShared>,
    /// 主流程保留的写器句柄，用于 10.3 第 6 步的 `pool.shrink_end`。
    pc: PoolChunk,
    /// 进度快照中的下标。
    idx: usize,
}

impl ChunkSlot {
    pub(super) fn new(shared: Arc<ChunkShared>, pc: PoolChunk, idx: usize) -> Self {
        ChunkSlot { shared, pc, idx }
    }
}

/// 10.3 第 1 步：在所有正在下载的分块中，选预计剩余时间（剩余字节 / 最近速度）
/// 最长的；速度为 0（例如正在退避重试）的视为无穷大，优先选中。
///
/// 返回 `(下标, 预计剩余秒数, 最近速度)`。
fn select_victim(slots: &[ChunkSlot]) -> Option<(usize, f64, f64)> {
    let mut best: Option<(usize, f64, f64)> = None;
    for (i, slot) in slots.iter().enumerate() {
        let remaining = slot.shared.remaining();
        if remaining == 0 {
            continue;
        }
        let speed = slot.shared.recent_speed();
        let eta = if speed <= 0.0 {
            f64::INFINITY
        } else {
            remaining as f64 / speed
        };
        if best.is_none_or(|(_, best_eta, _)| eta > best_eta) {
            best = Some((i, eta, speed));
        }
    }
    best
}

/// Phase 2 工作窃取：按 ARCHITECTURE 10.3 的 7 个步骤切分一个分块。
///
/// 返回 `Ok(true)` 表示发生了切分；`Ok(false)` 表示没有满足条件的候选（不变式完好）。
/// 任何一步失败都返回 `Err`，由主流程按任务失败处理。
pub(super) async fn try_split(
    ctx: Option<&DownloadContext>,
    spawner: &ChunkSpawner,
    slots: &mut Vec<ChunkSlot>,
    acked: &AckedList,
    next_db_index: &mut i64,
    min_split: u64,
    join_set: &mut JoinSet<Result<ChunkResult>>,
) -> Result<bool> {
    // 第 1 步：选择被切分分块。
    let Some((victim_i, eta, speed)) = select_victim(slots) else {
        return Ok(false);
    };

    // 第 2、3 步：加锁读取 reserved 与 end，切分点取剩余区间的中点并向下对齐到
    // 64 KiB；切分后两段都必须不小于最小切分大小。
    let (reserved, end) = slots[victim_i].shared.reserved_end();
    let Some(split) = compute_split_point(reserved, end, min_split) else {
        return Ok(false);
    };

    // 第 4 步：再次确认 reserved < split（被切分方可能在此期间又前进了）；
    // 满足则把 end 改为 split。
    if !slots[victim_i].shared.commit_split(split, end) {
        return Ok(false);
    }
    let db_index = slots[victim_i].shared.db_index;
    let victim_idx = slots[victim_i].idx;
    spawner
        .progress
        .lock()
        .await
        .set_chunk_end(victim_idx, split);

    // 第 5 步：数据库事务（同一事务内改被切分分块的 end 并插入新分块）。
    // 事务提交成功之后，才允许为新分块发起网络请求。
    let new_index = *next_db_index;
    if let Some(c) = ctx {
        c.store
            .split_chunk(c.task_id, db_index, split, new_index)
            .await?;
    }
    *next_db_index += 1;

    // 第 6 步：shrink_end。第 4 步已保证 reserved ≤ split，这一步必定成功；
    // 唯一可能的例外是被切分分块恰好在此之前已经读完并释放了写器——此时区间
    // [split, end) 从未被写过，跳过缩小是安全的。其他错误属于程序错误，任务失败。
    if let Err(e) = spawner.pool.shrink_end(&slots[victim_i].pc, split).await {
        if !matches!(e, Error::ChunkNotRegistered { .. }) {
            return Err(e);
        }
        warn!(
            task_id = spawner.task_id,
            chunk_id = db_index,
            "shrink skipped: writer already released"
        );
    }

    // 第 7 步：allocate_chunk(split, 原 end) → register → 用新连接下载。
    let shared = Arc::new(ChunkShared::new(split, end, new_index));
    let pc = spawner.open(&shared).await?;
    let new_idx = spawner.progress.lock().await.add_chunk(ChunkInit {
        db_index: new_index,
        start: split,
        end,
        parent: Some(db_index),
    });
    spawner.progress.lock().await.add_split();
    acked
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((new_index, Arc::clone(&shared.acked)));

    info!(
        task_id = spawner.task_id,
        victim = db_index,
        split,
        new_chunk = new_index,
        speed_bps = speed as u64,
        eta_secs = if eta.is_finite() { eta } else { -1.0 },
        "chunk split"
    );

    slots.push(ChunkSlot::new(Arc::clone(&shared), pc.clone(), new_idx));
    spawner.spawn(shared, pc, join_set, new_idx);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    type T = std::result::Result<(), Box<dyn std::error::Error>>;

    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;

    /// Phase 2 测试 1：切分点计算——64 KiB 对齐、两段都不小于最小切分大小、
    /// 剩余太小时放弃。
    #[test]
    fn split_point_is_aligned_and_respects_min_size() -> T {
        // 均分两段且对齐到 64 KiB。
        let split = compute_split_point(0, 8 * MIB, MIB);
        assert_eq!(split, Some(4 * MIB));
        assert_eq!(split.map(|s| s % SPLIT_ALIGN), Some(0));

        // 奇数额度也向下对齐：剩余 8 MiB + 100 KiB，中点不是 64 KiB 的整数倍。
        let end = 8 * MIB + 100 * KIB;
        let split = compute_split_point(0, end, MIB);
        let split = match split {
            Some(v) => v,
            None => return Err("expected a valid split point".into()),
        };
        assert_eq!(split % SPLIT_ALIGN, 0, "切分点必须向下对齐到 64 KiB");
        assert!(split <= end / 2 + SPLIT_ALIGN);
        assert!(split >= MIB && end - split >= MIB);

        // 已有进展时从 reserved 起算中点。
        let split = compute_split_point(3 * MIB, 7 * MIB, MIB);
        assert_eq!(split, Some(5 * MIB));

        // 剩余不足两倍最小切分大小 → 放弃。
        assert_eq!(compute_split_point(0, 2 * MIB - 1, MIB), None);
        // 向下对齐后第一段小于最小切分大小 → 放弃（200 KiB 的中点是 100 KiB，
        // 对齐到 64 KiB 后第一段只剩 64 KiB < 100 KiB）。
        assert_eq!(compute_split_point(0, 200 * KIB, 100 * KIB), None);
        // reserved 已经到 end → 放弃。
        assert_eq!(compute_split_point(4 * MIB, 4 * MIB, MIB), None);
        assert_eq!(compute_split_point(5 * MIB, 4 * MIB, MIB), None);
        Ok(())
    }

    /// Phase 2 测试 2：10.2 第 1、3 步——预留与截断；以及第 4 步的 reserved 回退。
    #[test]
    fn reserve_truncates_at_end() -> T {
        let s = ChunkShared::new(0, 1000, 0);
        // 完整预留。
        assert_eq!(s.reserve(400), 400);
        // 只剩 600 字节：本次只允许写 600（超出 end 的部分被截掉）。
        assert_eq!(s.reserve(1000), 600);
        let (reserved, end) = s.reserved_end();
        assert_eq!((reserved, end), (1000, 1000));
        // 区间已到 end → 可写长度为 0（10.2 第 2 步：丢弃剩余数据、关闭连接）。
        assert_eq!(s.reserve(1), 0);

        // 切分缩小 end 之后同样截断：reserved 已到 1024，新的 end 是 2048。
        let s = ChunkShared::new(0, 4096, 0);
        assert_eq!(s.reserve(1024), 1024);
        assert!(s.commit_split(2048, 4096));
        assert_eq!(s.reserve(4096), 1024, "只能写到新的 end 为止");
        assert_eq!(s.reserve(1), 0);

        // 10.2 第 4 步：写入失败 → reserved 退回 acked。
        let s = ChunkShared::new(0, 4096, 0);
        assert_eq!(s.reserve(1024), 1024);
        s.acked.store(256, Ordering::Release);
        s.rollback_reserved();
        assert_eq!(s.reserved_end().0, 256);
        Ok(())
    }

    /// Phase 2 测试 3：10.3 第 4 步的竞争——被切分方在"读取 reserved/end"与
    /// "再次确认"之间前进到切分点，切分必须放弃，end 不得变小。
    #[test]
    fn split_commit_loses_the_race() -> T {
        let s = Arc::new(ChunkShared::new(0, 16 * MIB, 0));
        let (reserved, end) = s.reserved_end();
        let split = match compute_split_point(reserved, end, MIB) {
            Some(v) => v,
            None => return Err("expected a valid split point".into()),
        };

        // 真实竞争窗口：另一个线程（被切分方）在窗口内把 reserved 推到 split。
        let writer = {
            let s = Arc::clone(&s);
            std::thread::spawn(move || (s.reserve(split), s.end()))
        };
        let joined = writer.join();
        assert!(joined.is_ok(), "writer thread panicked");
        let (advanced, end_after) = match joined {
            Ok(v) => v,
            Err(_) => return Err("writer thread panicked".into()),
        };
        assert_eq!(advanced, split, "被切分方把 reserved 推到了切分点");
        assert_eq!(end_after, end, "切分前 end 不变");

        // 第 4 步的再次确认必须失败，且 end 保持不变。
        assert!(
            !s.commit_split(split, end),
            "reserved >= split 时必须放弃切分"
        );
        let (reserved_after, end_after) = s.reserved_end();
        assert_eq!(end_after, end);
        assert_eq!(reserved_after, split);

        // 不变式：reserved ≤ end。
        assert!(reserved_after <= end_after);

        // 同一分块上重新计算的切分点只会更靠后，绝不会落回 reserved 之前。
        if let Some(next) = compute_split_point(reserved_after, end_after, MIB) {
            assert!(next > reserved_after);
        }
        Ok(())
    }
}
