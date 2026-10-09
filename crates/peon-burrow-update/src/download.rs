//! HTTP(S)：清单与资产的下载 —— **自己写的最小实现**（不引 `reqwest`）。
//!
//! 为什么不用 `reqwest`：`reqwest 0.13` 的 `rustls` feature 会拉 **aws-lc-rs**
//! （需要 C 工具链 / nasm，Windows 上直接构建失败），而本 workspace 的政策是
//! **只用 ring**。我们需要的也只是「GET 一个 URL、拿到一坨字节」：
//!
//! ```text
//! parse_url → TCP → (TLS, ring + Mozilla 根) → GET … Connection: close
//!           → 解析状态行/头 → Content-Length 或 chunked → 有上限地读完
//! ```
//!
//! | 支持 | 不支持（明确报错，不静默） |
//! | --- | --- |
//! | `https`（生产路径）、`http`（局域网镜像 / 测试） | SOCKS；`ftp:` 之类 |
//! | `3xx` 重定向（最多 [`MAX_REDIRECTS`] 跳，且**不许 https → http 降级**） | 请求体、cookie、连接复用 |
//! | `Content-Length` / `Transfer-Encoding: chunked` / 读到 EOF | 压缩编码（我们不解压，见 crate 文档） |
//! | 总超时 [`REQUEST_TIMEOUT`]、总字节上限 | 代理（**显式报错**，见 [`get`]） |
//!
//! 上层的测试不依赖真网络：[`Fetcher`] 是一个可注入的抓取函数，
//! `UpdateContext::fetcher = Some(...)` 之后 `check` / `apply` 一行网络都不碰。

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::UpdateContext;
use crate::error::UpdateError;

/// 单次抓取的硬超时（连接 + 握手 + 读完整个响应）。
///
/// 不做重试：设计上「下载中断 → 删掉半成品，下次重来，**不重试到爆**」
/// （`ai-docs/design/update-flow.md § 4`），退避由调用方的检查节流负责。
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// 最多跟随几次重定向（GitHub 的 `releases/latest/download/...` 至少要跳一次）。
pub const MAX_REDIRECTS: usize = 5;

/// 响应头的字节上限。
const MAX_HEAD_BYTES: usize = 64 * 1024;

/// 读取时的单次缓冲区大小。
const READ_CHUNK: usize = 16 * 1024;

/// 分块头（chunk size 行）的字节上限。
const MAX_CHUNK_LINE: usize = 8 * 1024;

/// 线上字节数比「解码后的包体上限」多允许多少（分块框架 / 头部开销 / 多读的那一坨）。
const WIRE_SLACK: u64 = 256 * 1024;

/// `User-Agent`：固定一个可识别的（镜像站 / CDN 常按 UA 做策略）。
const USER_AGENT: &str = concat!("burrow/", env!("CARGO_PKG_VERSION"));

/// 抓取函数的返回值（`Pin<Box<dyn Future>>`，好让 [`Fetcher`] 能存进 `UpdateContext`）。
pub type FetchFuture = Pin<Box<dyn Future<Output = Result<Vec<u8>, UpdateError>> + Send>>;

/// 可注入的抓取函数：`(url, max_bytes) -> Future<Vec<u8>>`。
///
/// 存在的意义：让上层可以**完全脱网**地测 `check` / `apply`（喂一份伪造的清单与资产字节），
/// 也让使用本 crate 的工具能塞进自己的传输栈（自签证书、mTLS、企业代理）。
///
/// ```no_run
/// use peon_burrow_update::{Fetcher, UpdateError};
/// use std::sync::Arc;
///
/// let stub: Fetcher = Arc::new(|url: &str, _max: usize| {
///     let url = url.to_string();
///     Box::pin(async move {
///         Err(UpdateError::HttpProtocol { url, reason: "测试里没有网络".to_string() })
///     })
/// });
/// ```
pub type Fetcher = Arc<dyn Fn(&str, usize) -> FetchFuture + Send + Sync>;

/// 抓一个 URL（走 [`UpdateContext::fetcher`]，没有注入就用内置的 [`get`]）。
///
/// # Errors
///
/// 见 [`get`]；注入 fetcher 时，错误由 fetcher 自己决定（本函数原样透传）。
pub(crate) async fn fetch(
    ctx: &UpdateContext,
    url: &str,
    max_bytes: u64,
) -> Result<Vec<u8>, UpdateError> {
    if let Some(fetcher) = &ctx.fetcher {
        // 注入之后**网络完全由 fetcher 负责**（包括它要不要用 ctx.proxy）
        let max = usize::try_from(max_bytes).unwrap_or(usize::MAX);
        return fetcher(url, max).await;
    }
    get(url, max_bytes, ctx.proxy.as_deref()).await
}

