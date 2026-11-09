//! 令牌桶限速器：一次下载的所有分块共用同一个实例，限制的是这次下载的**总速度**。
//!
//! ## 设计
//!
//! - 状态只有一个桶：当前令牌数 `tokens`（单位：字节）、上次补充时刻 `last`、
//!   当前速率 `rate` 与桶容量 `capacity`，用 `std::sync::Mutex` 保护。
//!   `Arc<RateLimiter>` 可以安全地在所有分块任务间共享；临界区内只做算术与
//!   `Instant::now()`，绝不跨 `.await` 持锁。
//! - 桶容量 = 1 秒配额（`rate`），最小 256 KiB。上限封顶防止长时间空闲后积累出
//!   无限突发；下限保证低速率时也能容纳一个完整的响应块。
//! - `acquire(n)` 采用"预扣 + 欠债"模型：先把 `n` 从 `tokens` 里扣掉（允许为负），
//!   再按 `-tokens / rate` 精确 `tokio::time::sleep` 一次，醒来后按真实经过时间补充，
//!   直到欠债还清。多个分块并发时，后扣的请求排在更后面（欠债更深），因此总放行
//!   速率恒为 `rate`，而不是"每个连接各自 `rate`"。
//! - 桶初始为空（`tokens = 0`）：所有字节都要先等到令牌生成，`N` 字节的下载耗时
//!   约为 `N / rate`，不会白送 1 秒配额。
//!
//! ## Phase 3a 动态调整
//!
//! - `rate = 0`（`None` 或 `Some(0)`）表示不限速。`set_rate` 在运行中随时可调用：
//!   先按旧速率补齐到当前时刻，再写入新速率 / 新容量，并把 `tokens` 截断到新容量
//!   上限（防止从 50 MiB/s 调到 1 MiB/s 时旧令牌造成突发）。
//! - 每次 `set_rate` 递增一个 `watch` 代际号。`acquire` 把 `sleep(wait)` 与
//!   `generation.changed()` 并列，速率一变立刻重新计算等待时间：改为不限速或调高速率
//!   时等待中的任务立即被唤醒，100ms 内按新速率生效。
//! - 本类型不含取消逻辑。调用方用 `tokio::select!` 把 `acquire` 与取消信号并列；
//!   `acquire` 是可取消的 future（不持有跨 await 的锁），取消时直接丢弃即可。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::watch;

/// 桶容量下限：256 KiB。
const MIN_CAPACITY: u64 = 256 * 1024;

/// 最小单次等待，避免计时精度导致的忙循环。
const MIN_WAIT_SECS: f64 = 0.001;

/// 桶状态。`rate = 0` 表示不限速。
#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last: Instant,
    rate: f64,
    capacity: f64,
}

/// 共享限速器。用 `Arc` 传给同一次下载的每个分块任务。
#[derive(Debug)]
pub struct RateLimiter {
    bucket: Mutex<Bucket>,
    /// 每次 `set_rate` 递增；等待中的 `acquire` 订阅它以便立刻重算等待时间。
    generation: watch::Sender<u64>,
}

impl RateLimiter {
    /// 创建限速器。`None` / `Some(0)` 表示不限速。
    pub fn new(bytes_per_sec: Option<u64>) -> Self {
        let rate = bytes_per_sec.unwrap_or(0) as f64;
        let capacity = rate.max(MIN_CAPACITY as f64);
        RateLimiter {
            bucket: Mutex::new(Bucket {
                tokens: 0.0,
                last: Instant::now(),
                rate,
                capacity,
            }),
            generation: watch::channel(0).0,
        }
    }

    /// 当前限速值（字节/秒）；不限速时返回 `None`。
    pub fn current_rate(&self) -> Option<u64> {
        let b = self.lock();
        if b.rate <= 0.0 {
            None
        } else {
            Some(b.rate as u64)
        }
    }

