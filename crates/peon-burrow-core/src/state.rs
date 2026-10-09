//! 可查询状态与取消源。
//!
//! 状态与日志**分开**（约定 C4）：控制面 / GUI / `doctor` 读 [`RelayState`]，
//! 日志走 `tracing` —— 从日志里解析状态是一条注定要修的弯路。
//!
//! 取消源用 `watch::channel(bool)` 自己实现，不引入 `tokio-util`：
//! 一处 `CancellationToken` 不值一个依赖，且省掉 feature 猜谜。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::SystemTime;

use tokio::sync::watch;

/// 中继对外可查询的状态快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayState {
    /// 是否正在监听。
    pub running: bool,
    /// 当前活着的连接数。
    pub active_connections: usize,
    /// 其中 watch 连接数。
    pub watch_connections: usize,
    /// 累计从客户端发往邮件服务器的字节数。
    pub bytes_to_server: u64,
    /// 累计从邮件服务器发往客户端的字节数。
    pub bytes_to_client: u64,
    /// 启动时间。
    pub started_at: SystemTime,
    /// 最近一次错误（给 `doctor` 与 GUI 看）。
    pub last_error: Option<String>,
}

impl RelayState {
    /// 还没启动时的状态。
    pub fn stopped() -> Self {
        Self {
            running: false,
            active_connections: 0,
            watch_connections: 0,
            bytes_to_server: 0,
            bytes_to_client: 0,
            started_at: SystemTime::now(),
            last_error: None,
        }
    }
}

/// 计数器：连接数与字节数。
///
/// 用原子而不是 `Mutex`（约定 C5）：这些值在每个数据块上都会变，
/// 加锁等于把锁放进热路径。
#[derive(Debug, Default)]
pub(crate) struct Metrics {
    active: AtomicUsize,
    watches: AtomicUsize,
    to_server: AtomicU64,
    to_client: AtomicU64,
    // C5-EXCEPTION: 只在出错时写一次，且**从不跨 await 持有**（对照 cancel 用的 watch 通道）；
    // 计数器本身是原子的，锁不在热路径上。
    last_error: std::sync::Mutex<Option<String>>,
}

impl Metrics {
    /// 连接建立。
    pub(crate) fn connection_opened(&self) {
        self.active.fetch_add(1, Ordering::Relaxed);
    }

    /// 连接关闭。
    pub(crate) fn connection_closed(&self) {
        self.active.fetch_sub(1, Ordering::Relaxed);
    }

    /// watch 连接建立。
    #[allow(dead_code, reason = "S5 的 watch 实现里使用")]
    pub(crate) fn watch_opened(&self) {
        self.watches.fetch_add(1, Ordering::Relaxed);
    }

    /// watch 连接关闭。
    #[allow(dead_code, reason = "S5 的 watch 实现里使用")]
    pub(crate) fn watch_closed(&self) {
        self.watches.fetch_sub(1, Ordering::Relaxed);
    }

    /// 记录一段发往邮件服务器的字节。
    pub(crate) fn add_to_server(&self, bytes: u64) {
        self.to_server.fetch_add(bytes, Ordering::Relaxed);
    }

    /// 记录一段发往客户端的字节。
    pub(crate) fn add_to_client(&self, bytes: u64) {
        self.to_client.fetch_add(bytes, Ordering::Relaxed);
    }

    /// 记下最近一次错误。
    ///
    /// ⚠️ 这把锁只在出错时碰一次，不在热路径上 —— 与上面的计数器不同。
    pub(crate) fn record_error(&self, message: impl Into<String>) {
        if let Ok(mut slot) = self.last_error.lock() {
            *slot = Some(message.into());
        }
    }

    /// 当前连接数。
    pub(crate) fn active(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    /// 打一份快照。
    pub(crate) fn snapshot(&self, running: bool, started_at: SystemTime) -> RelayState {
        RelayState {
            running,
            active_connections: self.active.load(Ordering::Relaxed),
            watch_connections: self.watches.load(Ordering::Relaxed),
            bytes_to_server: self.to_server.load(Ordering::Relaxed),
            bytes_to_client: self.to_client.load(Ordering::Relaxed),
            started_at,
            last_error: self.last_error.lock().ok().and_then(|slot| slot.clone()),
        }
    }
}

/// 取消源：`stop()` 时让所有后台任务退出。
#[derive(Debug, Clone)]
pub(crate) struct Cancel {
    tx: watch::Sender<bool>,
}

impl Cancel {
    /// 新建一个未取消的取消源。
    pub(crate) fn new() -> Self {
        let (tx, _rx) = watch::channel(false);
        Self { tx }
    }

    /// 取消。
    ///
    /// ⚠️ 用 `send_replace` 而不是 `send`：`send` 在**还没有订阅者**时返回 `Err` 且
    /// **不更新值**，于是稍后才订阅的人会看到「没被取消」，然后永远等一个不会来的变化。
    /// 这不是假想：`Cancel::new()` 之后先 `cancel()` 再 `subscribe()` 就会挂住。
    pub(crate) fn cancel(&self) {
        self.tx.send_replace(true);
    }

    /// 订阅取消信号。
    pub(crate) fn subscribe(&self) -> CancelToken {
        CancelToken {
            rx: self.tx.subscribe(),
        }
    }
}

/// 一个订阅者持有的取消令牌。
#[derive(Debug, Clone)]
pub(crate) struct CancelToken {
    rx: watch::Receiver<bool>,
}

impl CancelToken {
    /// 是否已取消。
    pub(crate) fn is_cancelled(&self) -> bool {
        *self.rx.borrow()
    }

    /// 等到被取消。
    pub(crate) async fn cancelled(&mut self) {
        if self.is_cancelled() {
            return;
        }
        while self.rx.changed().await.is_ok() {
            if self.is_cancelled() {
                return;
            }
        }
    }
}

/// 共享计数器的类型别名。
pub(crate) type SharedMetrics = Arc<Metrics>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_add_up() {
        let metrics = Metrics::default();
        metrics.connection_opened();
        metrics.connection_opened();
        metrics.connection_closed();
        assert_eq!(metrics.active(), 1);

        metrics.watch_opened();
        metrics.add_to_server(100);
        metrics.add_to_client(2048);
        metrics.record_error("boom");

        let state = metrics.snapshot(true, SystemTime::UNIX_EPOCH);
        assert!(state.running);
        assert_eq!(state.active_connections, 1);
        assert_eq!(state.watch_connections, 1);
        assert_eq!(state.bytes_to_server, 100);
        assert_eq!(state.bytes_to_client, 2048);
        assert_eq!(state.last_error.as_deref(), Some("boom"));
    }

    #[tokio::test]
    async fn cancel_wakes_subscribers() {
        let cancel = Cancel::new();
        let mut token = cancel.subscribe();
        assert!(!token.is_cancelled());
        cancel.cancel();
        token.cancelled().await;
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn an_already_cancelled_token_returns_immediately() {
        let cancel = Cancel::new();
        cancel.cancel();
        let mut token = cancel.subscribe();
        token.cancelled().await;
        assert!(token.is_cancelled());
    }
}
