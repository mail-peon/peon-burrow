//! 配置：**文件 / 环境变量 / CLI 三层合并** + 组合规则校验。
//!
//! ```text
//! CLI 参数  >  环境变量  >  配置文件  >  内置默认值（peon-burrow-core）
//! ```
//!
//! ⚠️ 默认值不在这里（布局铁律 L3）：结构里每个字段都是 `Option`，缺省就落到
//! `RelayOptions::default()`。**改默认值只改 core 一处。**
//!
//! 环境变量名保持 TS 版的 `PORT` / `RELAY_TOKEN` 等（旧脚本与旧文档继续有效）。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use peon_burrow_core::RelayOptions;
use peon_burrow_ipc_types::{Autostart, ServiceLevel};
use serde::Deserialize;

use crate::exit::AppError;
use crate::paths::Paths;

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
    host: Option<String>,
    port: Option<u16>,
    token: Option<String>,
    allowed_hosts: Option<Vec<String>>,
    tls_reject_unauthorized: Option<bool>,
    idle_timeout_secs: Option<u64>,
    max_connections: Option<usize>,
    backpressure_high_water_mib: Option<u64>,
    watch_reidle_secs: Option<u64>,
    watch_retry_delays_secs: Option<Vec<u64>>,
    log_level: Option<String>,
    trace: Option<bool>,
    log_max_mib: Option<u64>,
    log_keep_files: Option<u32>,
    service: Option<FileService>,
    update: Option<FileUpdate>,
    control: Option<FileControl>,
}

/// `[service]` 段（原始）。
#[derive(Debug, Default, Deserialize)]
struct FileService {
    mode: Option<ServiceLevel>,
    autostart: Option<Autostart>,
    name: Option<String>,
}

/// `[update]` 段（原始）。
#[derive(Debug, Default, Deserialize)]
struct FileUpdate {
    enabled: Option<bool>,
    channel: Option<UpdateChannel>,
    check_interval_hours: Option<u64>,
    verify_checksum: Option<bool>,
    base_url: Option<String>,
}

/// `[control]` 段（原始）。
#[derive(Debug, Default, Deserialize)]
struct FileControl {
    enabled: Option<bool>,
    allowed_users: Option<Vec<String>>,
}

/// 环境变量来源（可注入，测试不碰真实环境）。
#[derive(Debug, Default, Clone)]
pub struct EnvSource {
    values: BTreeMap<String, String>,
}

impl EnvSource {
    /// 读真实环境。
    pub fn from_env() -> Self {
        const KEYS: [&str; 8] = [
            "PORT",
            "HOST",
            "RELAY_TOKEN",
            "ALLOWED_HOSTS",
            "TLS_REJECT_UNAUTHORIZED",
            "RELAY_TRACE",
            "RELAY_CONFIG",
            "RELAY_LOG_LEVEL",
        ];
        let mut values = BTreeMap::new();
        for key in KEYS {
            if let Ok(value) = std::env::var(key) {
                values.insert(key.to_owned(), value);
            }
        }
        Self { values }
    }

    /// 用给定键值构造（测试用）。
    pub fn from_pairs(
        pairs: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
    ) -> Self {
        Self {
            values: pairs
                .into_iter()
                .map(|(key, value)| (key.into(), value.into()))
                .collect(),
        }
    }

    /// 取一个变量。
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }
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

impl Config {
    /// 三层合并。
    pub fn load(paths: &Paths, env: &EnvSource, cli: &CliOverrides) -> Result<Self, AppError> {
        let mut warnings = Vec::new();

        let path = cli
            .config
            .clone()
            .or_else(|| env.get("RELAY_CONFIG").map(PathBuf::from))
            .unwrap_or_else(|| paths.config_file().to_path_buf());

        let file = if path.exists() {
            parse_file(&path, &mut warnings)?
        } else {
            FileConfig::default()
        };

        let config = Self::merge(file, env, cli, warnings)?;
        config.validate()?;
        Ok(config)
    }

