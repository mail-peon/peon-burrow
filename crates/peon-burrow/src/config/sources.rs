use super::model::{FileConfig, LogLevel};
use crate::exit::AppError;
use std::collections::BTreeMap;

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

/// 解析端口字符串。
pub(super) fn parse_port(value: &str) -> Result<u16, AppError> {
    value.trim().parse::<u16>().map_err(|_| {
        AppError::Config(format!(
            "PORT 取值不合法：{value}（必须是 0..=65535 的整数，0 表示由系统分配）"
        ))
    })
}

/// 布尔解析：`1/true/yes/on` 为真，`0/false/no/off` 为假。
pub(super) fn parse_bool(name: &str, value: &str) -> Result<bool, AppError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => Err(AppError::Config(format!(
            "{name} 取值不合法：{other}（可用：1/true/yes/on 或 0/false/no/off）"
        ))),
    }
}

/// 空串归一成 `None`。
pub(super) fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// 读文件：先按 `toml::Table` 检查未知键，再反序列化。
pub(super) fn parse_file(
    path: &std::path::Path,
    warnings: &mut Vec<String>,
) -> Result<FileConfig, AppError> {
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
pub(super) fn warn_unknown_section(
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
