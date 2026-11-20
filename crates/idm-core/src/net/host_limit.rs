//! 按服务器（Host）限制并发连接数（Phase 3a）。
//!
//! 一个 [`HostConnectionLimiter`] 可以被多个并发的 `download` / `resume` 调用共享：
//! 内部按归一化后的 [`HostKey`] 各持有一个 `tokio::sync::Semaphore`，因此同一台
//! 服务器上的所有请求共享一个名额池，不同服务器互不干扰。
//!
//! 许可证 [`HostPermit`] 的生命周期严格等于**一次 HTTP 请求**：在 `req.send()`
//! 之前申请，读取响应流期间持有，请求结束 / 出错 / 被唤醒提前关闭连接时立即 drop。
//! 重试退避等待发生在单次请求函数返回之后，因此退避期间不会占用名额，
//! 下一次重试会重新申请。

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use reqwest::Url;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::error::{Error, Result};
use crate::task::cancel::CancelSignal;

/// 每个 Host 的默认并发连接上限。
pub const DEFAULT_MAX_PER_HOST: usize = 8;
/// 合法上限。
pub const MAX_PER_HOST_CEILING: usize = 64;

/// 归一化后的 Host：`(scheme, host, effective_port)`。
pub type HostKey = (String, String, u16);

/// 从 URL 提取归一化 Host。
pub fn host_key(url: &Url) -> Result<HostKey> {
    let scheme = url.scheme().to_ascii_lowercase();
    let host = url
        .host_str()
        .ok_or_else(|| Error::InvalidArgument {
            message: format!("URL has no host: {url}"),
        })?
        .to_ascii_lowercase();
    let port = url
        .port_or_known_default()
        .ok_or_else(|| Error::InvalidArgument {
            message: format!("URL has no effective port: {url}"),
        })?;
    Ok((scheme, host, port))
}

/// 单个 Host 的名额池：物理信号量 + 调小上限时的欠额。
///
/// `deficit` 表示「还需在许可归还时回收多少个」：调小上限时先 `forget_permits`
/// 回收空闲许可，空闲不足的部分记入欠额；之后每次 `HostPermit` 归还时，若欠额
/// 大于 0 就 CAS 减一并把该许可 `forget` 掉（不回到池、不唤醒等待者），直到欠额清零。
#[derive(Debug)]
struct HostSemaphore {
    sem: Arc<Semaphore>,
    deficit: AtomicUsize,
}

/// 按 Host 限制并发连接数。
#[derive(Debug)]
pub struct HostConnectionLimiter {
    max_per_host: AtomicUsize,
    semaphores: Mutex<HashMap<HostKey, Arc<HostSemaphore>>>,
}

impl Default for HostConnectionLimiter {
    fn default() -> Self {
        HostConnectionLimiter {
            max_per_host: AtomicUsize::new(DEFAULT_MAX_PER_HOST),
            semaphores: Mutex::new(HashMap::new()),
        }
    }
}

impl HostConnectionLimiter {
    /// `max_per_host` 必须在 1..=64；为 0（或 >64）返回配置错误。
    pub fn new(max_per_host: usize) -> Result<Self> {
        if !(1..=MAX_PER_HOST_CEILING).contains(&max_per_host) {
            return Err(Error::InvalidArgument {
                message: format!(
                    "max_connections_per_host must be in 1..={MAX_PER_HOST_CEILING}, got {max_per_host}"
                ),
            });
        }
        Ok(HostConnectionLimiter {
            max_per_host: AtomicUsize::new(max_per_host),
            semaphores: Mutex::new(HashMap::new()),
        })
    }

    pub fn max_per_host(&self) -> usize {
        self.max_per_host.load(Ordering::Acquire)
    }

    /// Phase 5b-4c：运行中即时调整每主机上限，`n` 必须在 1..=32。
    ///
    /// 调大：对每个已存在的 host 信号量 `add_permits(差值)`，唤醒排队分块。
    /// 调小：不中断已持有许可的连接——先 `forget_permits` 回收当前空闲许可，
    /// 空闲不足的部分记入欠额，由后续 `HostPermit` 归还时逐步回收。
    pub fn set_max_per_host(&self, n: usize) -> Result<()> {
        if !(1..=32).contains(&n) {
            return Err(Error::InvalidArgument {
                message: format!("max_connections_per_host must be in 1..=32, got {n}"),
            });
        }
        let old = self.max_per_host.swap(n, Ordering::AcqRel);
        if old == n {
            return Ok(());
        }
        let map = self.semaphores.lock().unwrap_or_else(|p| p.into_inner());
        for hs in map.values() {
            if n > old {
                hs.sem.add_permits(n - old);
            } else {
                let need = old - n;
                // 先回收空闲许可（available 之外的部分无法立刻回收）。
                let available = hs.sem.available_permits();
                let reclaimed = hs.sem.forget_permits(available.min(need));
                let debt = need - reclaimed;
                if debt > 0 {
                    hs.deficit.fetch_add(debt, Ordering::AcqRel);
                }
            }
        }
        Ok(())
    }

