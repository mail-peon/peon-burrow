//! 测试工具：上游服务器（明文 / TLS 回声、脚本化 IMAP）与测试证书。
//!
//! `publish = false` —— 发布它等于承诺一套测试 API。
//!
//! ⚠️ 这一层**不能**依赖 `peon-burrow-core`（core 的 dev-dependencies 里就有本 crate，
//! 反向依赖会成环）：所以「起一个中继 + 等就绪」的 harness 留在 `core/tests/common/` 里，
//! 这里只放与中继无关的**上游**与证书。

use std::net::SocketAddr;

pub mod imap;
pub mod tls;

/// 等到条件成立，或者超时。
///
/// 测试里**不许 `sleep` 之后假设就绪** —— 那是 flaky 的经典成因。
pub async fn wait_until<F>(mut condition: F, timeout: std::time::Duration) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if condition() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    condition()
}

/// 起一个「原样回写」的明文 TCP 服务器。
pub async fn spawn_echo() -> SocketAddr {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind echo");
    let addr = listener.local_addr().expect("echo addr");
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buffer = [0u8; 8192];
                loop {
                    match stream.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            if stream.write_all(&buffer[..read]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });
    addr
}
