use super::model::Config;
use crate::exit::AppError;
use peon_burrow_core::RelayOptions;

impl Config {
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

/// 是不是「只对本机可见」的监听地址。
///
/// ⚠️ 唯一实现放在 `peon_burrow_protocol::is_local_address`：从前 `config` 与 `doctor`
/// 各写一份，`doctor` 那份只认 `127.0.0.1` / `::1` / `localhost`，于是
/// `host = "[::1]"` 这种**安全**配置在诊断页被判 FAIL（退出码 2）。
/// `0.0.0.0` / `::` 不算（那是「所有网卡」，必须设 token）。
pub(super) fn is_loopback_host(host: &str) -> bool {
    peon_burrow_protocol::is_local_address(host)
}
