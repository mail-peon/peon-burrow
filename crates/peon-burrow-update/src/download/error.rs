use crate::error::UpdateError;
use std::io;

/// 内置实现内部用的错误（映射成 [`UpdateError`] 时补上 URL）。
pub(super) enum HttpError {
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
    pub(super) fn into_update(self, url: &str) -> UpdateError {
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
