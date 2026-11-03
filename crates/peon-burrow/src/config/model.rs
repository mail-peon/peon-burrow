use crate::exit::AppError;
use peon_burrow_core::RelayOptions;
use peon_burrow_ipc_types::{Autostart, ServiceLevel};
use serde::Deserialize;
use std::path::PathBuf;

/// 日志级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    /// 只记错误。
    Error,
    /// 记警告。
    Warn,
    /// 常规信息（默认）。
    Info,
    /// 调试信息。
    Debug,
    /// 逐字节（**含明文凭据**）。
    Trace,
}

impl LogLevel {
    /// 允许的取值（错误信息里要列出来）。
    pub const ALL: [&'static str; 5] = ["error", "warn", "info", "debug", "trace"];

    /// 解析（大小写不敏感）。
    pub fn parse(value: &str) -> Result<Self, AppError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "error" => Ok(Self::Error),
            "warn" => Ok(Self::Warn),
            "info" => Ok(Self::Info),
            "debug" => Ok(Self::Debug),
            "trace" => Ok(Self::Trace),
            other => Err(AppError::Config(format!(
                "log_level 取值不合法：{other}（可选：{}）",
                Self::ALL.join(" | ")
            ))),
        }
    }

    /// 规范写法。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

impl std::str::FromStr for LogLevel {
    type Err = AppError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

/// 更新通道。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdateChannel {
    /// 稳定通道。
    Stable,
    /// 抢先通道。
    Beta,
}

impl UpdateChannel {
    /// 规范写法。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Beta => "beta",
        }
    }

    /// 解析。
    pub fn parse(value: &str) -> Result<Self, AppError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "stable" => Ok(Self::Stable),
            "beta" => Ok(Self::Beta),
            other => Err(AppError::Config(format!(
                "channel 取值不合法：{other}（可选：stable | beta）"
            ))),
        }
    }
}

/// `[service]` 段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSection {
    /// 用户级（默认，零提权）还是系统级。
    pub level: ServiceLevel,
    /// 自启触发方式。
    pub autostart: Autostart,
    /// 服务名。
    pub name: String,
}

impl Default for ServiceSection {
    fn default() -> Self {
        Self {
            level: ServiceLevel::User,
            autostart: Autostart::Logon,
            name: "peon-burrow".to_owned(),
        }
    }
}

/// `[update]` 段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateSection {
    /// 是否启用自更新。
    pub enabled: bool,
    /// 通道。
    pub channel: UpdateChannel,
    /// 检查间隔（小时）。
    pub check_interval_hours: u64,
    /// 是否校验（校验和**从不跳过**，这个开关留给将来的签名策略）。
    pub verify_checksum: bool,
    /// 镜像站前缀。
    pub base_url: Option<String>,
}

impl Default for UpdateSection {
    fn default() -> Self {
        Self {
            enabled: true,
            channel: UpdateChannel::Stable,
            check_interval_hours: 24,
            verify_checksum: true,
            base_url: None,
        }
    }
}

/// `[control]` 段。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ControlSection {
    /// 是否开控制面。
    pub enabled: bool,
    /// 额外允许连接的本机用户（系统服务模式下有意义）。
    pub allowed_users: Vec<String>,
}

/// 配置文件的原始结构：**每个字段都是 `Option`**。
#[derive(Debug, Default, Deserialize)]
pub(crate) struct FileConfig {
    pub(super) host: Option<String>,
    pub(super) port: Option<u16>,
    pub(super) token: Option<String>,
    pub(super) allowed_hosts: Option<Vec<String>>,
    pub(super) tls_reject_unauthorized: Option<bool>,
    pub(super) idle_timeout_secs: Option<u64>,
    pub(super) max_connections: Option<usize>,
    pub(super) backpressure_high_water_mib: Option<u64>,
    pub(super) watch_reidle_secs: Option<u64>,
    pub(super) watch_retry_delays_secs: Option<Vec<u64>>,
    pub(super) log_level: Option<String>,
    pub(super) trace: Option<bool>,
    pub(super) log_max_mib: Option<u64>,
    pub(super) log_keep_files: Option<u32>,
    pub(super) service: Option<FileService>,
    pub(super) update: Option<FileUpdate>,
    pub(super) control: Option<FileControl>,
}

/// `[service]` 段（原始）。
#[derive(Debug, Default, Deserialize)]
pub(super) struct FileService {
    pub(super) mode: Option<ServiceLevel>,
    pub(super) autostart: Option<Autostart>,
    pub(super) name: Option<String>,
}

/// `[update]` 段（原始）。
#[derive(Debug, Default, Deserialize)]
pub(super) struct FileUpdate {
    pub(super) enabled: Option<bool>,
    pub(super) channel: Option<UpdateChannel>,
    pub(super) check_interval_hours: Option<u64>,
    pub(super) verify_checksum: Option<bool>,
    pub(super) base_url: Option<String>,
}

/// `[control]` 段（原始）。
#[derive(Debug, Default, Deserialize)]
pub(super) struct FileControl {
    pub(super) enabled: Option<bool>,
    pub(super) allowed_users: Option<Vec<String>>,
}

/// CLI 覆盖项（只放能覆盖配置的那些）。
#[derive(Debug, Default, Clone)]
pub struct CliOverrides {
    /// `--config`
    pub config: Option<PathBuf>,
    /// `--host`
    pub host: Option<String>,
    /// `--port`
    pub port: Option<u16>,
    /// `--token`
    pub token: Option<String>,
    /// `--allowed-hosts`
    pub allowed_hosts: Option<Vec<String>>,
    /// `--log-level`
    pub log_level: Option<String>,
    /// `--trace`
    pub trace: Option<bool>,
    /// `--tls-reject-unauthorized`
    pub tls_reject_unauthorized: Option<bool>,
}

/// 合并后的配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// 引擎参数（默认值来自 core）。
    pub relay: RelayOptions,
    /// 访问 token（`None` = 不要求）。
    pub token: Option<String>,
    /// 允许的邮件服务器（空 = 不限制，本机地址仍被拒）。
    pub allowed_hosts: Vec<String>,
    /// 是否校验证书链。
    pub tls_reject_unauthorized: bool,
    /// 日志级别。
    pub log_level: LogLevel,
    /// 日志文件上限（MiB，0 = 只写 stdout）。
    pub log_max_mib: u64,
    /// 日志保留个数。
    pub log_keep_files: u32,
    /// `[service]`
    pub service: ServiceSection,
    /// `[update]`
    pub update: UpdateSection,
    /// `[control]`
    pub control: ControlSection,
    /// 合并过程中攒下的警告（未知字段、被弃用的字段…），启动时打出来。
    pub warnings: Vec<String>,
}