/// 内置抓取实现：`GET url` → 响应体字节（超过 `max_bytes` 立刻断开）。
///
/// ⚠️ **代理这一版不支持**：`proxy` 只要给了值就直接返回
/// [`UpdateError::ProxyUnsupported`] —— 明确报错，绝不静默直连（那会让「配了代理却没用上」
/// 变成一个查不出来的问题）。真要支持，就是「先 CONNECT 建隧道，再在里面发 GET」。
///
/// `http` 也支持：局域网镜像与测试用的本地服务器都是明文（生产用 `https`，
/// 信任链由 HTTPS + sha256 + 签名三层兜住）。
///
/// # Errors
///
/// - [`UpdateError::BadUrl`] / [`UpdateError::ProxyUnsupported`]：URL 或代理不接受；
/// - [`UpdateError::Timeout`]：超过 [`REQUEST_TIMEOUT`]；
/// - [`UpdateError::Download`]：TCP / TLS / 读写失败（TLS 错误的文案在 `source` 里）；
/// - [`UpdateError::Tls`] / [`UpdateError::TlsConfig`]：握手 / rustls 配置；
/// - [`UpdateError::HttpStatus`]：不是 `200`（重定向跟完还是非 200 也算）；
/// - [`UpdateError::HttpProtocol`]：响应不合法（头看不懂、chunked 坏了、重定向超限、被截断）；
/// - [`UpdateError::BodyTooLarge`]：包体超过 `max_bytes`。
pub async fn get(url: &str, max_bytes: u64, proxy: Option<&str>) -> Result<Vec<u8>, UpdateError> {
    if let Some(proxy) = proxy {
        return Err(UpdateError::ProxyUnsupported {
            proxy: proxy.to_string(),
        });
    }

    match tokio::time::timeout(REQUEST_TIMEOUT, get_with_redirects(url, max_bytes)).await {
        Ok(result) => result,
        Err(_) => Err(UpdateError::Timeout {
            url: url.to_string(),
            seconds: REQUEST_TIMEOUT.as_secs(),
        }),
    }
}

/// 跟随重定向地抓（见 [`MAX_REDIRECTS`]）。
async fn get_with_redirects(url: &str, max_bytes: u64) -> Result<Vec<u8>, UpdateError> {
    let mut current = url.to_string();

    for _hop in 0..=MAX_REDIRECTS {
        let endpoint = parse_url(&current)?;
        let response = fetch_once(&endpoint, max_bytes)
            .await
            .map_err(|error| error.into_update(&current))?;

        // 3xx：跟一次（GitHub 的 latest → 具体 tag 就是这一跳）
        if (300..400).contains(&response.status) {
            let location =
                response
                    .header("location")
                    .ok_or_else(|| UpdateError::HttpProtocol {
                        url: current.clone(),
                        reason: format!("HTTP {} 重定向但缺少 Location", response.status),
                    })?;
            let next = resolve_location(&current, location)?;
            let next_endpoint = parse_url(&next)?;
            if endpoint.https && !next_endpoint.https {
                return Err(UpdateError::BadUrl {
                    url: next,
                    reason: "重定向把 https 降级成 http，拒绝".to_string(),
                });
            }
            tracing::debug!(event = "update.redirect", from = %current, to = %next, "跟随重定向");
            current = next;
            continue;
        }

        if response.status != 200 {
            return Err(UpdateError::HttpStatus {
                url: current,
                status: response.status,
            });
        }
        return Ok(response.body);
    }

    Err(UpdateError::HttpProtocol {
        url: url.to_string(),
        reason: format!("重定向超过 {MAX_REDIRECTS} 次"),
    })
}

/// 解析出来的目标地址。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Endpoint {
    /// 是否走 TLS。
    https: bool,
    /// 主机名（IPv6 字面量不带方括号）。
    host: String,
    /// 端口（缺省时按 scheme 填 443 / 80）。
    port: u16,
    /// 请求路径（以 `/` 开头，原样发送）。
    path: String,
}

