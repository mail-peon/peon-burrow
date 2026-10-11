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

/// 目标是不是本机地址（上游目标用）。
///
/// ⚠️ 判据是「**解析成 IP** 之后看它是不是回环」，不是字符串前缀匹配：
/// 从前只认 `127.` 前缀 / `localhost` / `0.0.0.0` / `::1`，于是
/// `::ffff:127.0.0.1`（IPv4 映射）与 `0:0:0:0:0:0:0:1`（展开写法）都能绕过去。
///
/// `0.0.0.0` / `::` 在这里也算：它们不是回环，但作为**上游目标**没有意义
/// （连的是本机或直接被拒），历史行为同样是拒绝。
///
/// 域名指向本机（`localtest.me` → 127.0.0.1）字面量判断挡不住 ——
/// 那一条由建连之后的 `Policy::check_peer` 兜（见 `peon-burrow-core`）。
pub fn is_loopback(host: &str) -> bool {
    is_local_address(host)
        || matches!(parse_host_ip(host), Some(address) if address.is_unspecified())
}

/// 是不是「只对本机可见」的地址：`localhost` / `127.0.0.0/8` / `::1`（含 IPv4 映射）。
///
/// 这是**监听地址**与**上游目标**共用的那一份判断（从前各写一份，`doctor` 那份最窄，
/// 会把 `host = "[::1]"` 这种安全配置误报成 FAIL）：
/// - 监听（产品层的 `config` / `doctor`）：只绑本机才允许不设 token；
/// - 上游目标：指向本机的目标默认要拒（防把中继当 SSRF 跳板）。
///
/// ⚠️ `0.0.0.0` / `::` **不算**：作为监听地址它等于「所有网卡」（必须设 token）。
pub fn is_local_address(host: &str) -> bool {
    match parse_host_ip(host) {
        Some(address) => is_local_ip(address),
        None => normalize_host(host).eq_ignore_ascii_case("localhost"),
    }
}

/// [`is_local_address`] 的 `IpAddr` 版本。
///
/// 建连**之后**的 peer 复核要用它：`std::net::IpAddr::is_loopback()` 只认 `::1`，
/// `::ffff:127.0.0.1`（IPv4 映射）会漏掉 —— 而那是真实的攻击写法之一。
pub fn is_local_ip(address: std::net::IpAddr) -> bool {
    match address {
        std::net::IpAddr::V4(v4) => v4.is_loopback(),
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

/// 去掉空白与 IPv6 字面量的方括号（`[::1]` 是 URL 里的写法），再试解析成 IP。
fn parse_host_ip(host: &str) -> Option<std::net::IpAddr> {
    normalize_host(host).parse().ok()
}

/// 去空白 + 去方括号。
fn normalize_host(host: &str) -> &str {
    host.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim()
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
            "  localhost  ",
            "127.0.0.1",
            "127.1.2.3",
            "0.0.0.0",
            // `::` 是「未指定」：作为上游目标它连的是本机（历史行为也是拒绝）
            "::",
            "::1",
            "[::1]",
            // 从前这三种能绕过（只认字面量 `::1` / `127.` 前缀）：
            "0:0:0:0:0:0:0:1",
            "::ffff:127.0.0.1",
            "[::ffff:127.0.0.1]",
        ] {
            assert!(is_loopback(host), "{host} should be loopback");
        }
        for host in [
            "imap.qq.com",
            "10.0.0.1",
            "example.com",
            "1270.0.0.1",
            // IPv4 映射的**公网**地址不算本机
            "::ffff:8.8.8.8",
        ] {
            assert!(!is_loopback(host), "{host} should not be loopback");
        }
    }

    #[test]
    fn local_addresses_exclude_the_unspecified_ones() {
        // 监听地址用这份判断：`0.0.0.0` / `::` 是「所有网卡」，必须设 token，
        // 所以它们**不算**「只对本机可见」；而它们作为上游目标依然要被拒（见上一条）。
        for host in ["localhost", "127.0.0.1", "127.9.9.9", "::1", "[::1]"] {
            assert!(is_local_address(host), "{host} should be a local address");
        }
        for host in ["0.0.0.0", "::", "10.0.0.1", "imap.qq.com", ""] {
            assert!(!is_local_address(host), "{host} should not be local-only");
        }
        // 两种判断的关系：本机 ⇒ 回环
        for host in ["localhost", "127.0.0.1", "::1"] {
            assert!(is_loopback(host));
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
