//! Phase 2 专用速度策略构造器。
//!
//! 与 `verify.rs` 一样单独成文件、只由需要它的测试 `#[path]` 引入：
//! `support/mod.rs` 会被每个测试 target 整体编译，放在库里会让其它 target
//! 因 dead_code 触发 clippy `-D warnings`。

use std::sync::Arc;

use crate::support::SpeedPolicy;

/// 所有非 Probe 响应统一限速（字节/秒）。
pub fn constant(bps: u64) -> SpeedPolicy {
    SpeedPolicy(Arc::new(move |_, _| Some(bps)))
}

/// 只有指定起点的那一个请求慢，其余请求完全不限速（"慢速第 0 号分块"）。
pub fn only_slow_at(slow_start: u64, slow_bps: u64) -> SpeedPolicy {
    SpeedPolicy(Arc::new(move |start, _| {
        if start == slow_start {
            Some(slow_bps)
        } else {
            None
        }
    }))
}

/// 起点落在 `[0, prefix_end)` 内的请求限速，其余请求不限速。
///
/// 用于"慢速分块 + 切分"场景：切出来的分块起点仍在慢速前缀内，因此它在被取消
/// 之前不会下完，续传时才能看到"未完成的切分分块"。
pub fn slow_in_prefix(prefix_end: u64, bps: u64) -> SpeedPolicy {
    SpeedPolicy(Arc::new(
        move |start, _| {
            if start < prefix_end { Some(bps) } else { None }
        },
    ))
}

/// 按种子为每个请求生成 0.25x ~ 2.0x 的确定性速度倍率（并发压力测试）。
pub fn seeded(seed: u64, base_bps: u64) -> SpeedPolicy {
    SpeedPolicy(Arc::new(move |_, request_no| {
        let h = (seed ^ request_no.wrapping_mul(0x9E37_79B9_7F4A_7C15))
            .wrapping_mul(0xBF58_476D_1CE4_E5B9);
        let frac = ((h >> 33) as f64) / (u32::MAX as f64 / 2.0);
        let mult = 0.25 + frac.clamp(0.0, 1.0) * 1.75;
        Some(((base_bps as f64) * mult).max(1.0) as u64)
    }))
}