    fn semaphore_for(&self, url: &Url) -> Result<Arc<HostSemaphore>> {
        let key = host_key(url)?;
        let mut map = self.semaphores.lock().unwrap_or_else(|p| p.into_inner());
        let current = self.max_per_host.load(Ordering::Acquire);
        Ok(map
            .entry(key)
            .or_insert_with(|| {
                Arc::new(HostSemaphore {
                    sem: Arc::new(Semaphore::new(current)),
                    deficit: AtomicUsize::new(0),
                })
            })
            .clone())
    }

    /// 申请一个连接名额；等待期间响应取消信号。
    ///
    /// 返回的 [`HostPermit`] 一旦被 drop 就立即把名额还给该 Host 的信号量。
    pub async fn acquire(&self, url: &Url, cancel: &CancelSignal) -> Result<HostPermit> {
        let mut fut = self.acquire_future(url)?;
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(Error::Cancelled),
            p = &mut fut => p,
        }
    }

    /// 返回一个可复用的名额申请 future（Phase 3d：可被 `select!` 中断而不丢失排队位置）。
    ///
    /// `Semaphore::acquire_owned` 的 future 在第一次 poll 时进入公平等待队列；若因
    /// `select!` 的其它分支先完成而被 drop，排队位置会丢失，重新申请只能排到队尾。
    /// 本方法把该 future **pin 住并返回**：调用方用 `&mut fut` 反复 poll 同一个
    /// future，即可在并列监听取消 / `end` 缩小的同时保持原来的排队位置。
    pub fn acquire_future(&self, url: &Url) -> Result<HostPermitFuture> {
        let host = self.semaphore_for(url)?;
        Ok(HostPermitFuture {
            inner: Box::pin(async move {
                let permit = Arc::clone(&host.sem).acquire_owned().await.map_err(|_| {
                    Error::InvalidArgument {
                        message: "host connection semaphore closed".to_string(),
                    }
                })?;
                Ok(HostPermit {
                    permit: Some(permit),
                    host: Arc::clone(&host),
                })
            }),
        })
    }

    /// 指定 Host 当前可用的名额数（诊断 / 测试用）。
    pub fn available_permits(&self, url: &Url) -> Result<usize> {
        Ok(self.semaphore_for(url)?.sem.available_permits())
    }
}

/// 可被 `select!` 中断而不丢失 Semaphore 公平队列位置的名额申请 future。
///
/// 实现了 [`Future`]，且内部是 `Pin<Box<...>>`，因此自身 [`Unpin`]：调用方可以直接
/// 在 `tokio::select!` 里写 `p = &mut fut`，而不会因为其它分支先完成就 drop 它。
pub struct HostPermitFuture {
    inner: Pin<Box<dyn Future<Output = Result<HostPermit>> + Send>>,
}

impl Future for HostPermitFuture {
    type Output = Result<HostPermit>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.inner.as_mut().poll(cx)
    }
}

/// 一次 HTTP 请求的连接名额（RAII）。
#[derive(Debug)]
pub struct HostPermit {
    /// 用 `Option` 包装以便在 Drop 中取出：`forget()` 消费 permit 本身，
    /// 使其不归还到信号量池（调小上限后的欠额回收）。
    permit: Option<OwnedSemaphorePermit>,
    /// 所属 Host 的名额池（含欠额计数）：归还时据此判断是否回收许可。
    host: Arc<HostSemaphore>,
}

