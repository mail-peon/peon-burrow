//! peon-burrow 的**线上协议**：只有类型与规则，**没有任何 IO**。
//!
//! 这是稳定层最底下的一层 —— 不依赖 tokio、rustls、interprocess。别的语言要实现客户端或
//! 服务端时，只需要这一份类型与 `ai-docs/design/wire-protocol.md`。
//!
//! ⚠️ **改这里就是改协议**：必须同时更新设计文档与 `mail-peon` 扩展，并升
//! [`WATCH_PROTOCOL_VERSION`]。向后兼容规则见设计文档 § 5。

pub mod close;
pub mod policy;
pub mod target;
pub mod watch;

pub use close::{
    MAX_REASON_BYTES, POLICY_VIOLATION, RESTARTING, WATCH_FAILED, truncate_close_reason,
};
pub use policy::{RejectReason, is_host_allowed, is_loopback};
pub use target::{RelayTarget, TargetResolve, resolve_target};
pub use watch::{ClientMessage, WATCH_PROTOCOL_VERSION, WatchCredentials, WatchRequest};