    /// 只做合并，不校验（校验单独一步，测试更好写）。
    pub(crate) fn merge(
        file: FileConfig,
        env: &EnvSource,
        cli: &CliOverrides,
        warnings: Vec<String>,
    ) -> Result<Self, AppError> {
        let mut warnings = warnings;
        let mut defaults = RelayOptions::default();

        // ---- 文件 ----------------------------------------------------------
        if let Some(host) = file.host {
            defaults.host = host;
        }
        if let Some(port) = file.port {
            defaults.port = port;
        }
        if let Some(secs) = file.idle_timeout_secs {
            defaults.idle_timeout = Duration::from_secs(secs);
        }
        if let Some(max) = file.max_connections {
            defaults.max_connections = max;
        }
        if let Some(secs) = file.watch_reidle_secs {
            defaults.watch_reidle = Duration::from_secs(secs);
        }
        if let Some(delays) = file.watch_retry_delays_secs {
            defaults.watch_retry_delays = delays.into_iter().map(Duration::from_secs).collect();
        }
        if let Some(trace) = file.trace {
            defaults.trace = trace;
        }
        if file.backpressure_high_water_mib.is_some() {
            warnings.push(
                "backpressure_high_water_mib 在当前实现里已无作用（背压由 await 传播，不再需要水位阈值），可以删掉这一行"
                    .to_owned(),
            );
        }

        let mut token = file.token.filter(|value| !value.is_empty());
        let mut allowed_hosts = file.allowed_hosts.unwrap_or_default();
        let mut tls_reject_unauthorized = file.tls_reject_unauthorized.unwrap_or(true);
        let mut log_level = match file.log_level {
            Some(value) => LogLevel::parse(&value)?,
            None => LogLevel::Info,
        };
        let mut service = ServiceSection::default();
        if let Some(section) = file.service {
            if let Some(mode) = section.mode {
                service.level = mode;
            }
            if let Some(autostart) = section.autostart {
                service.autostart = autostart;
            }
            if let Some(name) = section.name {
                service.name = name;
            }
        }
        let mut update = UpdateSection::default();
        if let Some(section) = file.update {
            if let Some(enabled) = section.enabled {
                update.enabled = enabled;
            }
            if let Some(channel) = section.channel {
                update.channel = channel;
            }
            if let Some(hours) = section.check_interval_hours {
                update.check_interval_hours = hours;
            }
            if let Some(verify) = section.verify_checksum {
                update.verify_checksum = verify;
            }
            update.base_url = section.base_url;
        }
        let mut control = ControlSection::default();
        if let Some(section) = file.control {
            control.enabled = section.enabled.unwrap_or(true);
            control.allowed_users = section.allowed_users.unwrap_or_default();
        } else {
            control.enabled = true;
        }

        // ---- 环境变量（保留 TS 版名字）--------------------------------------
        if let Some(value) = env.get("HOST")
            && !value.is_empty()
        {
            defaults.host = value.to_owned();
        }
        if let Some(value) = env.get("PORT") {
            defaults.port = parse_port(value)?;
        }
        if let Some(value) = env.get("RELAY_TOKEN") {
            token = non_empty(value);
        }
        if let Some(value) = env.get("ALLOWED_HOSTS") {
            allowed_hosts = value
                .split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(str::to_owned)
                .collect();
        }
        if let Some(value) = env.get("TLS_REJECT_UNAUTHORIZED") {
            // 只有字面 `0` 算「关」，与 TS 版一致
            tls_reject_unauthorized = value.trim() != "0";
        }
        if let Some(value) = env.get("RELAY_TRACE") {
            defaults.trace = parse_bool("RELAY_TRACE", value)?;
        }
        if let Some(value) = env.get("RELAY_LOG_LEVEL") {
            log_level = LogLevel::parse(value)?;
        }

        // ---- CLI（优先级最高）------------------------------------------------
        if let Some(host) = &cli.host {
            defaults.host = host.clone();
        }
        if let Some(port) = cli.port {
            defaults.port = port;
        }
        if let Some(value) = &cli.token {
            token = non_empty(value);
        }
        if let Some(hosts) = &cli.allowed_hosts {
            allowed_hosts = hosts.clone();
        }
        if let Some(value) = &cli.log_level {
            log_level = LogLevel::parse(value)?;
        }
        if let Some(trace) = cli.trace {
            defaults.trace = trace;
        }
        if let Some(value) = cli.tls_reject_unauthorized {
            tls_reject_unauthorized = value;
        }

        Ok(Self {
            relay: defaults,
            token,
            allowed_hosts,
            tls_reject_unauthorized,
            log_level,
            log_max_mib: file.log_max_mib.unwrap_or(10),
            log_keep_files: file.log_keep_files.unwrap_or(5),
            service,
            update,
            control,
            warnings,
        })
    }

