//! 错误分类与用户文案。
//!
//! ⚠️ 分类靠**类型**，不靠字符串匹配。TS 版用一个正则
//! （`/登录失败|无法打开收件箱|invalid token|host not allowed|.../`）判断「重试有没有用」，
//! 那种写法的失效方式很隐蔽：文案一改，致命错误就被当成可重试的，
//! 于是拿错误的密码反复重连 —— 服务器会把账号锁掉（QQ 会直接拒连一段时间）。

use std::fmt;

use peon_burrow_protocol::RejectReason;

/// 「重试没有意义」的失败原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FatalReason {
    /// 登录被拒（密码 / 授权码错）。
    LoginFailed(String),
    /// `SELECT INBOX` 被拒。
    SelectFailed(String),
    /// token 不匹配。
    InvalidToken,
    /// host 不在白名单里。
    HostNotAllowed,
    /// 请求本身不合法（缺字段等）。
    InvalidRequest(String),
    /// 服务器问候失败（`* BYE` 之类）。
    GreetingFailed(String),
}

impl FatalReason {
    /// 给用户看的文案（会被写进 `state:"failed"` 的 `error` 字段）。
    pub fn message(&self) -> String {
        match self {
            Self::LoginFailed(detail) => format!("登录失败：{detail}"),
            Self::SelectFailed(detail) => format!("无法打开收件箱：{detail}"),
            Self::InvalidToken => "invalid token".to_owned(),
            Self::HostNotAllowed => "host not allowed".to_owned(),
            Self::InvalidRequest(detail) => detail.clone(),
            Self::GreetingFailed(detail) => format!("服务器问候失败：{detail}"),
        }
    }

    /// 从协议层的拒绝原因映射过来。
    ///
    /// 返回 `None` 表示「这条拒绝不是致命原因」—— 目前协议层的拒绝都归为致命：
    /// 它们全是配置 / 凭据问题，重试只会重复同一次失败。
    pub fn from_reject(reason: RejectReason) -> Option<Self> {
        match reason {
            RejectReason::InvalidToken => Some(Self::InvalidToken),
            RejectReason::HostNotAllowed | RejectReason::LoopbackNotAllowed => {
                Some(Self::HostNotAllowed)
            }
            RejectReason::InvalidTarget
            | RejectReason::MissingCredentials
            | RejectReason::MissingTarget
            | RejectReason::WatchNotEnabled => {
                Some(Self::InvalidRequest(reason.message().to_owned()))
            }
            // `tls=0` 连 993 是「这条连接不该建立」，不是「重试无用」：交给透传路径拒绝即可
            RejectReason::PlaintextOnImplicitTlsPort => None,
        }
    }
}

impl fmt::Display for FatalReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

/// 建立 / 维持一条 watch 连接时的失败。
///
/// 调用方只看一件事：**要不要重连**（[`WatchError::is_fatal`]）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WatchError {
    /// 重试无用（凭据 / 配置问题），应当停下来让用户改配置。
    #[error("{0}")]
    Fatal(FatalReason),
    /// 网络层面的失败，退避后重试。
    #[error("{0}")]
    Transport(String),
}

impl WatchError {
    /// 是否「重试无用」。
    pub fn is_fatal(&self) -> bool {
        matches!(self, Self::Fatal(_))
    }

    /// 给用户看的文案。
    pub fn message(&self) -> String {
        match self {
            Self::Fatal(reason) => reason.message(),
            Self::Transport(detail) => detail.clone(),
        }
    }
}

/// 启动 / 运行中继时的错误。
#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    /// 端口已被占用（调用方据此给出「谁占着 / 换端口 / 前台强制」三条出路）。
    #[error("端口 {port} 已被占用")]
    PortInUse {
        /// 被占用的端口。
        port: u16,
    },
    /// 监听地址非法。
    #[error("监听地址无效：{addr}")]
    InvalidAddr {
        /// 那个地址。
        addr: String,
    },
    /// 其它 IO 错误。
    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),
}

impl RelayError {
    /// 把一次 `bind` 失败归类。
    ///
    /// `EADDRINUSE` 要单独成一种，因为它的处理方式完全不同：**不换端口、不杀进程**，
    /// 而是先判断占用者是不是自己的旧实例（见 `ai-docs/design/port-and-discovery.md § 5`）。
    pub fn from_bind_error(port: u16, error: std::io::Error) -> Self {
        if error.kind() == std::io::ErrorKind::AddrInUse {
            Self::PortInUse { port }
        } else {
            Self::Io(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_errors_are_classified() {
        let in_use = std::io::Error::from(std::io::ErrorKind::AddrInUse);
        assert!(matches!(
            RelayError::from_bind_error(41316, in_use),
            RelayError::PortInUse { port: 41316 }
        ));

        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(matches!(
            RelayError::from_bind_error(41316, denied),
            RelayError::Io(_)
        ));
    }

    #[test]
    fn fatal_reasons_carry_their_message() {
        assert_eq!(
            WatchError::Fatal(FatalReason::LoginFailed("服务器拒绝".to_owned())).message(),
            "登录失败：服务器拒绝"
        );
        assert!(WatchError::Fatal(FatalReason::HostNotAllowed).is_fatal());
        assert!(!WatchError::Transport("connection reset".to_owned()).is_fatal());
    }

    #[test]
    fn protocol_rejections_map_to_fatal_reasons() {
        assert_eq!(
            FatalReason::from_reject(RejectReason::InvalidToken),
            Some(FatalReason::InvalidToken)
        );
        assert_eq!(
            FatalReason::from_reject(RejectReason::LoopbackNotAllowed),
            Some(FatalReason::HostNotAllowed)
        );
        assert!(matches!(
            FatalReason::from_reject(RejectReason::MissingCredentials),
            Some(FatalReason::InvalidRequest(_))
        ));
        // 不是「重试无用」：这条连接根本不该建立
        assert_eq!(
            FatalReason::from_reject(RejectReason::PlaintextOnImplicitTlsPort),
            None
        );
    }
}