    /// 运行中修改限速值，`None` / `Some(0)` 表示改为不限速。
    ///
    /// 先按旧速率补齐到当前时刻，再换速率 / 容量并把剩余令牌截断到新容量；
    /// 最后递增代际号唤醒所有等待中的 `acquire`。
    pub fn set_rate(&self, bytes_per_sec: Option<u64>) {
        let rate = bytes_per_sec.unwrap_or(0) as f64;
        let capacity = rate.max(MIN_CAPACITY as f64);
        {
            let mut b = self.lock();
            let now = Instant::now();
            self.refill(&mut b, now);
            b.rate = rate;
            b.capacity = capacity;
            // 把当前剩余令牌截断到新桶容量上限（1 秒配额，最小 256 KiB）：
            // 高限速积攒的旧令牌不能带到低限速下突发。
            // 另外清掉旧速率下预扣的欠债（这些字节已经收到，按新速率再"偿还"
            // 一次只会造成额外停顿），使新速率从当前时刻立即生效。
            b.tokens = b.tokens.min(capacity).max(0.0);
        }
        self.generation.send_modify(|g| *g += 1);
    }

    /// 配置的限速值（字节/秒），不限速时为 0。保留给既有调用方/测试使用。
    pub fn bytes_per_sec(&self) -> u64 {
        self.lock().rate as u64
    }

    /// 桶容量（字节）。
    pub fn capacity(&self) -> u64 {
        self.lock().capacity as u64
    }

