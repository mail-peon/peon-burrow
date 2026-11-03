use super::error::HttpError;
use super::{MAX_CHUNK_LINE, READ_CHUNK};
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;

/// 响应体读取器：先吃 header 之后剩下的字节，不够再往 socket 读。
///
/// `wire_limit` 是**线上**字节上限（含分块框架开销），真正的包体上限由调用方在解码后判断。
pub(super) struct Body<S> {
    /// 底层连接。
    stream: S,
    /// 还没被消费的字节。
    buf: Vec<u8>,
    /// 已经被消费掉的字节数（用于算线上总量）。
    consumed: u64,
    /// 线上字节上限。
    wire_limit: u64,
    /// 对端已经关闭。
    eof: bool,
}

impl<S: AsyncRead + Unpin> Body<S> {
    /// 用「已经读到的剩余字节」起手。
    pub(super) fn new(stream: S, leftover: Vec<u8>, wire_limit: u64) -> Self {
        Self {
            stream,
            buf: leftover,
            consumed: 0,
            wire_limit,
            eof: false,
        }
    }

    /// 缓冲区里还有多少字节。
    pub(super) fn available(&self) -> usize {
        self.buf.len()
    }

    /// 取走前 `n` 字节。
    pub(super) fn take(&mut self, n: usize) -> Vec<u8> {
        let mut front = std::mem::take(&mut self.buf);
        let count = n.min(front.len());
        self.buf = front.split_off(count);
        self.consumed += count as u64;
        front
    }

    /// 再读一坨进来；返回字节数，`0` = 对端关闭。
    pub(super) async fn fill(&mut self) -> Result<usize, HttpError> {
        if self.eof {
            return Ok(0);
        }
        let mut chunk = vec![0u8; READ_CHUNK];
        let read = self.stream.read(&mut chunk).await.map_err(HttpError::Io)?;
        if read == 0 {
            self.eof = true;
            return Ok(0);
        }
        if self.consumed + self.buf.len() as u64 + read as u64 > self.wire_limit {
            return Err(HttpError::TooLarge {
                limit: self.wire_limit,
            });
        }
        self.buf.extend_from_slice(&chunk[..read]);
        Ok(read)
    }

    /// 保证缓冲区里至少有 `n` 字节；返回 `false` = 对端提前关了。
    pub(super) async fn ensure(&mut self, n: usize) -> Result<bool, HttpError> {
        while self.buf.len() < n {
            if self.fill().await? == 0 {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// 读一行（到 CRLF 为止，不含 CRLF）；返回 `None` = 干净的 EOF。
    pub(super) async fn line(&mut self) -> Result<Option<String>, HttpError> {
        loop {
            if let Some(index) = find(&self.buf, b"\r\n") {
                let line = String::from_utf8_lossy(&self.buf[..index]).into_owned();
                drop(self.take(index + 2));
                return Ok(Some(line));
            }
            if self.buf.len() > MAX_CHUNK_LINE {
                return Err(HttpError::Protocol("分块头过长".to_string()));
            }
            if self.fill().await? == 0 {
                return if self.buf.is_empty() {
                    Ok(None)
                } else {
                    Err(HttpError::Protocol("分块响应提前结束".to_string()))
                };
            }
        }
    }
}

/// 解 `Transfer-Encoding: chunked`。
pub(super) async fn read_chunked<S: AsyncRead + Unpin>(
    body: &mut Body<S>,
    max_bytes: u64,
) -> Result<Vec<u8>, HttpError> {
    let mut out: Vec<u8> = Vec::new();
    loop {
        let line = body
            .line()
            .await?
            .ok_or_else(|| HttpError::Protocol("分块响应缺少块长度".to_string()))?;
        let size_text = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| HttpError::Protocol(format!("块长度不是十六进制：{size_text}")))?;

        if size == 0 {
            // 0 块之后是 trailer，直到空行
            while let Some(trailer) = body.line().await? {
                if trailer.is_empty() {
                    break;
                }
            }
            return Ok(out);
        }

        if !body.ensure(size + 2).await? {
            return Err(HttpError::Protocol("分块响应被截断".to_string()));
        }
        out.extend_from_slice(&body.take(size));
        if out.len() as u64 > max_bytes {
            return Err(HttpError::TooLarge { limit: max_bytes });
        }
        let crlf = body.take(2);
        if crlf != b"\r\n" {
            return Err(HttpError::Protocol("分块之间不是 CRLF".to_string()));
        }
    }
}

/// 读到 EOF（没有 `Content-Length` 也没有 chunked 时的退路）。
pub(super) async fn read_to_eof<S: AsyncRead + Unpin>(
    body: &mut Body<S>,
    max_bytes: u64,
) -> Result<Vec<u8>, HttpError> {
    let mut out: Vec<u8> = Vec::new();
    loop {
        if body.available() == 0 && body.fill().await? == 0 {
            return Ok(out);
        }
        let count = body.available();
        out.extend_from_slice(&body.take(count));
        if out.len() as u64 > max_bytes {
            return Err(HttpError::TooLarge { limit: max_bytes });
        }
    }
}

/// 在 `haystack` 里找 `needle`（小实现，避免为这一个函数引依赖）。
pub(super) fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}
