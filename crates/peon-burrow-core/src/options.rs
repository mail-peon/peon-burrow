//! 引擎的运行期参数。
//!
//! ⚠️ **默认值只在这里定义一次**（布局铁律 L3）：产品层（`peon-burrow`）的 TOML 字段全是
//! `Option`，缺省就落到这里的 `Default`。别在别处再写一遍默认值 —— 两处同义异写是隐性 bug。

use std::time::Duration;

/// 中继的运行期参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayOptions {
    /// 监听地址。默认只绑本机 —— 中继能看到邮箱明文凭据。
    pub host: String,
    /// 监听端口。`0` 表示由内核分配（测试用）。
    pub port: u16,
    /// 透传连接的空闲超时；**不作用于 watch**（IDLE 会长期静默）。
    pub idle_timeout: Duration,
    /// 并发连接上限（一个账号的 watch 占 1 条）。
    pub max_connections: usize,
    /// 建立上游连接（TCP/TLS 握手）的超时。
    pub connect_timeout: Duration,
    /// watch 模式重发 `IDLE` 的间隔（RFC 2177 建议 ≤ 29 分钟）。
    pub watch_reidle: Duration,
    /// watch 断开后的重连退避阶梯。
    pub watch_retry_delays: Vec<Duration>,
    /// 是否把每条连接的字节打进日志（**含明文凭据**，默认关）。
    pub trace: bool,
}

impl Default for RelayOptions {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_owned(),
            port: 41316,
            idle_timeout: Duration::from_secs(15 * 60),
            max_connections: 32,
            connect_timeout: Duration::from_secs(20),
            watch_reidle: Duration::from_secs(25 * 60),
            watch_retry_delays: [2, 5, 15, 30, 60, 120, 300]
                .into_iter()
                .map(Duration::from_secs)
                .collect(),
            trace: false,
        }
    }
}

impl RelayOptions {
    /// 便捷构造：只关心监听地址与端口。
    pub fn on(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_documented_values() {
        let options = RelayOptions::default();
        assert_eq!(options.host, "127.0.0.1");
        assert_eq!(options.port, 41316);
        assert_eq!(options.idle_timeout, Duration::from_secs(900));
        assert_eq!(options.max_connections, 32);
        assert_eq!(options.watch_reidle, Duration::from_secs(1500));
        assert_eq!(
            options.watch_retry_delays.first(),
            Some(&Duration::from_secs(2))
        );
        assert_eq!(
            options.watch_retry_delays.last(),
            Some(&Duration::from_secs(300))
        );
        assert!(!options.trace);
    }
}