    /// 组合规则校验（`ai-docs/design/config-schema.md § 2.1`）。
    pub fn validate(&self) -> Result<(), AppError> {
        if self.relay.host.trim().is_empty() {
            return Err(AppError::Config("host 不能为空".to_owned()));
        }

        // ⚠️ 这是对 TS 版的**收紧**：监听非本机地址却不设 token / 白名单，
        // 等于把「能看到邮箱明文凭据的中继」开放给整个局域网。
        let loopback = is_loopback_host(&self.relay.host);
        if !loopback {
            if self.token.is_none() {
                return Err(AppError::Config(format!(
                    "host = {} 不是本机地址，必须同时设置 token（否则同网段的任何人都能用这个中继）",
                    self.relay.host
                )));
            }
            if self.allowed_hosts().is_empty() {
                return Err(AppError::Config(format!(
                    "host = {} 不是本机地址，必须同时设置 allowed_hosts（否则这个中继会变成任意目标的跳板）",
                    self.relay.host
                )));
            }
        }

        if self.relay.max_connections == 0 || self.relay.max_connections > 4096 {
            return Err(AppError::Config(format!(
                "max_connections 必须在 1..=4096 之间，当前是 {}",
                self.relay.max_connections
            )));
        }

        let delays = &self.relay.watch_retry_delays;
        if delays.is_empty() {
            return Err(AppError::Config(
                "watch_retry_delays_secs 不能为空（至少要有一个退避值）".to_owned(),
            ));
        }
        let mut previous = 0;
        for delay in delays {
            let secs = delay.as_secs();
            if secs < 1 {
                return Err(AppError::Config(
                    "watch_retry_delays_secs 里每一项都必须 ≥ 1 秒".to_owned(),
                ));
            }
            if secs < previous {
                return Err(AppError::Config(format!(
                    "watch_retry_delays_secs 必须递增，但出现了 {previous} → {secs}"
                )));
            }
            previous = secs;
        }

        if self.service.name.trim().is_empty() {
            return Err(AppError::Config("service.name 不能为空".to_owned()));
        }

        if self.update.check_interval_hours == 0 {
            return Err(AppError::Config(
                "update.check_interval_hours 必须 ≥ 1（0 会让检查变成死循环）".to_owned(),
            ));
        }

        Ok(())
    }

    /// 允许的邮件服务器（空 = 不限制，本机除外）。
    pub fn allowed_hosts(&self) -> &[String] {
        &self.allowed_hosts
    }

    /// 启动时要打出来的警告（含安全相关的持续提醒）。
    pub fn startup_warnings(&self) -> Vec<String> {
        let mut warnings = self.warnings.clone();
        if !self.tls_reject_unauthorized {
            warnings.push(
                "tls_reject_unauthorized = false：证书链不再校验，中间人可读走邮箱凭据。只应在测试环境使用"
                    .to_owned(),
            );
        }
        if self.relay.trace {
            warnings
                .push("trace = true：日志会包含 LOGIN 命令，也就是**邮箱授权码明文**".to_owned());
        }
        warnings
    }

    /// 引擎参数 + 访问 token + TLS 开关（`RelayServer::start_with` 的输入）。
    pub fn relay_options(&self) -> RelayOptions {
        self.relay.clone()
    }
}

/// 解析端口字符串。
fn parse_port(value: &str) -> Result<u16, AppError> {
    value.trim().parse::<u16>().map_err(|_| {
        AppError::Config(format!(
            "PORT 取值不合法：{value}（必须是 0..=65535 的整数，0 表示由系统分配）"
        ))
    })
}

/// 布尔解析：`1/true/yes/on` 为真，`0/false/no/off` 为假。
fn parse_bool(name: &str, value: &str) -> Result<bool, AppError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => Err(AppError::Config(format!(
            "{name} 取值不合法：{other}（可用：1/true/yes/on 或 0/false/no/off）"
        ))),
    }
}

/// 空串归一成 `None`。
fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// 是不是本机地址。
fn is_loopback_host(host: &str) -> bool {
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host == "::1"
        || host
            .parse::<std::net::IpAddr>()
            .map(|address| address.is_loopback())
            .unwrap_or(false)
}

