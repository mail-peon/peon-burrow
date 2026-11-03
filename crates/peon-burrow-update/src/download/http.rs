use super::body::{Body, find, read_chunked, read_to_eof};
use super::error::HttpError;
use super::url::Endpoint;
use super::{MAX_HEAD_BYTES, READ_CHUNK, USER_AGENT, WIRE_SLACK};
use rustls::pki_types::ServerName;
use std::sync::{Arc, OnceLock};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

/// 一个已经读完的响应。
pub(super) struct Response {
    /// HTTP 状态码。
    pub(super) status: u16,
    /// 响应头（key 已转小写）。
    pub(super) headers: Vec<(String, String)>,
    /// 响应体。
    pub(super) body: Vec<u8>,
}

impl Response {
    /// 按名字取响应头（`name` 必须是小写）。
    pub(super) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// 一次请求（一跳，不跟重定向）。
pub(super) async fn fetch_once(endpoint: &Endpoint, max_bytes: u64) -> Result<Response, HttpError> {
    let stream = TcpStream::connect((endpoint.host.as_str(), endpoint.port))
        .await
        .map_err(HttpError::Io)?;
    stream.set_nodelay(true).ok();

    let mut stream: Box<dyn Conn> = if endpoint.https {
        let config = tls_config().map_err(HttpError::Config)?;
        let connector = TlsConnector::from(config);
        let server_name = ServerName::try_from(endpoint.host.clone()).map_err(|error| {
            HttpError::Tls(format!("{} 不能作为 TLS 服务器名：{error}", endpoint.host))
        })?;
        let stream = connector
            .connect(server_name, stream)
            .await
            .map_err(|error| HttpError::Tls(error.to_string()))?;
        Box::new(stream)
    } else {
        Box::new(stream)
    };

    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: {USER_AGENT}\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        endpoint.path,
        endpoint.host_header()
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(HttpError::Io)?;
    stream.flush().await.map_err(HttpError::Io)?;

    // ---- 响应头：读到 \r\n\r\n 为止（连同「多读出来的那一坨包体」） ----
    let mut head: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = vec![0u8; READ_CHUNK];
    let head_end = loop {
        if let Some(index) = find(&head, b"\r\n\r\n") {
            break index + 4;
        }
        if head.len() > MAX_HEAD_BYTES {
            return Err(HttpError::Protocol("响应头超过 64 KiB".to_string()));
        }
        let read = stream.read(&mut chunk).await.map_err(HttpError::Io)?;
        if read == 0 {
            return Err(HttpError::Protocol("响应头还没读完连接就断了".to_string()));
        }
        head.extend_from_slice(&chunk[..read]);
    };

    let leftover = head[head_end..].to_vec();
    let head_text = String::from_utf8_lossy(&head[..head_end - 4]).into_owned();
    let mut lines = head_text.split("\r\n");

    let status_line = lines
        .next()
        .ok_or_else(|| HttpError::Protocol("没有状态行".to_string()))?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| HttpError::Protocol(format!("状态行看不懂：{status_line}")))?;

    let headers: Vec<(String, String)> = lines
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            Some((key.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect();

    // ---- 响应体 ----
    let mut body = Body::new(stream, leftover, max_bytes.saturating_add(WIRE_SLACK));
    let chunked = headers
        .iter()
        .find(|(key, _)| key == "transfer-encoding")
        .is_some_and(|(_, value)| value.to_ascii_lowercase().contains("chunked"));

    let bytes = if chunked {
        read_chunked(&mut body, max_bytes).await?
    } else if let Some((_, value)) = headers.iter().find(|(key, _)| key == "content-length") {
        let length: u64 = value
            .trim()
            .parse()
            .map_err(|_| HttpError::Protocol(format!("Content-Length 不是数字：{value}")))?;
        if length > max_bytes {
            return Err(HttpError::TooLarge { limit: max_bytes });
        }
        if !body.ensure(length as usize).await? {
            return Err(HttpError::Protocol(
                "响应体被截断（少于 Content-Length）".to_string(),
            ));
        }
        body.take(length as usize)
    } else {
        // 既没有 Content-Length 也不是 chunked：读到 EOF（`Connection: close` 时的常规做法）
        read_to_eof(&mut body, max_bytes).await?
    };

    Ok(Response {
        status,
        headers,
        body: bytes,
    })
}

/// `AsyncRead + AsyncWrite` 的组合 trait（好把明文与 TLS 塞进同一个 `Box`）。
pub(super) trait Conn: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Conn for T {}

/// rustls 客户端配置：**ring provider + Mozilla 根证书**（与 `peon-burrow-core` 的写法一致）。
///
/// 配置进程内只建一次（解析根证书不便宜），失败也缓存（重试同样会失败）。
pub(super) fn tls_config() -> Result<Arc<rustls::ClientConfig>, String> {
    static CONFIG: OnceLock<Result<Arc<rustls::ClientConfig>, String>> = OnceLock::new();

    match CONFIG.get_or_init(|| {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map(|builder| Arc::new(builder.with_root_certificates(roots).with_no_client_auth()))
            .map_err(|error| error.to_string())
    }) {
        Ok(config) => Ok(Arc::clone(config)),
        Err(reason) => Err(reason.clone()),
    }
}
