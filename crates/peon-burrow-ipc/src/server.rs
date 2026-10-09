//! 控制面服务端：一行请求 → 一行响应，答完即断。
//!
//! 顺序是刻意的（见 `ai-docs/design/control-plane-ipc.md § 4`）：
//! **先看协议版本 → 再验 token → 最后才交给 handler**。
//! 一个不知道版本、或者 token 不对的客户端，不该有机会碰业务逻辑。

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use peon_burrow_ipc_types::{
    ClientFrame, ControlHandler, IPC_PROTOCOL_VERSION, IpcError, IpcErrorCode, ServerFrame,
    decode_line, encode_line,
};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::client::read_line;
use crate::transport::{IoStream, Listener};

/// 鉴权与失败限速。
///
/// 「连续 5 次失败 → 锁 30 秒」防的是**本机**暴力猜 token 的进程 ——
/// 跨用户与网络访客根本连不到这个通道。
#[derive(Debug)]
pub struct AuthPolicy {
    token: String,
    max_failures: u32,
    lockout: Duration,
    failures: AtomicU32,
    locked_until: Mutex<Option<Instant>>,
}

impl AuthPolicy {
    /// 默认：5 次失败锁 30 秒。
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            max_failures: 5,
            lockout: Duration::from_secs(30),
            failures: AtomicU32::new(0),
            locked_until: Mutex::new(None),
        }
    }

    /// 改限速参数（测试用）。
    pub fn with_limits(mut self, max_failures: u32, lockout: Duration) -> Self {
        self.max_failures = max_failures;
        self.lockout = lockout;
        self
    }

    /// 现在是否处于锁定期。
    pub async fn is_locked(&self) -> bool {
        let guard = self.locked_until.lock().await;
        guard.is_some_and(|until| until > Instant::now())
    }

    /// 校验 token。
    async fn check(&self, provided: &str) -> Result<(), IpcError> {
        if self.is_locked().await {
            return Err(IpcError::new(
                IpcErrorCode::Unauthorized,
                "尝试次数过多，请稍后再试",
            ));
        }

        if provided == self.token {
            self.failures.store(0, Ordering::Relaxed);
            let mut guard = self.locked_until.lock().await;
            *guard = None;
            return Ok(());
        }

        let failures = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
        if failures >= self.max_failures {
            let mut guard = self.locked_until.lock().await;
            *guard = Some(Instant::now() + self.lockout);
            self.failures.store(0, Ordering::Relaxed);
            warn!(failures, "control auth lockout engaged");
        }
        Err(IpcError::new(IpcErrorCode::Unauthorized, "token 不匹配"))
    }
}

/// 开始服务，直到 `shutdown` 完成。
pub async fn serve<F>(
    listener: Listener,
    handler: Arc<dyn ControlHandler>,
    auth: Arc<AuthPolicy>,
    shutdown: F,
) -> std::io::Result<()>
where
    F: std::future::Future<Output = ()> + Send,
{
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                let stream = match accepted {
                    Ok(stream) => stream,
                    Err(error) => {
                        // 单条连接失败不该让控制面停摆
                        warn!(%error, "control accept failed");
                        continue;
                    }
                };
                let handler = Arc::clone(&handler);
                let auth = Arc::clone(&auth);
                tokio::spawn(async move {
                    if let Err(error) = handle_connection(stream, handler, auth).await {
                        debug!(%error, "control connection ended");
                    }
                });
            }
        }
    }

    Ok(())
}

