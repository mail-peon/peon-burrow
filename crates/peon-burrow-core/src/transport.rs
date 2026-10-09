//! 建上游连接（TCP / TLS）的**唯一实现**。
//!
//! tunnel 与 watch **共用这里** —— TS 版把「TLS 建连 + SNI + 证书错误提示」在
//! `imap-relay.ts:685-692`（透传）与 `1016-1023`（watch）各写了一遍、证书提示在
//! `843-847` 与 `1247-1247` 各写一遍，两份实现迟早会分叉。

use std::sync::Arc;
use std::time::Duration;

use peon_burrow_protocol::RelayTarget;
use rustls::pki_types::ServerName;

/// 重新导出：接入方注入信任根时不必自己依赖 `rustls`。
pub use rustls::pki_types::CertificateDer;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

/// TLS 客户端设置。
///
/// 这是给接入方留的第二个口子：企业私有 CA、自签证书（测试）都从这里注入，
/// 而不是去关掉校验。
#[derive(Debug, Clone, Default)]
pub struct TlsConfig {
    /// 额外的信任根（DER 编码）。
    pub extra_roots: Vec<CertificateDer<'static>>,
}

impl TlsConfig {
    /// 追加一个信任根。
    pub fn with_root(mut self, der: CertificateDer<'static>) -> Self {
        self.extra_roots.push(der);
        self
    }
}

/// 与邮件服务器之间的一条连接。
#[derive(Debug)]
pub enum Upstream {
    /// 明文 TCP。
    Plain(TcpStream),
    /// TLS（993 是 implicit TLS）。
    Tls(Box<TlsStream<TcpStream>>),
}

impl AsyncRead for Upstream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => std::pin::Pin::new(stream).poll_read(cx, buf),
            Self::Tls(stream) => std::pin::Pin::new(stream.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Upstream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(stream) => std::pin::Pin::new(stream).poll_write(cx, buf),
            Self::Tls(stream) => std::pin::Pin::new(stream.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => std::pin::Pin::new(stream).poll_flush(cx),
            Self::Tls(stream) => std::pin::Pin::new(stream.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => std::pin::Pin::new(stream).poll_shutdown(cx),
            Self::Tls(stream) => std::pin::Pin::new(stream.as_mut()).poll_shutdown(cx),
        }
    }
}

/// 建连失败。
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// TCP 连接失败。
    #[error("连接 {host}:{port} 失败：{message}")]
    Connect {
        /// 目标 host。
        host: String,
        /// 目标端口。
        port: u16,
        /// 底层错误文案。
        message: String,
    },
    /// 建连或握手超时。
    #[error("连接 {host}:{port} 超时（{timeout:?}）")]
    Timeout {
        /// 目标 host。
        host: String,
        /// 目标端口。
        port: u16,
        /// 用的超时。
        timeout: Duration,
    },
    /// TLS 握手失败。
    #[error("{0}")]
    Tls(String),
    /// TLS 配置有问题（证书、SNI 之类）。
    #[error("TLS 配置错误：{0}")]
    TlsConfig(String),
}

impl TransportError {
    /// 是否值得重试。
    ///
    /// ⚠️ 判据保守：证书问题与配置错误**不重试**（重试只是重复同一次失败），
    /// 网络类错误才重试。
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Tls(_) | Self::TlsConfig(_) => false,
            Self::Timeout { .. } | Self::Connect { .. } => true,
        }
    }

    /// 给用户看的文案。
    pub fn user_message(&self) -> String {
        match self {
            Self::Tls(detail) if is_certificate_problem(detail) => format!(
                "邮件服务器的证书没通过校验：{detail}。\
                 如果它用的是自签证书或私有 CA，请把根证书加进配置（测试环境才考虑跳过校验）"
            ),
            other => other.to_string(),
        }
    }
}

/// 证书类错误的启发式识别（rustls 的错误文案里含这些关键词）。
fn is_certificate_problem(detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    detail.contains("certificate")
        || detail.contains("unknownissuer")
        || detail.contains("self-signed")
        || detail.contains("selfsigned")
}

