//! 令牌桶限速器：一次下载的所有分块共用同一个实例，限制的是这次下载的**总速度**。
//!
//! ## 设计
//!
//! - 状态只有一个桶：当前令牌数 `tokens`（单位：字节）与上次补充时刻 `last`，
//!   用 `std::sync::Mutex` 保护。`Arc<RateLimiter>` 可以安全地在所有分块任务间共享；
//!   临界区内只做算术与 `Instant::now()`，绝不跨 `.await` 持锁。
//! - 桶容量 = 1 秒配额（`bytes_per_sec`），最小 256 KiB。上限封顶防止长时间空闲后
//!   积累出无限突发；下限保证低速率时也能容纳一个完整的响应块。
//! - `acquire(n)` 采用"预扣 + 欠债"模型：先把 `n` 从 `tokens` 里扣掉（允许为负），
//!   再按 `-tokens / rate` 精确 `tokio::time::sleep` 一次，醒来后按真实经过时间补充，
//!   直到欠债还清。多个分块并发时，后扣的请求排在更后面（欠债更深），因此总放行
//!   速率恒为 `rate`，而不是"每个连接各自 `rate`"。
//! - 桶初始为空（`tokens = 0`）：所有字节都要先等到令牌生成，`N` 字节的下载耗时
//!   约为 `N / rate`，不会白送 1 秒配额。
//! - 本类型不含取消逻辑。调用方用 `tokio::select!` 把 `acquire` 与取消信号并列；
//!   `acquire` 是可取消的 future（不持有跨 await 的锁），取消时直接丢弃即可。
//!   被丢弃的 `acquire` 会留下未偿还的欠债，但限速器随该次下载一起销毁，
//!   不会影响后续下载。

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 桶容量下限：256 KiB。
const MIN_CAPACITY: u64 = 256 * 1024;

/// 最小单次等待，避免计时精度导致的忙循环。
const MIN_WAIT_SECS: f64 = 0.001;

/// 桶状态。
#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

/// 共享限速器。用 `Arc` 传给同一次下载的每个分块任务。
#[derive(Debug)]
pub struct RateLimiter {
    bucket: Mutex<Bucket>,
    rate: f64,
    capacity: f64,
}

impl RateLimiter {
    /// 创建限速器，`bytes_per_sec` 为整次下载允许的总字节/秒。
    ///
    /// 桶容量取 `max(bytes_per_sec, 256 KiB)`；`bytes_per_sec` 为 0 时按 1 处理，
    /// 避免除零（实际用途中不会传入 0）。
    pub fn new(bytes_per_sec: u64) -> Self {
        let rate = bytes_per_sec.max(1) as f64;
        let capacity = rate.max(MIN_CAPACITY as f64);
        RateLimiter {
            bucket: Mutex::new(Bucket {
                tokens: 0.0,
                last: Instant::now(),
            }),
            rate,
            capacity,
        }
    }

    /// 配置的限速值（字节/秒）。
    pub fn bytes_per_sec(&self) -> u64 {
        self.rate as u64
    }

    /// 桶容量（字节）。
    pub fn capacity(&self) -> u64 {
        self.capacity as u64
    }

    /// 按真实经过时间补充令牌，封顶 `capacity`。
    fn refill(&self, b: &mut Bucket, now: Instant) {
        let elapsed = now.duration_since(b.last).as_secs_f64();
        b.tokens = (b.tokens + elapsed * self.rate).min(self.capacity);
        b.last = now;
    }

    /// 领取 `n` 个字节的配额；令牌不足时用 `tokio::time::sleep` 等待。
    ///
    /// 调用方应在"从网络读到一块数据之后、交给写入线程池之前"调用它：读取被延后
    /// 后 TCP 接收窗口填满，服务器端随之放慢发送，从而真正限制网络流量。
    pub async fn acquire(&self, n: u64) {
        if n == 0 {
            return;
        }
        {
            let mut b = self.bucket.lock().unwrap_or_else(|p| p.into_inner());
            self.refill(&mut b, Instant::now());
            b.tokens -= n as f64;
        }
        loop {
            let wait = {
                let mut b = self.bucket.lock().unwrap_or_else(|p| p.into_inner());
                self.refill(&mut b, Instant::now());
                if b.tokens >= 0.0 {
                    return;
                }
                (-b.tokens / self.rate).max(MIN_WAIT_SECS)
            };
            tokio::time::sleep(Duration::from_secs_f64(wait)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::task::JoinSet;

    use super::*;

    #[test]
    fn capacity_is_one_second_quota_with_256kib_floor() {
        assert_eq!(
            RateLimiter::new(2 * 1024 * 1024).capacity(),
            2 * 1024 * 1024
        );
        assert_eq!(RateLimiter::new(1024).capacity(), 256 * 1024);
        assert_eq!(RateLimiter::new(0).capacity(), 256 * 1024);
        assert_eq!(
            RateLimiter::new(2 * 1024 * 1024).bytes_per_sec(),
            2 * 1024 * 1024
        );
    }

    #[tokio::test]
    async fn acquire_waits_for_one_second_quota() {
        let rl = RateLimiter::new(256 * 1024);
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
        // 4 个并发任务各领 256 KiB；总限速 512 KiB/s → 合计应约 2 秒，
        // 若每个任务各自限速则只需约 0.5 秒。
        let rl = Arc::new(RateLimiter::new(512 * 1024));
        let mut set = JoinSet::new();
        let start = Instant::now();
        for _ in 0..4 {
            let r = rl.clone();
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
}