/// 读文件：先按 `toml::Table` 检查未知键，再反序列化。
fn parse_file(path: &std::path::Path, warnings: &mut Vec<String>) -> Result<FileConfig, AppError> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| AppError::Config(format!("读不了配置文件 {}：{error}", path.display())))?;

    let table: toml::Table = toml::from_str(&text).map_err(|error| {
        AppError::Config(format!(
            "配置文件 {} 不是合法 TOML：{error}",
            path.display()
        ))
    })?;

    for (key, value) in &table {
        match key.as_str() {
            "service" => warn_unknown_section(value, &["mode", "autostart", "name"], key, warnings),
            "update" => warn_unknown_section(
                value,
                &[
                    "enabled",
                    "channel",
                    "check_interval_hours",
                    "verify_checksum",
                    "base_url",
                ],
                key,
                warnings,
            ),
            "control" => warn_unknown_section(value, &["enabled", "allowed_users"], key, warnings),
            known if KNOWN_TOP_LEVEL.contains(&known) => {}
            unknown => warnings.push(format!(
                "配置文件里有未知字段 `{unknown}`，已忽略（拼错了？）"
            )),
        }
    }

    table.try_into::<FileConfig>().map_err(|error| {
        AppError::Config(format!(
            "配置文件 {} 有取值不合法：{error}\n可选值：log_level = {}；service.mode = user | system；service.autostart = logon | boot | off；update.channel = stable | beta",
            path.display(),
            LogLevel::ALL.join(" | ")
        ))
    })
}

/// 顶级已知字段。
const KNOWN_TOP_LEVEL: [&str; 14] = [
    "host",
    "port",
    "token",
    "allowed_hosts",
    "tls_reject_unauthorized",
    "idle_timeout_secs",
    "max_connections",
    "backpressure_high_water_mib",
    "watch_reidle_secs",
    "watch_retry_delays_secs",
    "log_level",
    "trace",
    "log_max_mib",
    "log_keep_files",
];