    /// 仅供单元测试读取当前令牌数（不触发补充）。
    #[cfg(test)]
    fn tokens(&self) -> f64 {
        self.lock().tokens
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Bucket> {
        self.bucket.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// 按真实经过时间补充令牌，封顶 `capacity`。不限速时不补充。
    fn refill(&self, b: &mut Bucket, now: Instant) {
        let elapsed = now.saturating_duration_since(b.last).as_secs_f64();
        if b.rate > 0.0 {
            b.tokens = (b.tokens + elapsed * b.rate).min(b.capacity);
        }
        b.last = now;
    }

    /// 领取 `n` 个字节的配额；令牌不足时等待。速率变化会立刻打断等待并重算。
    ///
    /// 调用方应在"从网络读到一块数据之后、交给写入线程池之前"调用它：读取被延后
    /// 后 TCP 接收窗口填满，服务器端随之放慢发送，从而真正限制网络流量。
    pub async fn acquire(&self, n: u64) {
        if n == 0 {
            return;
        }
        // 先订阅（状态语义，set_rate 发生在订阅之后不会漏掉通知）。
        let mut generation = self.generation.subscribe();
        generation.borrow_and_update();
        {
            let mut b = self.lock();
            if b.rate <= 0.0 {
                return;
            }
            self.refill(&mut b, Instant::now());
            b.tokens -= n as f64;
        }
        loop {
            let wait = {
                let mut b = self.lock();
                if b.rate <= 0.0 {
                    return;
                }
                self.refill(&mut b, Instant::now());
                if b.tokens >= 0.0 {
                    return;
                }
                (-b.tokens / b.rate).max(MIN_WAIT_SECS)
            };
            tokio::select! {
                biased;
                _ = generation.changed() => {}
                _ = tokio::time::sleep(Duration::from_secs_f64(wait)) => {}
            }
        }
    }
}

/// Phase 3a 双层限速：单任务限速 + 多个任务共享的全局限速。
///
/// 每读到一块网络数据，依次通过两个限速器；调用方把整个 `acquire` future 与
/// `CancelSignal` 并列在 `select!` 中，等待任意一个配额时都能立刻响应取消。
#[derive(Debug, Clone, Default)]
pub struct BandwidthLimiter {
    pub task_limiter: Option<Arc<RateLimiter>>,
    pub global_limiter: Option<Arc<RateLimiter>>,
}

impl BandwidthLimiter {
    pub fn new(
        task_limiter: Option<Arc<RateLimiter>>,
        global_limiter: Option<Arc<RateLimiter>>,
    ) -> Self {
        BandwidthLimiter {
            task_limiter,
            global_limiter,
        }
    }

    /// 两个限速器都没有配置时为真，调用方可以跳过 `select!`。
    pub fn is_unlimited(&self) -> bool {
        self.task_limiter.is_none() && self.global_limiter.is_none()
    }

    /// 依次满足单任务与全局配额。
    pub async fn acquire(&self, n: u64) {
        if let Some(t) = &self.task_limiter {
            t.acquire(n).await;
        }
        if let Some(g) = &self.global_limiter {
            g.acquire(n).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::task::JoinSet;

    use super::*;

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn capacity_is_one_second_quota_with_256kib_floor() {
        assert_eq!(RateLimiter::new(Some(2 * MIB)).capacity(), 2 * MIB);
        assert_eq!(RateLimiter::new(Some(1024)).capacity(), 256 * 1024);
        assert_eq!(RateLimiter::new(Some(0)).capacity(), 256 * 1024);
        assert_eq!(RateLimiter::new(None).capacity(), 256 * 1024);
        assert_eq!(RateLimiter::new(Some(2 * MIB)).bytes_per_sec(), 2 * MIB);
        assert_eq!(RateLimiter::new(None).current_rate(), None);
    }

    /// Phase 3a：`set_rate` 立刻改速率、截断旧令牌，并唤醒等待中的 `acquire`。
    #[tokio::test]
    async fn dynamic_rate_change_clamps_tokens_and_wakes_waiters() {
        let rl = Arc::new(RateLimiter::new(Some(50 * MIB)));
        assert_eq!(rl.current_rate(), Some(50 * MIB));
        assert_eq!(rl.capacity(), 50 * MIB);
        // 高限速下空闲一段时间会积攒令牌；降到 1 MiB/s 后必须被截断，
        // 不能把旧速率的令牌带到新速率下突发。
        tokio::time::sleep(Duration::from_millis(40)).await;
        rl.set_rate(Some(MIB));
        assert_eq!(rl.current_rate(), Some(MIB));
        assert_eq!(rl.capacity(), MIB);
        assert!(rl.tokens() <= MIB as f64, "旧令牌必须截断到新桶容量以内");
        assert_eq!(
            rl.tokens(),
            MIB as f64,
            "50 MiB/s 下空闲攒出的令牌必须被截到 1 MiB 的新容量"
        );

        // 一个需要等很久的 acquire；改为不限速后必须在 100ms 内被唤醒。
        let r = Arc::clone(&rl);
        let mut set = JoinSet::new();
        set.spawn(async move { r.acquire(64 * MIB).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let started = Instant::now();
        rl.set_rate(None);
        assert_eq!(rl.current_rate(), None);
        tokio::time::timeout(Duration::from_millis(100), async {
            while set.join_next().await.is_some() {}
        })
        .await
        .expect("waiting acquire must wake within 100ms after set_rate(None)");
        assert!(started.elapsed() < Duration::from_millis(100));

        // 调低速率时容量与当前速率立即生效。
        rl.set_rate(Some(MIB));
        assert_eq!(rl.current_rate(), Some(MIB));
        assert_eq!(rl.capacity(), MIB);
    }

    #[tokio::test]
    async fn acquire_waits_for_one_second_quota() {
        let rl = RateLimiter::new(Some(256 * 1024));
        let start = Instant::now();
        rl.acquire(256 * 1024).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(800),
            "too fast: {elapsed:?}"
        );
        assert!(
            elapsed <= Duration::from_millis(1800),
            "too slow: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn concurrent_acquires_share_one_bucket() {
        let rl = Arc::new(RateLimiter::new(Some(512 * 1024)));
        let mut set = JoinSet::new();
        let start = Instant::now();
        for _ in 0..4 {
            let r = Arc::clone(&rl);
            set.spawn(async move { r.acquire(256 * 1024).await });
        }
        while set.join_next().await.is_some() {}
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(1500),
            "too fast: {elapsed:?}"
        );
        assert!(
            elapsed <= Duration::from_millis(3200),
            "too slow: {elapsed:?}"
        );
    }

    /// 双层限速：两个桶都必须满足，且 global 可与另一个任务共享。
    #[tokio::test]
    async fn bandwidth_limiter_applies_both_layers() {
        let global = Arc::new(RateLimiter::new(Some(256 * 1024)));
        let bw = BandwidthLimiter::new(
            Some(Arc::new(RateLimiter::new(Some(4 * MIB)))),
            Some(global),
        );
        assert!(!bw.is_unlimited());
        let start = Instant::now();
        bw.acquire(256 * 1024).await;
        assert!(start.elapsed() >= Duration::from_millis(800));
        assert!(BandwidthLimiter::default().is_unlimited());
    }
}