impl Endpoint {
    /// `Host` 头：非默认端口才带端口。
    fn host_header(&self) -> String {
        if self.port == default_port(self.https) {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// scheme 的默认端口。
fn default_port(https: bool) -> u16 {
    if https { 443 } else { 80 }
}

/// 解析 `http(s)://host[:port]/path`。
///
/// 刻意不引 `url` crate：我们只需要「scheme / host / port / path」四件事，
/// 而且是**发送方**，路径按原样送出去最不容易出意外（清单与资产名都是 ASCII 安全字符）。
fn parse_url(url: &str) -> Result<Endpoint, UpdateError> {
    let bad = |reason: &str| UpdateError::BadUrl {
        url: url.to_string(),
        reason: reason.to_string(),
    };

    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| bad("缺少 scheme://（形如 https://host/path）"))?;
    let https = match scheme {
        "https" => true,
        "http" => false,
        other => return Err(bad(&format!("只支持 http / https，收到 {other}"))),
    };

    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err(bad("没有主机名"));
    }
    let path = path.split('#').next().unwrap_or("/").to_string();

    let (host, port) = split_authority(authority).map_err(|reason| bad(&reason))?;
    Ok(Endpoint {
        https,
        host,
        port: port.unwrap_or_else(|| default_port(https)),
        path,
    })
}

/// 把 `authority` 拆成 `(host, Option<port>)`；支持 `[::1]:8443` 这种 IPv6 字面量。
fn split_authority(authority: &str) -> Result<(String, Option<u16>), String> {
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| format!("IPv6 字面量缺少 ']'：{authority}"))?;
        let port = match tail.strip_prefix(':') {
            Some(port) => Some(parse_port(port)?),
            None if tail.is_empty() => None,
            None => return Err(format!("主机名后面有多余内容：{authority}")),
        };
        (host.to_string(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host.to_string(), Some(parse_port(port)?)),
            None => (authority.to_string(), None),
        }
    };

    if host.is_empty() {
        return Err(format!("主机名为空：{authority}"));
    }
    Ok((host, port))
}

/// 解析端口。
fn parse_port(port: &str) -> Result<u16, String> {
    port.parse::<u16>()
        .map_err(|_| format!("端口不是 0-65535 的数字：{port}"))
}

/// 把 `Location` 解析成绝对 URL（支持绝对地址与以 `/` 开头的相对地址）。
fn resolve_location(base: &str, location: &str) -> Result<String, UpdateError> {
    if location.starts_with("http://") || location.starts_with("https://") {
        return Ok(location.to_string());
    }
    if location.starts_with('/') {
        let (scheme, rest) = base.split_once("://").ok_or_else(|| UpdateError::BadUrl {
            url: base.to_string(),
            reason: "缺少 scheme://".to_string(),
        })?;
        let authority = rest.split('/').next().unwrap_or("");
        return Ok(format!("{scheme}://{authority}{location}"));
    }
    Err(UpdateError::BadUrl {
        url: location.to_string(),
        reason: "不支持这种相对重定向（只支持绝对地址或 / 开头）".to_string(),
    })
}

/// 一个已经读完的响应。
struct Response {
    /// HTTP 状态码。
    status: u16,
    /// 响应头（key 已转小写）。
    headers: Vec<(String, String)>,
    /// 响应体。
    body: Vec<u8>,
}