/// 段内的未知字段。
fn warn_unknown_section(
    value: &toml::Value,
    known: &[&str],
    section: &str,
    warnings: &mut Vec<String>,
) {
    let Some(table) = value.as_table() else {
        warnings.push(format!("`{section}` 应当是一段表（[section]），已忽略"));
        return;
    };
    for key in table.keys() {
        if !known.contains(&key.as_str()) {
            warnings.push(format!("配置段 [{section}] 里有未知字段 `{key}`，已忽略"));
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn file_config(text: &str) -> FileConfig {
        let table: toml::Table = toml::from_str(text).expect("toml");
        table.try_into().expect("config")
    }

    fn merged(text: &str, env: &EnvSource, cli: &CliOverrides) -> Config {
        let mut warnings = Vec::new();
        Config::merge(file_config(text), env, cli, Vec::new())
            .map(|mut config| {
                config.warnings.append(&mut warnings);
                config
            })
            .expect("merge")
    }

    #[test]
    fn defaults_come_from_core() {
        let config = merged("", &EnvSource::default(), &CliOverrides::default());
        let core_default = RelayOptions::default();
        assert_eq!(config.relay.port, core_default.port);
        assert_eq!(config.relay.host, core_default.host);
        assert_eq!(config.relay.max_connections, core_default.max_connections);
        assert_eq!(
            config.relay.watch_retry_delays,
            core_default.watch_retry_delays
        );
        assert_eq!(config.log_level, LogLevel::Info);
        assert_eq!(config.service.level, ServiceLevel::User);
        assert_eq!(config.service.autostart, Autostart::Logon);
        assert_eq!(config.update.channel, UpdateChannel::Stable);
    }

    #[test]
    fn cli_beats_env_beats_file() {
        let env = EnvSource::from_pairs([("PORT", "2000"), ("HOST", "env-host")]);
        let cli = CliOverrides {
            port: Some(3000),
            ..CliOverrides::default()
        };

        let config = merged("port = 1000\nhost = \"file-host\"\n", &env, &cli);
        assert_eq!(config.relay.port, 3000, "CLI 最高");
        assert_eq!(config.relay.host, "env-host", "ENV 压过文件");

        let env_only = EnvSource::from_pairs([("PORT", "2000")]);
        let config = merged("port = 1000\n", &env_only, &CliOverrides::default());
        assert_eq!(config.relay.port, 2000, "ENV 压过文件");

        let config = merged(
            "port = 1000\n",
            &EnvSource::default(),
            &CliOverrides::default(),
        );
        assert_eq!(config.relay.port, 1000, "文件压过默认值");
    }

    #[test]
    fn the_legacy_environment_variables_still_work() {
        let env = EnvSource::from_pairs([
            ("HOST", "127.0.0.1"),
            ("PORT", "41317"),
            ("RELAY_TOKEN", "  s3cret  "),
            ("ALLOWED_HOSTS", "imap.qq.com, *.163.com ,, imap.gmail.com"),
            ("TLS_REJECT_UNAUTHORIZED", "0"),
            ("RELAY_TRACE", "1"),
        ]);
        let config = merged("", &env, &CliOverrides::default());

        assert_eq!(config.relay.port, 41317);
        assert_eq!(config.token.as_deref(), Some("s3cret"), "应当去掉首尾空白");
        assert_eq!(
            config.allowed_hosts,
            vec!["imap.qq.com", "*.163.com", "imap.gmail.com"],
            "逗号分隔、忽略空项"
        );
        assert!(!config.tls_reject_unauthorized);
        assert!(config.relay.trace);
    }

    #[test]
    fn only_a_literal_zero_disables_certificate_verification() {
        for (value, expected) in [
            ("0", false),
            ("false", true),
            ("no", true),
            ("off", true),
            ("1", true),
        ] {
            let env = EnvSource::from_pairs([("TLS_REJECT_UNAUTHORIZED", value)]);
            let config = merged("", &env, &CliOverrides::default());
            assert_eq!(config.tls_reject_unauthorized, expected, "value = {value}");
        }
    }

    #[test]
    fn an_invalid_enum_lists_the_allowed_values() {
        let env = EnvSource::from_pairs([("RELAY_LOG_LEVEL", "verbose")]);
        let error = Config::merge(
            FileConfig::default(),
            &env,
            &CliOverrides::default(),
            Vec::new(),
        )
        .expect_err("should fail");
        let message = error.to_string();
        assert!(message.contains("verbose"), "{message}");
        for allowed in LogLevel::ALL {
            assert!(message.contains(allowed), "缺少可选值 {allowed}：{message}");
        }

        let env = EnvSource::from_pairs([("RELAY_TRACE", "maybe")]);
        let error = Config::merge(
            FileConfig::default(),
            &env,
            &CliOverrides::default(),
            Vec::new(),
        )
        .expect_err("should fail");
        assert!(error.to_string().contains("1/true/yes/on"), "{}", error);
    }

    #[test]
    fn an_invalid_toml_enum_reports_the_options() {
        let mut warnings = Vec::new();
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("relay.toml");
        std::fs::write(&path, "[service]\nmode = \"root\"\n").expect("write");
        let error = parse_file(&path, &mut warnings).expect_err("should fail");
        let message = error.to_string();
        assert!(message.contains("user | system"), "{message}");
    }

    #[test]
    fn unknown_fields_warn_instead_of_failing() {
        let mut warnings = Vec::new();
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("relay.toml");
        std::fs::write(
            &path,
            "port = 41316\nmy_note = \"hi\"\n\n[service]\nmode = \"user\"\nnope = 1\n",
        )
        .expect("write");

        let config = parse_file(&path, &mut warnings).expect("should parse");
        assert_eq!(config.port, Some(41316));
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings.iter().any(|warning| warning.contains("my_note")));
        assert!(warnings.iter().any(|warning| warning.contains("nope")));
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let paths = Paths::for_test("C:\\definitely\\not\\there");
        let config = Config::load(&paths, &EnvSource::default(), &CliOverrides::default())
            .expect("missing file should fall back to defaults");
        assert_eq!(config.relay.port, RelayOptions::default().port);
    }

    #[test]
    fn a_non_loopback_host_requires_a_token_and_an_allow_list() {
        let config = merged(
            "host = \"0.0.0.0\"\n",
            &EnvSource::default(),
            &CliOverrides::default(),
        );
        let error = config.validate().expect_err("should fail");
        assert_eq!(error.exit_code(), crate::exit::ExitCode::Config);
        assert!(error.to_string().contains("token"), "{error}");

        let config = merged(
            "host = \"0.0.0.0\"\ntoken = \"s3cret\"\n",
            &EnvSource::default(),
            &CliOverrides::default(),
        );
        let error = config.validate().expect_err("should fail");
        assert!(error.to_string().contains("allowed_hosts"), "{error}");

        let config = merged(
            "host = \"0.0.0.0\"\ntoken = \"s3cret\"\nallowed_hosts = [\"imap.qq.com\"]\n",
            &EnvSource::default(),
            &CliOverrides::default(),
        );
        config.validate().expect("both set is fine");
    }

    #[test]
    fn loopback_hosts_are_recognised() {
        for host in ["127.0.0.1", "localhost", "::1", "[::1]"] {
            let config = merged(
                &format!("host = \"{host}\"\n"),
                &EnvSource::default(),
                &CliOverrides::default(),
            );
            config
                .validate()
                .unwrap_or_else(|error| panic!("{host} 应当是本机地址：{error}"));
        }
    }

    #[test]
    fn limits_and_backoff_ladders_are_validated() {
        let config = merged(
            "max_connections = 0\n",
            &EnvSource::default(),
            &CliOverrides::default(),
        );
        assert!(config.validate().is_err());

        let config = merged(
            "max_connections = 9999\n",
            &EnvSource::default(),
            &CliOverrides::default(),
        );
        assert!(config.validate().is_err());

        let config = merged(
            "watch_retry_delays_secs = []\n",
            &EnvSource::default(),
            &CliOverrides::default(),
        );
        assert!(
            config
                .validate()
                .expect_err("empty")
                .to_string()
                .contains("不能为空")
        );

        let config = merged(
            "watch_retry_delays_secs = [5, 2]\n",
            &EnvSource::default(),
            &CliOverrides::default(),
        );
        assert!(
            config
                .validate()
                .expect_err("not increasing")
                .to_string()
                .contains("递增")
        );

        let config = merged(
            "watch_retry_delays_secs = [0, 5]\n",
            &EnvSource::default(),
            &CliOverrides::default(),
        );
        assert!(
            config
                .validate()
                .expect_err("zero")
                .to_string()
                .contains("≥ 1")
        );
    }

    #[test]
    fn security_related_settings_produce_persistent_warnings() {
        let config = merged(
            "tls_reject_unauthorized = false\ntrace = true\n",
            &EnvSource::default(),
            &CliOverrides::default(),
        );
        let warnings = config.startup_warnings();
        assert!(warnings.iter().any(|warning| warning.contains("中间人")));
        assert!(warnings.iter().any(|warning| warning.contains("授权码")));
    }

    #[test]
    fn the_deprecated_backpressure_field_warns() {
        let config = merged(
            "backpressure_high_water_mib = 16\n",
            &EnvSource::default(),
            &CliOverrides::default(),
        );
        assert!(
            config
                .warnings
                .iter()
                .any(|warning| warning.contains("backpressure")),
            "{:?}",
            config.warnings
        );
    }

    #[test]
    fn relay_options_carry_the_merged_values() {
        let config = merged(
            "port = 41319\nidle_timeout_secs = 60\nwatch_reidle_secs = 120\n",
            &EnvSource::default(),
            &CliOverrides::default(),
        );
        let options = config.relay_options();
        assert_eq!(options.port, 41319);
        assert_eq!(options.idle_timeout, Duration::from_secs(60));
        assert_eq!(options.watch_reidle, Duration::from_secs(120));
    }

    #[test]
    fn the_config_path_can_be_overridden_by_env_then_cli() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path: &Path = directory.path();
        std::fs::write(path.join("a.toml"), "port = 1111\n").expect("write");
        std::fs::write(path.join("b.toml"), "port = 2222\n").expect("write");

        let paths = Paths::for_test(path.join("data"));
        let env = EnvSource::from_pairs([(
            "RELAY_CONFIG",
            path.join("a.toml").to_string_lossy().to_string(),
        )]);
        let config = Config::load(&paths, &env, &CliOverrides::default()).expect("load");
        assert_eq!(config.relay.port, 1111);

        let cli = CliOverrides {
            config: Some(path.join("b.toml")),
            ..CliOverrides::default()
        };
        let config = Config::load(&paths, &env, &cli).expect("load");
        assert_eq!(config.relay.port, 2222, "CLI 的 --config 压过 RELAY_CONFIG");
    }
}
/// 给 crate 内其它模块的测试用（`doctor` 的测试要造配置）。
#[cfg(test)]
pub(crate) mod test_support {
    use super::FileConfig;

    /// 从 TOML 文本构造原始配置。
    pub(crate) fn file_config(text: &str) -> FileConfig {
        let table: toml::Table = toml::from_str(text).expect("测试里的 TOML 应当合法");
        table.try_into().expect("测试里的字段应当合法")
    }
}