impl Drop for HostPermit {
    fn drop(&mut self) {
        let Some(permit) = self.permit.take() else {
            return;
        };
        // 欠额 > 0 时 CAS 减一，并把本次许可 forget 掉（不归还、不唤醒等待者）。
        // 多个归还并发：CAS 保证恰好欠额次数被回收，其余许可正常归还。
        let deficit = &self.host.deficit;
        let mut current = deficit.load(Ordering::Acquire);
        while current > 0 {
            match deficit.compare_exchange_weak(
                current,
                current - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    permit.forget();
                    return;
                }
                Err(actual) => current = actual,
            }
        }
        // 无欠额：permit 在此正常 drop，归还到信号量池。
        drop(permit);
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn host_key_normalizes_scheme_host_and_port() {
        let a = host_key(&Url::parse("http://Example.COM:80/a").unwrap()).unwrap();
        let b = host_key(&Url::parse("HTTP://example.com/a").unwrap()).unwrap();
        assert_eq!(a, b);
        assert_eq!(a, ("http".into(), "example.com".into(), 80));
        assert_eq!(
            host_key(&Url::parse("https://example.com/x").unwrap()).unwrap(),
            ("https".into(), "example.com".into(), 443)
        );
        assert_eq!(
            host_key(&Url::parse("http://127.0.0.1:8080/x").unwrap()).unwrap(),
            ("http".into(), "127.0.0.1".into(), 8080)
        );
    }

    #[test]
    fn zero_and_excessive_limits_are_rejected() {
        assert!(HostConnectionLimiter::new(0).is_err());
        assert!(HostConnectionLimiter::new(65).is_err());
        assert_eq!(HostConnectionLimiter::new(5).unwrap().max_per_host(), 5);
        assert_eq!(HostConnectionLimiter::default().max_per_host(), 8);
    }

