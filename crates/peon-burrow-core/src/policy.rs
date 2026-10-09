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
}
