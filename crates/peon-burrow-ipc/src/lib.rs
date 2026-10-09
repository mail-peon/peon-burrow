//! 控制面：**本地 socket 优先，loopback TCP 作退路**。
//!
//! - 类型与协议在 [`peon_burrow_ipc_types`]（只依赖 serde）；
//! - 这里只做传输与两端的实现：一行 JSON 请求 ↔ 一行 JSON 响应，答完即断。
//!
//! 为什么不引 `interprocess`：`tokio` 自带命名管道（Windows）与 `UnixListener`/`UnixStream`
//! （Unix），一行 `cfg` 就能覆盖两个平台，少一个依赖。传输藏在
//! [`transport::IoStream`] 后面，将来要换实现也不动上层。

pub mod client;
pub mod endpoint;
pub mod server;
pub mod transport;

pub use client::{ClientError, ControlClient};
pub use endpoint::{ControlEndpoint, TransportKind, read_endpoint, write_endpoint};
pub use server::{AuthPolicy, serve};
pub use transport::{Listener, connect};
