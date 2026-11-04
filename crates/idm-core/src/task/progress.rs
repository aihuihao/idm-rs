//! 下载进度快照与发布（`tokio::sync::watch`）。
//!
//! 发布频率不超过每 200ms 一次；没有读者时下载照常进行，不阻塞。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::task::state::ChunkState;

/// 单个分块的进度。
#[derive(Debug, Clone, Default)]
pub struct ChunkProgress {
    pub start: u64,
    pub current: u64,
    pub end: u64,
    pub state: ChunkState,
}

/// 全局进度快照。
#[derive(Debug, Clone, Default)]
pub struct ProgressSnapshot {
    pub total_size: Option<u64>,
    pub downloaded: u64,
    pub chunks: Vec<ChunkProgress>,
    /// 最近一段时间的平均速度（字节/秒）。
    pub speed: f64,
}

/// 进度发布器：内部持有共享快照与 `watch::Sender`，按 200ms 节流发布。
pub struct ProgressPublisher {
    current: Arc<Mutex<ProgressSnapshot>>,
    tx: watch::Sender<ProgressSnapshot>,
    last_publish: Instant,
    prev_downloaded: u64,
    prev_time: Instant,
}

impl ProgressPublisher {
    /// 创建发布器与接收端。`ranges` 为各分块的 (start, end)。
    pub fn new(
        total_size: Option<u64>,
        ranges: &[(u64, u64)],
    ) -> (Self, watch::Receiver<ProgressSnapshot>) {
        let chunks = ranges
            .iter()
            .map(|&(start, end)| ChunkProgress {
                start,
                current: start,
                end,
                state: ChunkState::Pending,
            })
            .collect();
        let snap = ProgressSnapshot {
            total_size,
            downloaded: 0,
            chunks,
            speed: 0.0,
        };
        let current = Arc::new(Mutex::new(snap.clone()));
        let (tx, rx) = watch::channel(snap);
        let now = Instant::now();
        (
            ProgressPublisher {
                current,
                tx,
                last_publish: now,
                prev_downloaded: 0,
                prev_time: now,
            },
            rx,
        )
    }

    /// 共享快照句柄（供各分块任务直接更新）。
    pub fn snapshot(&self) -> Arc<Mutex<ProgressSnapshot>> {
        self.current.clone()
    }

    /// 设置某分块状态。
    pub fn set_chunk_state(&self, idx: usize, state: ChunkState) {
        if let Some(c) = self
            .current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .chunks
            .get_mut(idx)
        {
            c.state = state;
        }
    }

    /// 设置某分块当前写入位置。
    pub fn set_chunk_current(&self, idx: usize, current: u64) {
        if let Some(c) = self
            .current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .chunks
            .get_mut(idx)
        {
            c.current = current;
        }
    }

    /// 若距上次发布已超过 200ms 则发布一次。即使无人读取也不阻塞。
    pub fn publish_if_due(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_publish) >= Duration::from_millis(200) {
            self.publish(now);
        }
    }

    /// 强制发布（完成时使用）。
    pub fn publish_final(&mut self) {
        self.publish(Instant::now());
    }

    fn publish(&mut self, now: Instant) {
        let mut snap = self.current.lock().unwrap_or_else(|e| e.into_inner());
        let downloaded: u64 = snap
            .chunks
            .iter()
            .map(|c| c.current.saturating_sub(c.start))
            .sum();
        let elapsed = now.duration_since(self.prev_time).as_secs_f64();
        if elapsed > 0.0 {
            snap.speed = (downloaded.saturating_sub(self.prev_downloaded)) as f64 / elapsed;
        }
        snap.downloaded = downloaded;
        let out = snap.clone();
        drop(snap);
        // 发送失败（无读者被 drop）忽略即可。
        let _ = self.tx.send(out);
        self.prev_downloaded = downloaded;
        self.prev_time = now;
        self.last_publish = now;
    }
}
