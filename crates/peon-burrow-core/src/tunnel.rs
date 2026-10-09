//! 透传隧道：WebSocket ↔ 上游 TCP/TLS 的字节搬运。
//!
//! **背压靠 `await` 传播**：读写都在同一个任务里 `.await`，上游读多少、WebSocket 就得
//! 发完才继续读下一块 —— 内存占用因此被限制在一个读缓冲以内。TS 版用「在途字节计数 +
//! pause/resume + 16 MiB 阈值」手工实现同一件事，那种做法在 Rust 里是多余的。

use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

use crate::state::{CancelToken, SharedMetrics};
use crate::transport::Upstream;

/// 一次透传的结束方式。
#[derive(Debug, thiserror::Error)]
pub(crate) enum TunnelError {
    /// WebSocket 侧出错。
    #[error("WebSocket 错误：{0}")]
    WebSocket(String),
    /// 上游 IO 出错。
    #[error("上游 IO 错误：{0}")]
    Upstream(#[from] std::io::Error),
}

/// 上游读缓冲大小。
const READ_BUFFER: usize = 16 * 1024;

/// 搬运字节，直到任一方向结束 / 空闲超时 / 被取消。
///
/// `first_frame` 是被分流逻辑扣下的那一帧（**必须补投**，否则客户端的第一个 IMAP
/// 命令就丢了 —— 见 `ai-docs/04-parity-node-to-rust.md` C9）。
pub(crate) async fn run<S>(
    ws: WebSocketStream<S>,
    upstream: Upstream,
    idle_timeout: Duration,
    metrics: SharedMetrics,
    cancel: CancelToken,
    first_frame: Option<Message>,
) -> Result<(), TunnelError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sink, mut stream) = ws.split();
    let (mut reader, mut writer) = tokio::io::split(upstream);

    if let Some(frame) = first_frame {
        forward_to_upstream(&frame, &mut writer, &metrics).await?;
    }

    let outbound_metrics = SharedMetrics::clone(&metrics);
    let to_upstream = async move {
        while let Some(message) = stream.next().await {
            let message = message.map_err(|error| TunnelError::WebSocket(error.to_string()))?;
            if matches!(message, Message::Close(_)) {
                break;
            }
            forward_to_upstream(&message, &mut writer, &outbound_metrics).await?;
        }
        // 客户端关了：把 FIN 传给上游，让邮件服务器也能干净收场
        let _ = writer.shutdown().await;
        Ok::<(), TunnelError>(())
    };

    let inbound_metrics = SharedMetrics::clone(&metrics);
    let to_client = async move {
        let mut buffer = vec![0u8; READ_BUFFER];
        loop {
            let read = match tokio::time::timeout(idle_timeout, reader.read(&mut buffer)).await {
                Ok(result) => result?,
                // 空闲超时：IMAP 的 IDLE 会长期静默，但那是 watch 的事；
                // 透传连接的静默到点就该断（默认 15 分钟，与 TS 版一致）
                Err(_) => break,
            };
            if read == 0 {
                break;
            }
            inbound_metrics.add_to_client(read as u64);
            sink.send(Message::Binary(Bytes::copy_from_slice(&buffer[..read])))
                .await
                .map_err(|error| TunnelError::WebSocket(error.to_string()))?;
        }
        let _ = sink.send(Message::Close(None)).await;
        let _ = sink.close().await;
        Ok::<(), TunnelError>(())
    };

    let mut cancel = cancel;
    tokio::select! {
        result = to_upstream => result,
        result = to_client => result,
        _ = cancel.cancelled() => Ok(()),
    }
}

/// 把一帧 WebSocket 消息写进上游（文本帧按 UTF-8 编码后原样写）。
async fn forward_to_upstream<W>(
    message: &Message,
    writer: &mut W,
    metrics: &SharedMetrics,
) -> Result<(), TunnelError>
where
    W: AsyncWrite + Unpin,
{
    match message {
        Message::Text(text) => {
            let bytes = text.as_str().as_bytes();
            writer.write_all(bytes).await?;
            metrics.add_to_server(bytes.len() as u64);
        }
        Message::Binary(data) => {
            writer.write_all(data).await?;
            metrics.add_to_server(data.len() as u64);
        }
        // Ping / Pong 由 tungstenite 自己处理；Close 在上层处理
        _ => {}
    }
    Ok(())
}
