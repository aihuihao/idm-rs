//! 取消信号：基于 `tokio::sync::watch` 的**状态广播**。
//!
//! 不使用 `Notify`：后者只唤醒**当时正在等待**的任务，若取消发生在任务开始等待
//! 之前（Probe 阶段、分块启动之前）就会被永久漏掉。`watch::channel(false)` 保存的是
//! 持久状态，任何时刻（包括等待间隙）触发的取消，后来者读取当前值即可看到；
//! `subscribe()` 得到的独立接收端也不会互相消费通知。

use tokio::sync::watch;

/// 可共享的取消信号。`cancel()` 把状态置为 `true`，此后所有观察者都能看到。
#[derive(Debug, Clone)]
pub struct CancelSignal {
    tx: watch::Sender<bool>,
    rx: watch::Receiver<bool>,
}

impl Default for CancelSignal {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelSignal {
    /// 创建未取消的信号。
    pub fn new() -> Self {
        let (tx, rx) = watch::channel(false);
        CancelSignal { tx, rx }
    }

    /// 触发取消；重复调用无害。所有订阅者都会看到 `true`。
    pub fn cancel(&self) {
        let _ = self.tx.send(true);
    }

    /// 同步检查当前是否已取消（不等待）。供分块任务在读网络/写磁盘之前检查。
    pub fn is_cancelled(&self) -> bool {
        *self.rx.borrow()
    }

    /// 订阅一个独立接收端，供分块任务用 `select!` 监听。
    pub fn subscribe(&self) -> watch::Receiver<bool> {
        self.tx.subscribe()
    }

    /// 等待取消发生；若已取消则立即返回。状态语义保证不会漏掉取消。
    pub async fn cancelled(&self) {
        let mut rx = self.tx.subscribe();
        wait_cancelled(&mut rx).await;
    }
}

/// 等待某个接收端观察到取消。供 `select!` 与网络读取同时监听。
pub(crate) async fn wait_cancelled(rx: &mut watch::Receiver<bool>) {
    while !*rx.borrow() {
        // 发送端由 `CancelSignal` 持有；若发送端全部 drop，视为取消，避免永久挂起。
        if rx.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancellation_before_wait_is_not_lost() {
        let sig = CancelSignal::new();
        // 取消发生在任何等待开始之前；`cancelled()` 必须立即返回。
        sig.cancel();
        assert!(sig.is_cancelled());
        let r = tokio::time::timeout(std::time::Duration::from_millis(100), sig.cancelled()).await;
        assert!(
            r.is_ok(),
            "cancelled() must return immediately when already cancelled"
        );
    }

    #[tokio::test]
    async fn all_subscribers_observe_cancellation() {
        let sig = CancelSignal::new();
        let a = sig.subscribe();
        let b = sig.subscribe();
        sig.cancel();
        assert!(*a.borrow());
        assert!(*b.borrow());
        assert!(sig.is_cancelled());
    }

    #[tokio::test]
    async fn not_cancelled_by_default() {
        let sig = CancelSignal::default();
        assert!(!sig.is_cancelled());
        let pending =
            tokio::time::timeout(std::time::Duration::from_millis(20), sig.cancelled()).await;
        assert!(pending.is_err(), "must keep waiting while not cancelled");
    }
}
