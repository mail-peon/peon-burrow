use super::model::*;
use super::sources::{EnvSource, non_empty, parse_bool, parse_port};
use crate::exit::AppError;
use peon_burrow_core::RelayOptions;
use std::time::Duration;

impl Config {
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
}
