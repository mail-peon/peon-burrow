//! 访问策略：**执行**那半边（规则本身在 `peon-burrow-protocol`）。
//!
//! 默认实现就是 TS 版那三条：token 比对、host 白名单、禁止把中继当成本机跳板。
//! 想换一套（公司内网白名单、动态 token）就实现 [`Policy`] 再用
//! `RelayServer::start_with` 注入 —— 这是给接入方留的第一个口子。

use peon_burrow_protocol::{RejectReason, RelayTarget, is_host_allowed, is_loopback};

/// 一条连接能不能继续。
///
/// 实现必须是**同步**的：策略检查发生在建连之前，且被拒的连接一个字节都不许发出去
/// （见 `ai-docs/04-parity-node-to-rust.md` 的 I3 / I4）。
pub trait Policy: Send + Sync + std::fmt::Debug {
    /// 检查目标；`Err` 表示拒绝以及原因。
    fn check(&self, target: &RelayTarget) -> Result<(), RejectReason>;

    /// **建连之后**再核一次：解析出来的对端地址如果指向本机，同样要拒。
    ///
    /// 为什么需要第二道闸：`check()` 只能看域名**字面量**，而 `localtest.me` 这类
    /// 公网域名（A 记录 127.0.0.1）能让中继去连本机服务 —— 文档把「不给本机跳板」
    /// 当安全属性（`ai-docs/04-parity-node-to-rust.md:110`），所以必须拿
    /// `peer_addr()` 复核一次（在策略里解析 DNS 会有 TOCTOU，不做）。
    ///
    /// 默认实现放行：自定义策略不关心这件事就不必实现。
    fn check_peer(&self, target: &RelayTarget, peer: std::net::IpAddr) -> Result<(), RejectReason> {
        let _ = (target, peer);
        Ok(())
    }
}

/// 默认策略：`token` + `allowed_hosts` + loopback 保护。
///
/// 语义（与 TS 版一致）：
/// - `token` 为 `None` / 空串 → 不要求 token；
/// - `allowed_hosts` 为空 → 允许任意 host，**但本机地址除外**（防 SSRF 跳板）；
/// - 本机地址要连，必须**显式**写进 `allowed_hosts`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PolicyRules {
    token: Option<String>,
    allowed_hosts: Vec<String>,
}

impl PolicyRules {
    /// 构造一套规则；空 token 会被归一成「不要求」。
    pub fn new(token: Option<String>, allowed_hosts: Vec<String>) -> Self {
        Self {
            token: token.filter(|value| !value.is_empty()),
            allowed_hosts,
        }
    }

    /// 要求的 token（`None` = 不要求）。
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// 允许的 host 模式（空 = 允许任意，loopback 除外）。
    pub fn allowed_hosts(&self) -> &[String] {
        &self.allowed_hosts
    }
}

impl Policy for PolicyRules {
    fn check(&self, target: &RelayTarget) -> Result<(), RejectReason> {
        if let Some(expected) = &self.token {
            let provided = target.token.as_deref().unwrap_or("");
            if provided != expected {
                return Err(RejectReason::InvalidToken);
            }
        }

        let listed = is_host_allowed(&target.host, &self.allowed_hosts);

        if !self.allowed_hosts.is_empty() && !listed {
            return Err(RejectReason::HostNotAllowed);
        }

        if is_loopback(&target.host) && !listed {
            return Err(RejectReason::LoopbackNotAllowed);
        }

        Ok(())
    }