/// 建一条上游连接。
///
/// - `tls = false`：裸 TCP；
/// - `tls = true`：立刻 TLS 握手（993 是 implicit TLS，握手完成前不接受任何 IMAP 命令），
///   SNI 用目标主机名 —— **IP 字面量不发 SNI**（那会让部分服务器直接拒握手）。
pub async fn connect(
    target: &RelayTarget,
    tls: &TlsConfig,
    timeout: Duration,
) -> Result<Upstream, TransportError> {
    let host = target.host.clone();
    let port = target.port;
    let attempt = async {
        let stream = TcpStream::connect((host.as_str(), port))
            .await
            .map_err(|error| TransportError::Connect {
                host: host.clone(),
                port,
                message: error.to_string(),
            })?;
        stream.set_nodelay(true).ok();

        if !target.tls {
            return Ok(Upstream::Plain(stream));
        }

        let connector = TlsConnector::from(Arc::new(client_config(tls)?));
        let server_name = server_name_for(&host)?;
        let stream = connector
            .connect(server_name, stream)
            .await
            .map_err(|error| TransportError::Tls(error.to_string()))?;
        Ok(Upstream::Tls(Box::new(stream)))
    };

    tokio::time::timeout(timeout, attempt)
        .await
        .unwrap_or_else(|_| {
            Err(TransportError::Timeout {
                host: target.host.clone(),
                port: target.port,
                timeout,
            })
        })
}

/// SNI：主机名用 DNS 名，IP 字面量用 IP 名（rustls 因此不会发 SNI 扩展）。
fn server_name_for(host: &str) -> Result<ServerName<'static>, TransportError> {
    ServerName::try_from(host.to_owned()).map_err(|error| {
        TransportError::TlsConfig(format!("{host} 不能作为 TLS 服务器名：{error}"))
    })
}

/// 构造 rustls 客户端配置：Mozilla 根证书 + 注入的额外根。
fn client_config(tls: &TlsConfig) -> Result<rustls::ClientConfig, TransportError> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    for der in &tls.extra_roots {
        roots
            .add(der.clone())
            .map_err(|error| TransportError::TlsConfig(error.to_string()))?;
    }

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| TransportError::TlsConfig(error.to_string()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::{IpAddr, Ipv4Addr};

    #[test]
    fn a_dns_name_becomes_a_dns_server_name() {
        let name = server_name_for("imap.qq.com").expect("dns name");
        assert!(matches!(name, ServerName::DnsName(_)));
    }

    #[test]
    fn an_ip_literal_becomes_an_ip_server_name() {
        // IP 字面量必须走 IpAddress，rustls 据此不发 SNI
        let name = server_name_for("127.0.0.1").expect("ip");
        match name {
            ServerName::IpAddress(address) => {
                assert_eq!(address, IpAddr::V4(Ipv4Addr::from([127, 0, 0, 1])));
            }
            other => panic!("expected an ip server name, got {other:?}"),
        }
    }

    #[test]
    fn certificate_failures_are_not_retried() {
        let error = TransportError::Tls("invalid peer certificate: UnknownIssuer".to_owned());
        assert!(!error.is_retryable());
        assert!(
            error.user_message().contains("自签证书") || error.user_message().contains("根证书")
        );
    }

    #[test]
    fn network_failures_are_retried() {
        let error = TransportError::Connect {
            host: "imap.qq.com".to_owned(),
            port: 993,
            message: "connection refused".to_owned(),
        };
        assert!(error.is_retryable());
        assert!(error.user_message().contains("imap.qq.com:993"));
    }

    #[test]
    fn extra_roots_are_accepted() {
        let der = CertificateDer::from(vec![0u8; 8]);
        let config = TlsConfig::default().with_root(der);
        assert_eq!(config.extra_roots.len(), 1);
        // 无效 DER 必须在建配置时就报出来，而不是等到握手
        assert!(matches!(
            client_config(&config),
            Err(TransportError::TlsConfig(_))
        ));
    }
}
