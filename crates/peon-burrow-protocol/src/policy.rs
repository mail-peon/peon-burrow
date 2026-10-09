//! 访问控制里**纯规则**的那部分：白名单匹配、loopback 判定、拒绝原因。
//!
//! 策略的**执行**（什么时机检查、被拒之后怎么关连接）在 `peon-burrow-core`；
//! 这里只回答「这个 host 允许吗」这种没有副作用的问题。

/// 一条连接被拒绝的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// `RELAY_TOKEN` 已设置，但请求里的 token 不匹配。
    InvalidToken,
    /// host 不在 `ALLOWED_HOSTS` 里。
    HostNotAllowed,
    /// 目标是本机地址，但没有显式写进 `ALLOWED_HOSTS`（防 SSRF 跳板）。
    LoopbackNotAllowed,
    /// `tls=0` 却要连 993（implicit TLS 端口）。
    PlaintextOnImplicitTlsPort,
    /// 请求里既没有目标，也不是 watch 请求。
    MissingTarget,
    /// watch 请求缺少有效的 host / port。
    InvalidTarget,
    /// watch 请求缺少 user / pass。
    MissingCredentials,
    /// 本构建没有启用 `imap-watch`。
    WatchNotEnabled,
}

impl RejectReason {
    /// 给用户看的文案（会被用作 WebSocket 关闭原因，调用方负责按字节截断）。
    ///
    /// ⚠️ 这些文案与 TS 版逐字一致（见 `ai-docs/design/wire-protocol.md § 2.2`）——
    /// 排查问题时用户拿到的就是这句，别随手改。
    pub fn message(self) -> &'static str {
        match self {
            Self::InvalidToken => "invalid token",
            Self::HostNotAllowed => "host not allowed",
            Self::LoopbackNotAllowed => "拒绝连接本机地址（请显式加进 ALLOWED_HOSTS）",
            Self::PlaintextOnImplicitTlsPort => {
                "993 端口是 implicit TLS，必须开启 TLS（143 + STARTTLS 本版本不支持）"
            }
            Self::MissingTarget => "缺少目标 host（用 /host:port 或 ?host=&port=）",
            Self::InvalidTarget => "watch 请求缺少有效的 host / port",
            Self::MissingCredentials => "watch 请求缺少 user / pass",
            Self::WatchNotEnabled => "本构建未启用 imap-watch（IMAP IDLE 推送不可用）",
        }
    }
}

/// host 是否命中白名单。
///
/// 规则与 TS 版一致：大小写不敏感；`pattern` 里含 `*` 时按通配匹配（`*` 匹配任意序列），
/// 其余字符都按**字面量**比较 —— 所以 `*.gmail.com` 里的 `.` 不会变成「任意字符」。
///
/// ⚠️ `patterns` 为空时返回 `false`：**空白名单 = 不允许任何 host**。
/// 「空 = 允许任意」这条语义由调用方（`core` 的策略层）处理，库不该替它猜。
pub fn is_host_allowed(host: &str, patterns: &[String]) -> bool {
    let host = host.trim().to_ascii_lowercase();
    if host.is_empty() {
        return false;
    }
    patterns.iter().any(|pattern| {
        let pattern = pattern.trim().to_ascii_lowercase();
        if pattern.is_empty() {
            return false;
        }
        if pattern == host {
            return true;
        }
        pattern.contains('*') && glob_match(&pattern, &host)
    })
}

/// 目标是不是本机地址。
pub fn is_loopback(host: &str) -> bool {
    let host = host.trim().to_ascii_lowercase();
    host == "localhost"
        || host.starts_with("127.")
        || host == "0.0.0.0"
        || host == "::1"
        || host == "[::1]"
}

/// 通配匹配：`*` 匹配任意序列（含空串），其余字节按字面量。
///
/// 用两指针 + 回溯实现，不引入正则 —— 协议层要保持零依赖、零编译成本。
fn glob_match(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let (mut p, mut v) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut mark = 0usize;

    while v < value.len() {
        if p < pattern.len() && pattern[p] == value[v] {
            p += 1;
            v += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            mark = v;
        } else if let Some(star_at) = star {
            p = star_at + 1;
            mark += 1;
            v = mark;
        } else {
            return false;
        }
    }

    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    #[test]
    fn exact_match_is_case_insensitive() {
        assert!(is_host_allowed("imap.qq.com", &list(&["imap.qq.com"])));
        assert!(is_host_allowed("IMAP.QQ.COM", &list(&["imap.qq.com"])));
        assert!(is_host_allowed("imap.qq.com", &list(&["IMAP.QQ.COM"])));
    }

    #[test]
    fn wildcard_matches_a_subdomain_only() {
        let patterns = list(&["*.gmail.com"]);
        assert!(is_host_allowed("imap.gmail.com", &patterns));
        assert!(is_host_allowed("smtp.gmail.com", &patterns));
        assert!(!is_host_allowed("gmail.com", &patterns));
    }

    #[test]
    fn a_dot_in_a_pattern_is_literal() {
        // `*.qq.com` 不该匹配 `imapxqq.com`
        let patterns = list(&["*.qq.com"]);
        assert!(is_host_allowed("imap.qq.com", &patterns));
        assert!(!is_host_allowed("imapxqq.com", &patterns));
    }

    #[test]
    fn a_bare_star_matches_everything() {
        let patterns = list(&["*"]);
        assert!(is_host_allowed("imap.qq.com", &patterns));
        assert!(is_host_allowed("127.0.0.1", &patterns));
    }

    #[test]
    fn star_inside_a_pattern_works() {
        let patterns = list(&["imap.*.com"]);
        assert!(is_host_allowed("imap.qq.com", &patterns));
        assert!(is_host_allowed("imap.foo.bar.com", &patterns));
        assert!(!is_host_allowed("smtp.qq.com", &patterns));
    }

    #[test]
    fn an_empty_whitelist_allows_nothing() {
        // 「空 = 允许任意」由 core 的策略层处理，库这里必须保守
        assert!(!is_host_allowed("imap.qq.com", &[]));
    }

    #[test]
    fn empty_entries_are_ignored() {
        assert!(!is_host_allowed("imap.qq.com", &list(&["", "   "])));
        assert!(is_host_allowed("imap.qq.com", &list(&["", "imap.qq.com"])));
    }

    #[test]
    fn loopback_recognises_the_usual_spellings() {
        for host in [
            "localhost",
            "LOCALHOST",
            "127.0.0.1",
            "127.1.2.3",
            "0.0.0.0",
            "::1",
            "[::1]",
        ] {
            assert!(is_loopback(host), "{host} should be loopback");
        }
        for host in ["imap.qq.com", "10.0.0.1", "example.com", "1270.0.0.1"] {
            assert!(!is_loopback(host), "{host} should not be loopback");
        }
    }

    #[test]
    fn messages_are_stable() {
        // 这些字符串会被写进关闭帧，改动等于改协议
        assert_eq!(RejectReason::InvalidToken.message(), "invalid token");
        assert_eq!(RejectReason::HostNotAllowed.message(), "host not allowed");
        assert!(
            RejectReason::LoopbackNotAllowed
                .message()
                .starts_with("拒绝连接本机地址")
        );
        assert!(
            RejectReason::PlaintextOnImplicitTlsPort
                .message()
                .contains("implicit TLS")
        );
        assert!(
            RejectReason::WatchNotEnabled
                .message()
                .contains("imap-watch")
        );
    }
}
