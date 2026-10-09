//! peon-burrow 的**引擎**：WebSocket 服务、字节隧道、访问策略、可查询状态。
//!
//! 这一层的定位是**可嵌入**：它不认识配置文件、不认识服务管理器、也不认识控制面。
//! 输入是 `RelayOptions` 值，输出是 `RelayServer` 句柄 —— 见 `ai-docs/modules.md § 2`。
//!
//! 最小用法：
//!
//! ```no_run
//! use peon_burrow_core::{RelayOptions, RelayServer};
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let relay = RelayServer::start(RelayOptions::on("127.0.0.1", 0)).await?;
//! println!("扩展里填 {}", relay.url());
//! relay.stop().await?;
//! # Ok(())
//! # }
//! ```
//!
//! 要换访问策略或注入私有 CA，用 `RelayServer::start_with`（接入点见 `ai-docs/modules.md § 10`）。
//!
//! feature：
//! - `imap-watch`（**默认关**）：启用 IMAP `IDLE` 监听。库用户默认拿到的是纯字节隧道；
//!   关掉时收到 `__watch` 请求会以 `1008` **明确拒绝**（不是静默当透传）。

pub mod error;
pub mod options;
pub mod policy;
pub mod server;
pub mod state;
pub mod transport;

pub(crate) mod dispatch;
pub(crate) mod tunnel;

#[cfg(feature = "imap-watch")]
pub(crate) mod line_buffer;
#[cfg(feature = "imap-watch")]
pub(crate) mod watch;

pub use error::{FatalReason, RelayError, WatchError};
pub use options::RelayOptions;
pub use policy::{Policy, PolicyRules};
pub use server::RelayServer;
pub use state::RelayState;
pub use transport::{CertificateDer, TlsConfig, TransportError, Upstream};