/// 处理一条连接：读一行 → 处理 → 回一行 → 关。
async fn handle_connection(
    mut stream: Box<dyn IoStream>,
    handler: Arc<dyn ControlHandler>,
    auth: Arc<AuthPolicy>,
) -> std::io::Result<()> {
    let line = match tokio::time::timeout(Duration::from_secs(5), read_line(&mut stream)).await {
        Ok(result) => result?,
        // 客户端没发完整的一行就走了
        Err(_) => return Ok(()),
    };

    let frame = process(&line, &handler, &auth).await;
    let payload = match encode_line(&frame) {
        Ok(payload) => payload,
        Err(error) => {
            warn!(%error, "failed to encode control response");
            return Ok(());
        }
    };
    stream.write_all(payload.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

/// 纯函数：一行请求 → 一行响应（可单测）。
async fn process(
    line: &str,
    handler: &Arc<dyn ControlHandler>,
    auth: &Arc<AuthPolicy>,
) -> ServerFrame {
    let frame: ClientFrame = match decode_line::<ClientFrame>(line) {
        Ok(frame) => frame,
        Err(error) => {
            // 错误也必须带上 id（客户端靠它对上号），能从原文里捞就捞
            let id = serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|value| {
                    value
                        .get("id")
                        .and_then(|id| id.as_str())
                        .map(str::to_owned)
                })
                .unwrap_or_default();
            return ServerFrame::err(
                id,
                IpcError::new(
                    IpcErrorCode::UnknownCommand,
                    format!("无法解析这条请求（命令不在白名单里？）：{error}"),
                ),
            );
        }
    };

    if frame.v > IPC_PROTOCOL_VERSION {
        return ServerFrame::err(
            frame.id,
            IpcError::new(
                IpcErrorCode::ProtocolTooNew,
                format!(
                    "客户端协议版本 {} 比服务新（服务支持 {}），请更新中继",
                    frame.v, IPC_PROTOCOL_VERSION
                ),
            ),
        );
    }

    if let Err(error) = auth.check(&frame.token).await {
        return ServerFrame::err(frame.id, error);
    }

    match handler.handle(frame.request).await {
        Ok(result) => ServerFrame::ok(frame.id, result),
        Err(error) => ServerFrame::err(frame.id, error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ControlClient;
    use crate::endpoint::ControlEndpoint;
    use crate::transport;
    use peon_burrow_ipc_types::{HandlerFuture, MAX_LINE_BYTES, Request};
    use serde_json::{Value, json};

    struct TestHandler;

    impl ControlHandler for TestHandler {
        fn handle(&self, request: Request) -> HandlerFuture {
            Box::pin(async move {
                match request {
                    Request::Ping => Ok(json!({ "pong": true })),
                    Request::Version => Ok(json!({ "version": "0.1.0" })),
                    other => Err(IpcError::new(
                        IpcErrorCode::Busy,
                        format!("测试里没实现 {}", other.command()),
                    )),
                }
            })
        }
    }

    /// 起一个控制面（loopback TCP，端口由内核分配），返回地址与关闭句柄。
    async fn spawn_control(
        auth: Arc<AuthPolicy>,
    ) -> (ControlEndpoint, tokio::sync::oneshot::Sender<()>) {
        let listener = Listener::bind(&ControlEndpoint::loopback_tcp(0, "t"))
            .await
            .expect("bind");
        let address = listener.address().expect("address");
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = serve(listener, Arc::new(TestHandler), auth, async move {
                let _ = shutdown_rx.await;
            })
            .await;
        });
        let mut endpoint = ControlEndpoint::loopback_tcp(0, "s3cret");
        endpoint.address = address;
        (endpoint, shutdown_tx)
    }

    /// 直接连线发一行原文（用来构造合法客户端发不出的请求）。
    async fn raw_round_trip(endpoint: &ControlEndpoint, line: String) -> Option<Value> {
        let mut stream = transport::connect(endpoint).await.expect("connect");
        stream.write_all(line.as_bytes()).await.expect("write");
        stream.flush().await.expect("flush");
        match read_line(&mut stream).await {
            Ok(response) => serde_json::from_str(&response).ok(),
            Err(_) => None,
        }
    }

    #[tokio::test]
    async fn a_request_gets_a_response() {
        let (endpoint, shutdown) = spawn_control(Arc::new(AuthPolicy::new("s3cret"))).await;
        let client = ControlClient::new(endpoint);

        assert_eq!(
            client.request(Request::Ping).await.expect("ping"),
            json!({ "pong": true })
        );
        assert_eq!(
            client.request(Request::Version).await.expect("version"),
            json!({ "version": "0.1.0" })
        );

        let error = client
            .request(Request::Status)
            .await
            .expect_err("not implemented");
        assert!(matches!(
            error,
            crate::ClientError::Rejected {
                code: IpcErrorCode::Busy,
                ..
            }
        ));

        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn a_wrong_token_is_rejected() {
        let (mut endpoint, shutdown) = spawn_control(Arc::new(AuthPolicy::new("s3cret"))).await;
        endpoint.token = "wrong".to_owned();
        let client = ControlClient::new(endpoint);

        let error = client
            .request(Request::Ping)
            .await
            .expect_err("should reject");
        assert!(
            matches!(
                error,
                crate::ClientError::Rejected {
                    code: IpcErrorCode::Unauthorized,
                    ..
                }
            ),
            "got {error:?}"
        );

        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn too_many_failures_lock_the_channel() {
        let auth = Arc::new(AuthPolicy::new("s3cret"));
        let (mut endpoint, shutdown) = spawn_control(Arc::clone(&auth)).await;

        let wrong = ControlClient::new(ControlEndpoint {
            token: "wrong".to_owned(),
            ..endpoint.clone()
        });
        for _ in 0..5 {
            let _ = wrong.request(Request::Ping).await;
        }
        assert!(auth.is_locked().await, "5 次失败之后应当进入锁定期");

        // 即使 token 正确，锁定期内也拒绝
        endpoint.token = "s3cret".to_owned();
        let right = ControlClient::new(endpoint);
        let error = right.request(Request::Ping).await.expect_err("locked");
        assert!(
            matches!(
                error,
                crate::ClientError::Rejected {
                    code: IpcErrorCode::Unauthorized,
                    ..
                }
            ),
            "got {error:?}"
        );

        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn an_unknown_command_is_answered_with_an_error_frame() {
        let (endpoint, shutdown) = spawn_control(Arc::new(AuthPolicy::new("s3cret"))).await;

        let response = raw_round_trip(
            &endpoint,
            r#"{"v":1,"id":"9","token":"s3cret","cmd":"readCredentials"}"#.to_owned() + "\n",
        )
        .await
        .expect("should answer");

        assert_eq!(response["id"], json!("9"), "错误响应也要带上 id");
        assert_eq!(response["ok"], json!(false));
        assert_eq!(response["error"]["code"], json!("unknownCommand"));

        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn a_newer_protocol_version_is_refused_before_auth() {
        let (endpoint, shutdown) = spawn_control(Arc::new(AuthPolicy::new("s3cret"))).await;

        // 版本检查在 token 之前：连 token 都不给也该得到「版本太新」
        let response = raw_round_trip(
            &endpoint,
            r#"{"v":99,"id":"1","cmd":"ping"}"#.to_owned() + "\n",
        )
        .await
        .expect("should answer");

        assert_eq!(response["error"]["code"], json!("protocolTooNew"));

        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn an_over_long_line_is_dropped_without_killing_the_server() {
        let (endpoint, shutdown) = spawn_control(Arc::new(AuthPolicy::new("s3cret"))).await;

        let mut oversized =
            String::from(r#"{"v":1,"id":"1","token":"s3cret","cmd":"ping","pad":""#);
        oversized.push_str(&"x".repeat(MAX_LINE_BYTES + 64));
        let response = raw_round_trip(&endpoint, oversized).await;
        assert!(response.is_none(), "超长行应当被直接断开，而不是回一条响应");

        // 服务仍然活着
        let client = ControlClient::new(endpoint);
        assert!(client.request(Request::Ping).await.is_ok());

        let _ = shutdown.send(());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn the_named_pipe_transport_works() {
        let name = format!("peon-burrow-control-test-{}", std::process::id());
        let endpoint = ControlEndpoint::local_socket(&name, "s3cret");
        let listener = Listener::bind(&endpoint).await.expect("bind pipe");

        let handler: Arc<dyn ControlHandler> = Arc::new(TestHandler);
        let auth = Arc::new(AuthPolicy::new("s3cret"));
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = serve(listener, handler, auth, async move {
                let _ = shutdown_rx.await;
            })
            .await;
        });

        let client = ControlClient::new(endpoint);
        assert_eq!(
            client.request(Request::Ping).await.expect("ping"),
            json!({ "pong": true })
        );

        let _ = shutdown_tx.send(());
    }
}
