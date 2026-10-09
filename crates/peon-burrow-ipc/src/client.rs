//! 控制面客户端：桌面端与 CLI 都用它。

use std::time::Duration;

use peon_burrow_ipc_types::{
    ClientFrame, IpcError, IpcErrorCode, MAX_LINE_BYTES, Request, ServerFrame, decode_line,
    encode_line,
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::endpoint::ControlEndpoint;
use crate::transport;

/// 客户端错误。
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// 连不上（服务没在跑、地址不对）。
    #[error("连不上控制面：{0}")]
    Connect(String),
    /// 超时。
    #[error("控制面响应超时（{0:?}）")]
    Timeout(Duration),
    /// 传输层出错。
    #[error("控制面 IO 错误：{0}")]
    Io(#[from] std::io::Error),
    /// 响应不是合法的 JSON 帧。
    #[error("控制面响应无法解析：{0}")]
    Protocol(String),
    /// 服务明确拒绝了这条请求。
    #[error("{message}")]
    Rejected {
        /// 错误码。
        code: IpcErrorCode,
        /// 说明。
        message: String,
    },
}

impl From<IpcError> for ClientError {
    fn from(error: IpcError) -> Self {
        Self::Rejected {
            code: error.code,
            message: error.message,
        }
    }
}

/// 控制面客户端。
#[derive(Debug, Clone)]
pub struct ControlClient {
    endpoint: ControlEndpoint,
    timeout: Duration,
    id: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl ControlClient {
    /// 默认超时 2 秒（GUI 每 2 秒轮询一次 `ping`，超时不该更久）。
    pub fn new(endpoint: ControlEndpoint) -> Self {
        Self {
            endpoint,
            timeout: Duration::from_secs(2),
            id: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
        }
    }

    /// 改超时。
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// 发一条请求，拿结果。
    pub async fn request(&self, request: Request) -> Result<Value, ClientError> {
        let id = self
            .id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .to_string();
        let frame = ClientFrame::new(id, self.endpoint.token.clone(), request);
        let line = encode_line(&frame).map_err(|error| ClientError::Protocol(error.to_string()))?;

        let attempt = async {
            let mut stream = transport::connect(&self.endpoint)
                .await
                .map_err(|error| ClientError::Connect(error.to_string()))?;
            stream.write_all(line.as_bytes()).await?;
            stream.flush().await?;
            let response = read_line(&mut stream).await?;
            let frame: ServerFrame =
                decode_line(&response).map_err(|error| ClientError::Protocol(error.to_string()))?;
            frame.into_result().map_err(ClientError::from)
        };

        tokio::time::timeout(self.timeout, attempt)
            .await
            .unwrap_or(Err(ClientError::Timeout(self.timeout)))
    }
}

/// 读一行，**有上限**（防止对面一直不发换行把内存填满）。
pub(crate) async fn read_line<S>(stream: &mut S) -> std::io::Result<String>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut buffer = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        let read = stream.read(&mut byte).await?;
        if read == 0 {
            break;
        }
        if byte[0] == b'\n' {
            break;
        }
        buffer.push(byte[0]);
        if buffer.len() > MAX_LINE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "控制面单行超过 8 KiB",
            ));
        }
    }
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_unreachable_control_plane_reports_a_clear_error() {
        // 端口 1 上不会有东西在听。有的系统立刻回 RST（Connect），有的把它当黑洞（Timeout）——
        // 两种都是「连不上控制面」，但**必须**是这两者之一，不能是「解析失败」这种误导性错误。
        let client = ControlClient::new(ControlEndpoint::loopback_tcp(1, "t"))
            .with_timeout(Duration::from_millis(500));
        let error = client
            .request(Request::Ping)
            .await
            .expect_err("should fail");
        assert!(
            matches!(error, ClientError::Connect(_) | ClientError::Timeout(_)),
            "got {error:?}"
        );
    }

    #[tokio::test]
    async fn reading_stops_at_the_line_limit() {
        let payload = vec![b'a'; MAX_LINE_BYTES + 10];
        let mut cursor = std::io::Cursor::new(payload);
        let error = read_line(&mut cursor).await.expect_err("should reject");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn reading_stops_at_the_newline() {
        let mut cursor = std::io::Cursor::new(b"{\"ok\":true}\nrest".to_vec());
        assert_eq!(read_line(&mut cursor).await.expect("read"), "{\"ok\":true}");
    }
}
