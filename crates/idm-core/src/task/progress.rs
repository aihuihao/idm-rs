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
    /// 分块从开始到退出的耗时（秒）。分块尚未退出时为 0。
    /// 用于 CLI 在下载结束时打印分块汇总表。
    pub elapsed_secs: f64,
    /// 分块平均写入速度（字节/秒）= 已写入字节数 / 耗时。
    pub avg_speed: f64,
}

/// 全局进度快照。
#[derive(Debug, Clone, Default)]
pub struct ProgressSnapshot {
    pub total_size: Option<u64>,
    pub downloaded: u64,
    pub chunks: Vec<ChunkProgress>,
    /// 最近一段时间的平均速度（字节/秒）。
    pub speed: f64,
    /// 是否可断点续传：仅静态分块（总长已知且支持 Range）为 `true`；
    /// 总长未知/不支持 Range 的单连接顺序下载为 `false`（供 Phase 1c 使用）。
    pub resumable: bool,
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
    ///
    /// 传入 `tx` 时复用外部通道（调用方可在下载开始前就订阅进度，用于实时刷新）；
    /// 传 `None` 则内部创建通道。无论哪种，发布都按 200ms 节流，且没有读者也不阻塞。
    pub fn new(
        total_size: Option<u64>,
        ranges: &[(u64, u64)],
        tx: Option<watch::Sender<ProgressSnapshot>>,
        resumable: bool,
    ) -> (Self, watch::Receiver<ProgressSnapshot>) {
        let chunks = ranges
            .iter()
            .map(|&(start, end)| ChunkProgress {
                start,
                current: start,
                end,
                state: ChunkState::Pending,
                elapsed_secs: 0.0,
                avg_speed: 0.0,
            })
            .collect();
        let snap = ProgressSnapshot {
            total_size,
            downloaded: 0,
            chunks,
            speed: 0.0,
            resumable,
        };
        let current = Arc::new(Mutex::new(snap.clone()));
        let (tx, rx) = match tx {
            Some(t) => {
                let r = t.subscribe();
                let _ = t.send(snap);
                (t, r)
            }
            None => watch::channel(snap),
        };
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

    /// 记录某分块退出时的耗时与平均写入速度（供分块汇总表使用）。
    pub fn set_chunk_timing(&self, idx: usize, elapsed_secs: f64, avg_speed: f64) {
        if let Some(c) = self
            .current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .chunks
            .get_mut(idx)
        {
            c.elapsed_secs = elapsed_secs;
            c.avg_speed = avg_speed;
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
