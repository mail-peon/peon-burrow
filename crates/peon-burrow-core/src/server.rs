//! WebSocket 服务端：接受连接、策略检查、第一帧分流、交给隧道。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use futures_util::{SinkExt, StreamExt};
use peon_burrow_protocol::{
    POLICY_VIOLATION, TargetResolve, WATCH_FAILED, resolve_target, truncate_close_reason,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::Utf8Bytes;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tracing::{debug, error, info, warn};

use crate::dispatch::{Dispatch, classify};
use crate::error::RelayError;
use crate::options::RelayOptions;
use crate::policy::Policy;
use crate::state::{Cancel, CancelToken, Metrics, RelayState, SharedMetrics};
use crate::transport::{self, TlsConfig};
use crate::tunnel;

/// 中继服务句柄。
///
/// 生命周期是显式的（工厂 + `start` / `stop`），不在模块加载时监听 ——
/// 这样测试才能在同进程里起两个实例、也才能干净地重启。
pub struct RelayServer {
    local_addr: SocketAddr,
    cancel: Cancel,
    state_rx: watch::Receiver<RelayState>,
    metrics: SharedMetrics,
    options: RelayOptions,
    running: Arc<AtomicBool>,
    started_at: SystemTime,
}

impl std::fmt::Debug for RelayServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayServer")
            .field("local_addr", &self.local_addr)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl RelayServer {
    /// 用默认策略与默认 TLS 配置启动。
    pub async fn start(options: RelayOptions) -> Result<Self, RelayError> {
        Self::start_with(
            options,
            Arc::new(crate::policy::PolicyRules::default()),
            TlsConfig::default(),
        )
        .await
    }

    /// 启动，并注入自定义策略与 TLS 配置（接入点见 `ai-docs/modules.md § 10`）。
    pub async fn start_with(
        options: RelayOptions,
        policy: Arc<dyn Policy>,
        tls: TlsConfig,
    ) -> Result<Self, RelayError> {
        let listener = TcpListener::bind((options.host.as_str(), options.port))
            .await
            .map_err(|error| RelayError::from_bind_error(options.port, error))?;
        let local_addr = listener.local_addr()?;
        let started_at = SystemTime::now();

        let metrics: SharedMetrics = Arc::new(Metrics::default());
        let (state_tx, state_rx) = watch::channel(RelayState {
            running: true,
            started_at,
            ..RelayState::stopped()
        });
        let cancel = Cancel::new();
        let running = Arc::new(AtomicBool::new(true));

        let accept_cancel = cancel.subscribe();
        let accept_metrics = SharedMetrics::clone(&metrics);
        let accept_options = options.clone();
        tokio::spawn(async move {
            accept_loop(
                listener,
                accept_options,
                policy,
                tls,
                accept_metrics,
                accept_cancel,
            )
            .await;
        });

        // 状态发布：每秒一次快照（控制面/GUI 需要「变化」而不是每一字节都推）
        let publish_metrics = SharedMetrics::clone(&metrics);
        let publish_running = Arc::clone(&running);
        let mut publish_cancel = cancel.subscribe();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = publish_cancel.cancelled() => break,
                    _ = ticker.tick() => {
                        let _ = state_tx.send(publish_metrics.snapshot(publish_running.load(Ordering::Relaxed), started_at));
                    }
                }
            }
            let _ = state_tx.send(publish_metrics.snapshot(false, started_at));
        });

        info!(addr = %local_addr, "relay listening");
        Ok(Self {
            local_addr,
            cancel,
            state_rx,
            metrics,
            options,
            running,
            started_at,
        })
    }

    /// **立刻**打一份状态快照，不等发布 tick。
    ///
    /// 控制面的 `status` / `doctor` 走这条路径：它们要的是「现在」，不是「最多一秒前」。
    pub fn snapshot(&self) -> RelayState {
        self.metrics
            .snapshot(self.running.load(Ordering::Relaxed), self.started_at)
    }

    /// 实际监听的地址（`port: 0` 时用它取内核分配的端口）。
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// 状态快照句柄：**不借用服务句柄**，可以交给控制面长期持有。
    ///
    /// 有了它，控制面的状态闭包就不必把整个 `RelayServer` 塞进 `Arc` ——
    /// 而 `stop(self)` 需要独占，塞进 `Arc` 会让「怎么停下来」变成一道难题。
    pub fn snapshot_handle(&self) -> Arc<dyn Fn() -> RelayState + Send + Sync> {
        let metrics = SharedMetrics::clone(&self.metrics);
        let running = Arc::clone(&self.running);
        let started_at = self.started_at;
        Arc::new(move || metrics.snapshot(running.load(Ordering::Relaxed), started_at))
    }

    /// 可查询状态的订阅端。
    pub fn state(&self) -> watch::Receiver<RelayState> {
        self.state_rx.clone()
    }

    /// 扩展里该填的地址。
    pub fn url(&self) -> String {
        let host = if self.options.host.contains(':') {
            format!("[{}]", self.options.host)
        } else {
            self.options.host.clone()
        };
        format!("ws://{host}:{}/", self.local_addr.port())
    }

    /// 停止：先取消、再等连接断开（**主动断开**，否则 IMAP 的 IDLE 会把关停卡住）。
    pub async fn stop(self) -> Result<(), RelayError> {
        self.running.store(false, Ordering::Relaxed);
        self.cancel.cancel();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while self.metrics.active() > 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        info!(stopped = true, "relay stopped");
        Ok(())
    }
}

