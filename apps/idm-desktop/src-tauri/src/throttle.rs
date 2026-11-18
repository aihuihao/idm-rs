//! 事件节流器：相同内容或间隔不足时不推送。

use std::time::{Duration, Instant};

pub struct Throttle {
    min_interval: Duration,
    last_emit: Option<Instant>,
    last_snapshot: Option<Vec<u8>>,
}

impl Throttle {
    pub fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            last_emit: None,
            last_snapshot: None,
        }
    }

    /// 内容与上次推送相同返回 false；距上次推送不足 min_interval 返回 false；
    /// 否则返回 true 并记录本次时间与内容。
    pub fn should_emit(&mut self, now: Instant, snapshot: &[u8]) -> bool {
        if self.last_snapshot.as_deref() == Some(snapshot) {
            return false;
        }
        if let Some(last) = self.last_emit
            && now.saturating_duration_since(last) < self.min_interval
        {
            return false;
        }
        self.last_emit = Some(now);
        self.last_snapshot = Some(snapshot.to_vec());
        true
    }
}
