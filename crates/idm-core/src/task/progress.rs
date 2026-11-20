//! 下载进度快照与发布（`tokio::sync::watch`）。
//!
//! 发布频率不超过每 200ms 一次；没有读者时下载照常进行，不阻塞。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::task::state::ChunkState;

/// 一个分块的初始信息（Phase 2：新增数据库编号与来源）。
#[derive(Debug, Clone, Copy)]
pub struct ChunkInit {
    /// 数据库中的 `chunk_index`（汇总表里显示的编号）。
    pub db_index: i64,
    pub start: u64,
    pub end: u64,
    /// 来源：`None` 表示初始分块，`Some(i)` 表示从第 i 号分块切出。
    pub parent: Option<i64>,
    /// Phase 2.2：该分块是否由"接管"产生（汇总表的 source 列用来区分两种方式）。
    pub takeover: bool,
}

/// 单个分块的进度。
#[derive(Debug, Clone, Default)]
pub struct ChunkProgress {
    /// 数据库中的 `chunk_index`；未关联数据库时为 0。
    pub db_index: i64,
    /// 来源：`None` = 初始分块，`Some(i)` = 从第 i 号分块切出。
    pub parent: Option<i64>,
    /// Phase 2.2：接管产生（`source=takeover_from_N`）；否则为普通对半切。
    pub takeover: bool,
    pub start: u64,
    pub current: u64,
    pub end: u64,
    pub state: ChunkState,
    /// 分块从开始到退出的耗时（秒）。分块尚未退出时为 0。
    /// 用于 CLI 在下载结束时打印分块汇总表。
    pub elapsed_secs: f64,
    /// 分块平均写入速度（字节/秒）= 已写入字节数 / 耗时。
    pub avg_speed: f64,
    /// 该分块已发生的重试次数（Phase 1d）。
    pub retries: u64,
    /// Phase 5b-4b：该分块是否正在排队等待 Host 连接许可（只读展示字段）。
    /// 为 `true` 时它尚未发出请求、没有占连接槽位；桌面据此显示「排队」状态。
    pub waiting_for_permit: bool,
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
    /// 本次运行发生的动态切分次数（Phase 2，含接管）。
    pub split_count: u64,
    /// 其中属于 10.5「接管」的次数（Phase 2.2）。
    pub takeover_count: u64,
}

/// 进度发布器：内部持有共享快照与 `watch::Sender`，按 200ms 节流发布。
pub struct ProgressPublisher {
    current: Arc<Mutex<ProgressSnapshot>>,
    tx: watch::Sender<ProgressSnapshot>,
    last_publish: Instant,
    /// 速度滑动窗口：记录最近约 1 秒的 (时刻, 累计下载字节)。
    /// Phase 5b-4c：写入线程按批次更新 chunk.current，若用相邻两次 publish
    /// （200ms）算瞬时速度会因批次抖动出现数倍尖峰；用 1 秒窗口平均可平滑，
    /// 且仍能在一秒内反映真实速度变化。
    speed_window: VecDeque<(Instant, u64)>,
    /// 在窗口均值之上再叠加指数移动平均，平滑收尾阶段的单次批次跳变。
    speed_ema: Option<f64>,
}

/// 速度滑动窗口时长（字节数在此区间上的增量除以时间差）。
const SPEED_WINDOW: Duration = Duration::from_secs(1);
/// EMA 权重：新样本占 1/4，抑制单次写入批次的尖峰，同时约 4 次发布（0.8 秒）
/// 内即可跟上真实速度变化。
const SPEED_EMA_ALPHA: f64 = 0.25;