    /// 见 [`Policy::check_peer`]：域名解析到本机也要拒（`localtest.me` → 127.0.0.1）。
    ///
    /// 白名单里的 host 仍然放行 —— 那是用户**显式**说的「我就要连本机」
    /// （与 `check()` 里 `listed` 的例外保持一致）。
    fn check_peer(&self, target: &RelayTarget, peer: std::net::IpAddr) -> Result<(), RejectReason> {
        if peon_burrow_protocol::is_local_ip(peer)
            && !is_host_allowed(&target.host, &self.allowed_hosts)
        {
            return Err(RejectReason::LoopbackNotAllowed);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(host: &str, token: Option<&str>) -> RelayTarget {
        RelayTarget {
            host: host.to_owned(),
            port: 993,
            tls: true,
            token: token.map(str::to_owned),
        }
    }

    fn rules(token: Option<&str>, hosts: &[&str]) -> PolicyRules {
        PolicyRules::new(
            token.map(str::to_owned),
            hosts.iter().map(|host| (*host).to_owned()).collect(),
        )
    }

    #[test]
    fn no_token_configured_means_no_requirement() {
        let rules = rules(None, &[]);
        assert_eq!(rules.check(&target("imap.qq.com", None)), Ok(()));
        assert_eq!(
            rules.check(&target("imap.qq.com", Some("anything"))),
            Ok(())
        );
    }

    #[test]
    fn an_empty_token_is_normalised_to_no_requirement() {
        assert_eq!(rules(Some(""), &[]).token(), None);
    }

    #[test]
    fn a_configured_token_must_match() {
        let rules = rules(Some("s3cret"), &[]);
        assert_eq!(rules.check(&target("imap.qq.com", Some("s3cret"))), Ok(()));
        assert_eq!(
            rules.check(&target("imap.qq.com", Some("wrong"))),
            Err(RejectReason::InvalidToken)
        );
        assert_eq!(
            rules.check(&target("imap.qq.com", None)),
            Err(RejectReason::InvalidToken)
        );
    }

    #[test]
    fn an_empty_allow_list_permits_any_remote_host() {
        let rules = rules(None, &[]);
        assert_eq!(rules.check(&target("imap.qq.com", None)), Ok(()));
        assert_eq!(rules.check(&target("imap.163.com", None)), Ok(()));
    }

    #[test]
    fn an_allow_list_rejects_everything_else() {
        let rules = rules(None, &["*.qq.com"]);
        assert_eq!(rules.check(&target("imap.qq.com", None)), Ok(()));
        assert_eq!(
            rules.check(&target("imap.gmail.com", None)),
            Err(RejectReason::HostNotAllowed)
        );
    }

    #[test]
    fn loopback_is_refused_unless_listed_explicitly() {
        // 空白名单允许任意 host，但本机地址是例外（防 SSRF 跳板）
        let open = rules(None, &[]);
        assert_eq!(
            open.check(&target("127.0.0.1", None)),
            Err(RejectReason::LoopbackNotAllowed)
        );
        assert_eq!(
            open.check(&target("localhost", None)),
            Err(RejectReason::LoopbackNotAllowed)
        );

        let listed = rules(None, &["127.0.0.1", "localhost"]);
        assert_eq!(listed.check(&target("127.0.0.1", None)), Ok(()));
        assert_eq!(listed.check(&target("localhost", None)), Ok(()));
    }

    #[test]
    fn loopback_in_a_non_empty_list_still_needs_its_own_entry() {
        let rules = rules(None, &["*.qq.com"]);
        assert_eq!(
            rules.check(&target("127.0.0.1", None)),
            Err(RejectReason::HostNotAllowed)
        );
    }

    #[test]
    fn a_peer_that_resolves_to_loopback_is_refused_too() {
        // `check()` 只能看域名**字面量**：`localtest.me` 是公网域名、A 记录是 127.0.0.1，
        // 所以必须靠建连之后的 `check_peer` 复核（文档把这条当安全属性）。
        let open = rules(None, &[]);
        let localtest = target("localtest.me", None);
        for peer in ["127.0.0.1", "::1", "::ffff:127.0.0.1"] {
            assert_eq!(
                open.check_peer(&localtest, peer.parse().expect("ip")),
                Err(RejectReason::LoopbackNotAllowed),
                "{peer} 应当被拒"
            );
        }
        // 公网对端照旧放行
        assert_eq!(
            open.check_peer(&localtest, "8.8.8.8".parse().expect("ip")),
            Ok(())
        );

        // 显式写进白名单 = 用户说了「我就要连本机」，两道闸都放行
        let listed = rules(None, &["localtest.me"]);
        assert_eq!(listed.check(&localtest), Ok(()));
        assert_eq!(
            listed.check_peer(&localtest, "127.0.0.1".parse().expect("ip")),
            Ok(())
        );

        // 默认实现（自定义策略不关心这件事）放行 —— 这条守的是「不破坏接入方」
        #[derive(Debug)]
        struct Anything;
        impl Policy for Anything {
            fn check(&self, _target: &RelayTarget) -> Result<(), RejectReason> {
                Ok(())
            }
        }
        assert_eq!(
            Anything.check_peer(&localtest, "127.0.0.1".parse().expect("ip")),
            Ok(())
        );
    }
}
