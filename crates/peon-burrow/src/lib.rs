//! `peon-burrow` 的**产品层**：配置、诊断、控制面接线、命令行。
//!
//! 与 `peon-burrow-core` 的分工：core 是「能被别人嵌入的引擎」，这里是「一个具体的程序」。
//! 想让自己的项目用引擎，直接依赖 `peon-burrow-core`；想照抄一个 CLI/服务的样子，看这里。
//!
//! ⚠️ 这一层的 **lib API 不承诺稳定**（见 `STABILITY.md`）：只有 `burrow` 这个命令的行为
//! 是给用户看的契约。

pub mod cli;
pub mod config;
pub mod control;
pub mod doctor;
pub mod exit;
pub mod paths;
pub mod run;
pub mod update;

pub use cli::{Cli, Command, ServiceCommand};
pub use config::{Config, LogLevel};
pub use exit::{AppError, ExitCode};
pub use paths::Paths;
pub use run::run;

/// crate 版本（`--version` 与状态里都用它）。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