impl ProgressPublisher {
    /// 创建发布器与接收端。`chunks` 为各分块的初始信息（含数据库编号与来源）。
    ///
    /// 传入 `tx` 时复用外部通道（调用方可在下载开始前就订阅进度，用于实时刷新）；
    /// 传 `None` 则内部创建通道。无论哪种，发布都按 200ms 节流，且没有读者也不阻塞。
    pub fn new(
        total_size: Option<u64>,
        chunks: &[ChunkInit],
        tx: Option<watch::Sender<ProgressSnapshot>>,
        resumable: bool,
    ) -> (Self, watch::Receiver<ProgressSnapshot>) {
        let chunks = chunks
            .iter()
            .map(|c| ChunkProgress {
                db_index: c.db_index,
                parent: c.parent,
                takeover: c.takeover,
                start: c.start,
                current: c.start,
                end: c.end,
                state: ChunkState::Pending,
                elapsed_secs: 0.0,
                avg_speed: 0.0,
                retries: 0,
                waiting_for_permit: false,
            })
            .collect();
        let snap = ProgressSnapshot {
            total_size,
            downloaded: 0,
            chunks,
            speed: 0.0,
            resumable,
            split_count: 0,
            takeover_count: 0,
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
                speed_window: VecDeque::from([(now, 0)]),
                speed_ema: None,
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

    /// Phase 5b-4b：同步某分块是否正在排队等待 Host 许可（只读展示，不改下载逻辑）。
    pub fn set_chunk_waiting(&self, idx: usize, waiting: bool) {
        if let Some(c) = self
            .current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .chunks
            .get_mut(idx)
        {
            c.waiting_for_permit = waiting;
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

    /// 记录某分块的重试次数（Phase 1d，供分块汇总表使用）。
    pub fn set_chunk_retries(&self, idx: usize, retries: u64) {
        if let Some(c) = self
            .current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .chunks
            .get_mut(idx)
        {
            c.retries = retries;
        }
    }

    /// 追加一个切分出来的分块，返回它在快照中的下标（Phase 2）。
    pub fn add_chunk(&self, init: ChunkInit) -> usize {
        let mut snap = self.current.lock().unwrap_or_else(|e| e.into_inner());
        snap.chunks.push(ChunkProgress {
            db_index: init.db_index,
            parent: init.parent,
            takeover: init.takeover,
            start: init.start,
            current: init.start,
            end: init.end,
            state: ChunkState::Pending,
            elapsed_secs: 0.0,
            avg_speed: 0.0,
            retries: 0,
            waiting_for_permit: false,
        });
        snap.chunks.len() - 1
    }

    /// 更新某分块的区间终点（被切分后缩小，Phase 2）。
    pub fn set_chunk_end(&self, idx: usize, end: u64) {
        if let Some(c) = self
            .current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .chunks
            .get_mut(idx)
        {
            c.end = end;
        }
    }

    /// 切分次数加一（Phase 2，含接管）。
    pub fn add_split(&self) {
        self.current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .split_count += 1;
    }

    /// 10.5 接管次数加一（Phase 2.2，用于在日志/汇总中区分两种方式）。
    pub fn add_takeover(&self) {
        self.current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .takeover_count += 1;
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
        // 维护约 1 秒的速度窗口：追加当前样本，丢弃早于窗口起点的旧样本，
        // 始终保留至少一个样本作为计算基准。
        self.speed_window.push_back((now, downloaded));
        let cutoff = now.checked_sub(SPEED_WINDOW).unwrap_or(now);
        while self.speed_window.len() > 2 && self.speed_window[1].0 <= cutoff {
            self.speed_window.pop_front();
        }
        if let Some(&(base_time, base_bytes)) = self.speed_window.front() {
            let elapsed = now.saturating_duration_since(base_time).as_secs_f64();
            if elapsed > 0.0 {
                let window_speed = (downloaded.saturating_sub(base_bytes)) as f64 / elapsed;
                let smoothed = match self.speed_ema {
                    None => window_speed,
                    Some(prev) => SPEED_EMA_ALPHA * window_speed + (1.0 - SPEED_EMA_ALPHA) * prev,
                };
                self.speed_ema = Some(smoothed);
                snap.speed = smoothed;
            }
        }
        snap.downloaded = downloaded;
        let out = snap.clone();
        drop(snap);
        // 发送失败（无读者被 drop）忽略即可。
        let _ = self.tx.send(out);
        self.last_publish = now;
    }
}