    /// 名额在 drop 时立即释放；等待中的申请能被取消信号打断。
    #[tokio::test]
    async fn host_permit_releases_on_drop_and_wait_is_cancellable() {
        let url = Url::parse("http://example.com/f").unwrap();
        let limiter = HostConnectionLimiter::new(1).unwrap();
        let cancel = CancelSignal::new();
        let first = limiter.acquire(&url, &cancel).await.unwrap();
        let start = Instant::now();
        let second =
            tokio::time::timeout(Duration::from_millis(60), limiter.acquire(&url, &cancel)).await;
        assert!(
            second.is_err(),
            "second acquire must wait while permit is held"
        );
        drop(first);
        assert!(start.elapsed() < Duration::from_millis(500));
        assert!(
            limiter.acquire(&url, &cancel).await.is_ok(),
            "permit must be released on drop"
        );

        let limiter2 = HostConnectionLimiter::new(1).unwrap();
        let sig = CancelSignal::new();
        let Ok(held) = limiter2.acquire(&url, &sig).await else {
            panic!("permit must be available");
        };
        let s2 = sig.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            s2.cancel();
        });
        let cancelled = limiter2.acquire(&url, &sig).await;
        assert!(matches!(cancelled, Err(Error::Cancelled)));
        drop(held);
    }

    /// Phase 3d：`select!` 中断后继续 poll 同一个 `acquire_future`，必须保持
    /// Semaphore 公平队列中的位置——先入队的等待者要先于后入队者拿到名额。
    #[tokio::test]
    async fn acquire_future_keeps_queue_position_when_interrupted() {
        let url = Url::parse("http://example.com/f").unwrap();
        let limiter = HostConnectionLimiter::new(1).unwrap();
        let sig = CancelSignal::new();
        let held = limiter.acquire(&url, &sig).await.unwrap();

        // A 先入队，并在 select! 中被无关分支中断一次（future 本身不被 drop）。
        let mut a = limiter.acquire_future(&url).unwrap();
        tokio::select! {
            biased;
            _ = tokio::task::yield_now() => {}
            _ = &mut a => panic!("A must not complete while permit is held"),
        }
        // B 后入队。
        let mut b = limiter.acquire_future(&url).unwrap();
        tokio::select! {
            biased;
            _ = tokio::task::yield_now() => {}
            _ = &mut b => panic!("B must not complete while permit is held"),
        }
        assert_eq!(limiter.available_permits(&url).unwrap(), 0);

        drop(held);
        // 若 A 丢了排队位置（被重新排到 B 之后），这里会一直等 B 完成而挂起。
        let first = tokio::time::timeout(Duration::from_millis(500), &mut a).await;
        assert!(
            matches!(first, Ok(Ok(_))),
            "先入队且被中断过的等待者必须先拿到名额"
        );
        drop(first);
        let second = tokio::time::timeout(Duration::from_millis(500), &mut b).await;
        assert!(matches!(second, Ok(Ok(_))), "A 释放名额后 B 应拿到名额");
    }

    /// Phase 5b-4c：调大上限应立即唤醒正在排队的获取者。
    #[tokio::test]
    async fn raise_limit_releases_waiters() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let url = Url::parse("http://example.com/f")?;
        let limiter = HostConnectionLimiter::new(1)?;
        let cancel = CancelSignal::new();
        let first = limiter.acquire(&url, &cancel).await?;

        let mut second = limiter.acquire_future(&url)?;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut second)
                .await
                .is_err(),
            "调大前第二个获取者必须阻塞"
        );

        limiter.set_max_per_host(2)?;
        let acquired = tokio::time::timeout(Duration::from_millis(100), &mut second).await?;
        assert!(acquired.is_ok(), "调到 2 后等待者应在 100ms 内拿到许可");
        assert_eq!(limiter.max_per_host(), 2);
        drop(acquired);
        drop(first);
        Ok(())
    }

    /// Phase 5b-4c：调小上限不中断已持有者；空闲不够的部分记为欠额，
    /// 归还时优先抵扣欠额，稳定后同时持有不超过新上限。
    #[tokio::test]
    async fn lower_limit_drains_without_interrupting()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let url = Url::parse("http://example.com/f")?;
        let limiter = HostConnectionLimiter::new(4)?;
        let cancel = CancelSignal::new();
        let mut held = Vec::new();
        for _ in 0..4 {
            held.push(limiter.acquire(&url, &cancel).await?);
        }
        assert_eq!(limiter.available_permits(&url)?, 0);

        limiter.set_max_per_host(2)?;
        assert_eq!(held.len(), 4, "已持有的 4 个许可不应被中断");
        assert_eq!(limiter.max_per_host(), 2);
        assert_eq!(limiter.available_permits(&url)?, 0, "没有空闲可立即回收");

        // 归还全部 4 个：前 2 个被欠额吸收，后 2 个回到池中。
        drop(held.remove(0));
        drop(held.remove(0));
        assert_eq!(
            limiter.available_permits(&url)?,
            0,
            "欠额抵扣期间可用数仍为 0"
        );
        drop(held.remove(0));
        drop(held.remove(0));
        assert_eq!(limiter.available_permits(&url)?, 2, "稳定后池内应恰好 2 个");

        let mut a = limiter.acquire_future(&url)?;
        let mut b = limiter.acquire_future(&url)?;
        let mut c = limiter.acquire_future(&url)?;
        let ra = tokio::time::timeout(Duration::from_millis(100), &mut a).await?;
        let rb = tokio::time::timeout(Duration::from_millis(100), &mut b).await?;
        assert!(ra.is_ok(), "应拿到第 1 个许可");
        assert!(rb.is_ok(), "应拿到第 2 个许可");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut c)
                .await
                .is_err(),
            "第 3 个获取者必须阻塞（上限已降到 2）"
        );
        drop(ra);
        drop(rb);
        drop(c);
        Ok(())
    }

    /// Phase 5b-4c：4 → 1 → 3 交替调整，最终可同时持有恰好 3 个。
    #[tokio::test]
    async fn lower_then_raise_settles_correctly()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let url = Url::parse("http://example.com/f")?;
        let limiter = HostConnectionLimiter::new(4)?;
        let cancel = CancelSignal::new();
        let mut held = Vec::new();
        for _ in 0..4 {
            held.push(limiter.acquire(&url, &cancel).await?);
        }

        limiter.set_max_per_host(1)?;
        drop(held.remove(0));
        drop(held.remove(0));
        drop(held.remove(0));
        assert_eq!(held.len(), 1, "欠额抵扣后应只剩 1 个持有");
        assert_eq!(limiter.available_permits(&url)?, 0);

        limiter.set_max_per_host(3)?;
        assert_eq!(limiter.max_per_host(), 3);
        assert_eq!(
            limiter.available_permits(&url)?,
            2,
            "调大应立即补足 2 个空闲"
        );

        let mut a = limiter.acquire_future(&url)?;
        let mut b = limiter.acquire_future(&url)?;
        let mut c = limiter.acquire_future(&url)?;
        let ra = tokio::time::timeout(Duration::from_millis(100), &mut a).await?;
        let rb = tokio::time::timeout(Duration::from_millis(100), &mut b).await?;
        assert!(ra.is_ok() && rb.is_ok(), "调大后应能同时持有 3 个");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut c)
                .await
                .is_err(),
            "第 4 个获取者必须阻塞（上限为 3）"
        );
        drop(ra);
        drop(rb);
        drop(c);
        drop(held);
        Ok(())
    }
}