/// 接受循环。
async fn accept_loop(
    listener: TcpListener,
    options: RelayOptions,
    policy: Arc<dyn Policy>,
    tls: TlsConfig,
    metrics: SharedMetrics,
    mut cancel: CancelToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, peer)) => {
                        let policy = Arc::clone(&policy);
                        let tls = tls.clone();
                        let metrics = SharedMetrics::clone(&metrics);
                        let cancel = cancel.clone();
                        let options = options.clone();
                        tokio::spawn(async move {
                            if let Err(error) = handle_connection(stream, peer, options, policy, tls, metrics, cancel).await {
                                debug!(%peer, %error, "connection ended");
                            }
                        });
                    }
                    Err(error) => {
                        error!(%error, "accept failed");
                        metrics.record_error(format!("accept 失败：{error}"));
                    }
                }
            }
        }
    }
}

/// 连接级错误（只进日志，不影响进程）。
#[derive(Debug, thiserror::Error)]
enum ConnectionError {
    #[error("握手失败：{0}")]
    Handshake(String),
    #[error("WebSocket 协议错误：{0}")]
    Protocol(String),
    #[error("隧道错误：{0}")]
    Tunnel(#[from] tunnel::TunnelError),
}

/// 连接结束时把计数减回去（异常路径也要减）。
struct ConnectionGuard {
    metrics: SharedMetrics,
}

impl ConnectionGuard {
    fn new(metrics: SharedMetrics) -> Self {
        metrics.connection_opened();
        Self { metrics }
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.metrics.connection_closed();
    }
}

/// 处理一条连接：握手 → 策略 → 建上游 → 分流 → 搬运。
// `result_large_err`：tungstenite 的 `ErrorResponse` 天生就大，而它只在握手失败时构造
#[allow(clippy::result_large_err)]
async fn handle_connection(
    stream: TcpStream,
    peer: SocketAddr,
    options: RelayOptions,
    policy: Arc<dyn Policy>,
    tls: TlsConfig,
    metrics: SharedMetrics,
    cancel: CancelToken,
) -> Result<(), ConnectionError> {
    // 升级请求里的目标只有握手回调能看到，用一个 OnceLock 把它带出来
    let uri_slot: Arc<OnceLock<String>> = Arc::new(OnceLock::new());
    let slot = Arc::clone(&uri_slot);
    let mut ws = tokio_tungstenite::accept_hdr_async(
        stream,
        move |request: &Request, response: Response| {
            let _ = slot.set(request.uri().to_string());
            Ok(response)
        },
    )
    .await
    .map_err(|error| ConnectionError::Handshake(error.to_string()))?;

    let _guard = ConnectionGuard::new(SharedMetrics::clone(&metrics));

    if metrics.active() > options.max_connections {
        warn!(%peer, limit = options.max_connections, "too many connections");
        return close_with(&mut ws, POLICY_VIOLATION, "too many connections").await;
    }

    // ① 策略检查**同步**完成：被拒的连接一个字节都不发
    let raw_target = uri_slot.get().cloned().unwrap_or_default();
    let target = match resolve_target(&raw_target) {
        TargetResolve::Target(target) => {
            if let Err(reason) = policy.check(&target) {
                warn!(%peer, reason = reason.message(), "policy reject");
                return close_with(&mut ws, POLICY_VIOLATION, reason.message()).await;
            }
            Some(target)
        }
        // watch 请求故意不带目标：留着这条连接，等第一帧
        TargetResolve::NoTarget => None,
        TargetResolve::Rejected(reason) => {
            warn!(%peer, reason = reason.message(), "reject");
            return close_with(&mut ws, POLICY_VIOLATION, reason.message()).await;
        }
    };

    // ② 立刻建上游（不等第一帧）
    let upstream = match &target {
        Some(target) => {
            info!(%peer, host = %target.host, port = target.port, tls = target.tls, "connect upstream");
            match transport::connect(target, &tls, options.connect_timeout).await {
                Ok(upstream) => Some(upstream),
                Err(error) => {
                    let message = error.user_message();
                    metrics.record_error(message.clone());
                    warn!(%peer, %message, "upstream failed");
                    return close_with(&mut ws, POLICY_VIOLATION, &message).await;
                }
            }
        }
        None => None,
    };

    // ③ 第一帧分流
    let mut cancel = cancel;
    let first = tokio::select! {
        _ = cancel.cancelled() => return Ok(()),
        frame = ws.next() => frame,
    };
    let Some(first) = first else {
        return Ok(());
    };
    let first = first.map_err(|error| ConnectionError::Protocol(error.to_string()))?;

    match classify(&first, target.is_some()) {
        Dispatch::Reject(reason) => close_with(&mut ws, POLICY_VIOLATION, reason.message()).await,
        Dispatch::Watch(request) => {
            handle_watch(ws, *request, policy, tls, metrics, cancel, options).await
        }
        Dispatch::Passthrough => {
            let Some(upstream) = upstream else {
                return close_with(&mut ws, POLICY_VIOLATION, "缺少目标 host").await;
            };
            tunnel::run(
                ws,
                upstream,
                options.idle_timeout,
                metrics,
                cancel,
                Some(first),
            )
            .await
            .map_err(ConnectionError::from)
        }
    }
}

/// 用关闭帧拒绝一条连接。
///
/// ⚠️ 关闭原因**必须**按字节截断到 120 以内：WebSocket 规范上限 123 字节，
/// 超了 `close()` 会失败 —— TS 版因此被一个非法请求打崩过整个进程。
async fn close_with<S>(
    ws: &mut WebSocketStream<S>,
    code: u16,
    reason: &str,
) -> Result<(), ConnectionError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let reason = truncate_close_reason(reason);
    let frame = CloseFrame {
        code: CloseCode::from(code),
        reason: Utf8Bytes::from(reason),
    };
    let _ = ws.send(Message::Close(Some(frame))).await;
    let _ = ws.close(None).await;
    Ok(())
}