impl Response {
    /// 按名字取响应头（`name` 必须是小写）。
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// 一次请求（一跳，不跟重定向）。
async fn fetch_once(endpoint: &Endpoint, max_bytes: u64) -> Result<Response, HttpError> {
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

/// 响应体读取器：先吃 header 之后剩下的字节，不够再往 socket 读。
///
/// `wire_limit` 是**线上**字节上限（含分块框架开销），真正的包体上限由调用方在解码后判断。
struct Body<S> {
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
    fn new(stream: S, leftover: Vec<u8>, wire_limit: u64) -> Self {
        Self {
            stream,
            buf: leftover,
            consumed: 0,
            wire_limit,
            eof: false,
        }
    }

    /// 缓冲区里还有多少字节。
    fn available(&self) -> usize {
        self.buf.len()
    }

    /// 取走前 `n` 字节。
    fn take(&mut self, n: usize) -> Vec<u8> {
        let mut front = std::mem::take(&mut self.buf);
        let count = n.min(front.len());
        self.buf = front.split_off(count);
        self.consumed += count as u64;
        front
    }

    /// 再读一坨进来；返回字节数，`0` = 对端关闭。
    async fn fill(&mut self) -> Result<usize, HttpError> {
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
    async fn ensure(&mut self, n: usize) -> Result<bool, HttpError> {
        while self.buf.len() < n {
            if self.fill().await? == 0 {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// 读一行（到 CRLF 为止，不含 CRLF）；返回 `None` = 干净的 EOF。
    async fn line(&mut self) -> Result<Option<String>, HttpError> {
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
async fn read_chunked<S: AsyncRead + Unpin>(
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
async fn read_to_eof<S: AsyncRead + Unpin>(
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

/// `AsyncRead + AsyncWrite` 的组合 trait（好把明文与 TLS 塞进同一个 `Box`）。
trait Conn: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Conn for T {}

/// rustls 客户端配置：**ring provider + Mozilla 根证书**（与 `peon-burrow-core` 的写法一致）。
///
/// 配置进程内只建一次（解析根证书不便宜），失败也缓存（重试同样会失败）。
fn tls_config() -> Result<Arc<rustls::ClientConfig>, String> {
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

/// 在 `haystack` 里找 `needle`（小实现，避免为这一个函数引依赖）。
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// 内置实现内部用的错误（映射成 [`UpdateError`] 时补上 URL）。
enum HttpError {
    /// 底层读写 / 连接失败。
    Io(io::Error),
    /// TLS 握手 / 服务器名失败。
    Tls(String),
    /// rustls 配置建不起来。
    Config(String),
    /// 响应不合法。
    Protocol(String),
    /// 超过字节上限。
    TooLarge {
        /// 触发的上限。
        limit: u64,
    },
}

impl HttpError {
    /// 补上 URL，变成对外的错误。
    fn into_update(self, url: &str) -> UpdateError {
        match self {
            HttpError::Io(source) => UpdateError::Download {
                url: url.to_string(),
                source,
            },
            HttpError::Tls(reason) => UpdateError::Tls {
                url: url.to_string(),
                reason,
            },
            HttpError::Config(reason) => UpdateError::TlsConfig { reason },
            HttpError::Protocol(reason) => UpdateError::HttpProtocol {
                url: url.to_string(),
                reason,
            },
            HttpError::TooLarge { limit } => UpdateError::BodyTooLarge {
                url: url.to_string(),
                limit,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_urls_we_actually_build() {
        assert_eq!(
            parse_url(
                "https://github.com/mail-peon/peon-burrow/releases/latest/download/latest.json"
            )
            .unwrap(),
            Endpoint {
                https: true,
                host: "github.com".to_string(),
                port: 443,
                path: "/mail-peon/peon-burrow/releases/latest/download/latest.json".to_string(),
            }
        );
        assert_eq!(
            parse_url("http://127.0.0.1:8080/a/b").unwrap(),
            Endpoint {
                https: false,
                host: "127.0.0.1".to_string(),
                port: 8080,
                path: "/a/b".to_string(),
            }
        );
        // 没有路径 → "/"；镜像的嵌套前缀原样保留
        assert_eq!(parse_url("https://mirror.invalid").unwrap().path, "/");
        assert_eq!(
            parse_url("https://gh-proxy.com/https://github.com/x")
                .unwrap()
                .path,
            "/https://github.com/x"
        );
        // Host 头只在非默认端口时带端口
        assert_eq!(parse_url("https://a.b/c").unwrap().host_header(), "a.b");
        assert_eq!(parse_url("http://a.b:80/c").unwrap().host_header(), "a.b");
        assert_eq!(
            parse_url("https://a.b:8443/c").unwrap().host_header(),
            "a.b:8443"
        );
    }

    #[test]
    fn rejects_urls_we_cannot_handle() {
        assert!(matches!(
            parse_url("ftp://a.b/c").unwrap_err(),
            UpdateError::BadUrl { .. }
        ));
        assert!(matches!(
            parse_url("github.com/x").unwrap_err(),
            UpdateError::BadUrl { .. }
        ));
        assert!(matches!(
            parse_url("https:///x").unwrap_err(),
            UpdateError::BadUrl { .. }
        ));
        assert!(matches!(
            parse_url("https://a.b:notaport/x").unwrap_err(),
            UpdateError::BadUrl { .. }
        ));
    }

    #[test]
    fn resolves_the_redirects_github_sends() {
        assert_eq!(
            resolve_location(
                "https://github.com/x/latest/download/latest.json",
                "https://objects.githubusercontent.com/abc"
            )
            .unwrap(),
            "https://objects.githubusercontent.com/abc"
        );
        assert_eq!(
            resolve_location(
                "https://gh-proxy.com/https://github.com/x/latest/download/latest.json",
                "/real/path"
            )
            .unwrap(),
            "https://gh-proxy.com/real/path"
        );
        assert!(resolve_location("https://a.b/c", "relative/path").is_err());
    }

    #[tokio::test]
    async fn proxy_is_rejected_loudly_instead_of_being_ignored() {
        let error = get("https://a.b/c", 1024, Some("http://p:8080"))
            .await
            .expect_err("内置实现这一版不支持代理");
        assert!(
            matches!(error, UpdateError::ProxyUnsupported { .. }),
            "{error}"
        );
        assert!(error.to_string().contains("代理"), "{error}");
    }

    #[test]
    fn find_works_on_the_delimiters_we_use() {
        assert_eq!(find(b"abc\r\n\r\ndef", b"\r\n\r\n"), Some(3));
        assert_eq!(find(b"abc", b"\r\n\r\n"), None);
        assert_eq!(find(b"", b"\r\n"), None);
    }
}
