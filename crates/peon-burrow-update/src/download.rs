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

mod body;
mod error;
mod http;
mod url;

use self::http::fetch_once;
use self::url::{parse_url, resolve_location};
use crate::UpdateContext;
use crate::error::UpdateError;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

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

#[cfg(test)]
mod tests;