/// watch 模式：校验 → 共用访问控制 → 起消息转发 → 跑 IMAP 状态机。
#[cfg(feature = "imap-watch")]
async fn handle_watch(
    ws: tokio_tungstenite::WebSocketStream<TcpStream>,
    request: peon_burrow_protocol::WatchRequest,
    policy: Arc<dyn Policy>,
    tls: TlsConfig,
    metrics: SharedMetrics,
    cancel: CancelToken,
    options: RelayOptions,
) -> Result<(), ConnectionError> {
    let credentials = match request.credentials() {
        Ok(credentials) => credentials,
        Err(reason) => return fail_watch(ws, reason).await,
    };

    // watch 与透传**共用同一套访问控制** —— 白名单不能只在透传路径上生效
    let target = peon_burrow_protocol::RelayTarget {
        host: credentials.host.clone(),
        port: credentials.port,
        tls: credentials.tls,
        token: credentials.token.clone(),
    };
    if let Err(reason) = policy.check(&target) {
        warn!(host = %target.host, reason = reason.message(), "watch policy reject");
        return fail_watch(ws, reason).await;
    }

    metrics.watch_opened();
    let (messages, mut outgoing) =
        tokio::sync::mpsc::channel::<peon_burrow_protocol::ClientMessage>(32);
    let (mut sink, mut incoming) = ws.split();

    // 一条任务专门把推送写出去；它同时监听取消与客户端断开
    let mut forwarding_cancel = cancel.clone();
    let forwarding = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = forwarding_cancel.cancelled() => break,
                message = outgoing.recv() => match message {
                    Some(message) => {
                        if sink.send(Message::text(message.to_json())).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
            }
        }
        let _ = sink.close().await;
    });

    let watcher = crate::watch::run_forever(
        credentials,
        tls,
        messages.clone(),
        SharedMetrics::clone(&metrics),
        cancel.clone(),
        &options,
    );

    // 客户端走了 / 被取消 → 结束 watch
    let client_closed = async {
        while let Some(message) = incoming.next().await {
            match message {
                Ok(Message::Close(_)) | Err(_) => break,
                Ok(_) => {}
            }
        }
    };

    tokio::select! {
        _ = watcher => {}
        _ = client_closed => {}
    }

    drop(messages);
    let _ = forwarding.await;
    metrics.watch_closed();
    Ok(())
}

/// 没有 `imap-watch` 时的兜底（分流层已经拒绝了，这里是防御）。
#[cfg(not(feature = "imap-watch"))]
async fn handle_watch(
    mut ws: tokio_tungstenite::WebSocketStream<TcpStream>,
    _request: peon_burrow_protocol::WatchRequest,
    _policy: Arc<dyn Policy>,
    _tls: TlsConfig,
    _metrics: SharedMetrics,
    _cancel: CancelToken,
    _options: RelayOptions,
) -> Result<(), ConnectionError> {
    close_with(
        &mut ws,
        WATCH_FAILED,
        peon_burrow_protocol::RejectReason::WatchNotEnabled.message(),
    )
    .await
}
/// watch 启动失败：**先推一条 `state:"failed"`**，再以 `1011` 关闭。
///
/// 顺序不能反（`ai-docs/design/wire-protocol.md § 4.1`）：扩展要靠这条消息知道
/// 「不用重连了，去改配置」；只丢一个关闭码的话，它只会看到连接断了。
#[cfg(feature = "imap-watch")]
async fn fail_watch(
    mut ws: tokio_tungstenite::WebSocketStream<TcpStream>,
    reason: peon_burrow_protocol::RejectReason,
) -> Result<(), ConnectionError> {
    let message = peon_burrow_protocol::ClientMessage::Failed {
        error: reason.message().to_owned(),
    };
    let _ = ws.send(Message::text(message.to_json())).await;
    close_with(&mut ws, WATCH_FAILED, reason.message()).await
}
