//! 把 WebSocket 升级请求的目标串解析成 [`RelayTarget`]。
//!
//! 支持两种形态（扩展侧 `buildRelayUrl()` 会产出这两种）：
//! - 路径：`/imap.qq.com:993?tls=1`
//! - 查询：`/tunnel?host=imap.qq.com&port=993&tls=1`

use crate::policy::RejectReason;

/// 默认 TLS 端口（implicit TLS）。
pub const DEFAULT_TLS_PORT: u16 = 993;

/// 默认明文端口（STARTTLS 本版本不支持，所以只是「把字节原样发过去」）。
pub const DEFAULT_PLAIN_PORT: u16 = 143;

/// 一次连接的目标。
///
/// `tls` 为真时由中继与邮件服务器完成 TLS 握手（**中继因此能看到明文**，
/// 见 `ai-docs/00-overview.md § 6）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayTarget {
    /// 邮件服务器主机名。
    pub host: String,
    /// 端口。
    pub port: u16,
    /// 是否用 TLS 连接。
    pub tls: bool,
    /// 请求里带的 token（空串与缺失都记作 `None`）。
    pub token: Option<String>,
}

/// [`resolve_target`] 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetResolve {
    /// 解析成功。
    Target(RelayTarget),
    /// 这个 URL 里**没有目标** —— 这不是错误：watch 请求故意不带目标，要等第一帧再决定。
    NoTarget,
    /// 必须**立刻**拒绝（不能等第一帧，否则就成了「被静默放行」）。
    Rejected(RejectReason),
}

/// 解析升级请求的目标。
///
/// 规则与 TS 版逐条对齐（见 `ai-docs/04-parity-node-to-rust.md` W2–W4、C4–C6）：
/// 查询参数优先于路径；`tls` 只有字面 `0` 才算明文；端口缺失/非法时按 `tls` 取 993 / 143；
/// `tls=0` 配 993 直接拒绝。
pub fn resolve_target(raw: &str) -> TargetResolve {
    let (path, query) = match raw.split_once('?') {
        Some((path, query)) => (path, query),
        None => (raw, ""),
    };
    let params = parse_query(query);

    let mut host = query_value(&params, "host").unwrap_or_default();
    let mut port = query_value(&params, "port").and_then(|value| parse_port(&value));

    if host.is_empty()
        && let Some((path_host, path_port)) = parse_path(path)
    {
        host = path_host;
        port = port.or(path_port);
    }

    if host.is_empty() {
        return TargetResolve::NoTarget;
    }

    let tls = query_value(&params, "tls")
        .map(|value| value != "0")
        .unwrap_or(true);
    let port = port.unwrap_or(if tls {
        DEFAULT_TLS_PORT
    } else {
        DEFAULT_PLAIN_PORT
    });

    if !tls && port == DEFAULT_TLS_PORT {
        return TargetResolve::Rejected(RejectReason::PlaintextOnImplicitTlsPort);
    }

    TargetResolve::Target(RelayTarget {
        host,
        port,
        tls,
        token: query_value(&params, "token").filter(|value| !value.is_empty()),
    })
}

/// 解析 `k=v&k2=v2`，百分号解码，**同名取第一个**（与 `URLSearchParams.get` 一致）。
fn parse_query(query: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = match pair.split_once('=') {
            Some((key, value)) => (key, value),
            None => (pair, ""),
        };
        out.push((percent_decode(key), percent_decode(value)));
    }
    out
}

/// 从查询参数里取值（同名取第一个）。
fn query_value(params: &[(String, String)], key: &str) -> Option<String> {
    params
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.clone())
}

/// 从路径里解析 `host[:port]`，支持 IPv6 的 `[::1]:993` 写法。
///
/// 与 TS 版的正则等价：端口部分必须全是数字，否则整条路径**不算匹配**
/// （`/imap.qq.com:` 这种会退化成「没有目标」）。
fn parse_path(path: &str) -> Option<(String, Option<u16>)> {
    let decoded = percent_decode(path.trim_start_matches('/'));
    if decoded.is_empty() {
        return None;
    }
    if let Some(rest) = decoded.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        if host.is_empty() {
            return None;
        }
        return match after.strip_prefix(':') {
            // 端口部分必须全是数字，否则整条路径不算匹配（与 TS 的正则等价）
            Some(port) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
                Some((host.to_owned(), parse_port(port)))
            }
            None if after.is_empty() => Some((host.to_owned(), None)),
            _ => None,
        };
    }
    match decoded.split_once(':') {
        Some((host, port)) => {
            if host.is_empty() || port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            Some((host.to_owned(), parse_port(port)))
        }
        None => Some((decoded, None)),
    }
}

/// 解析一个端口号；非法时返回 `None`（调用方回落到默认端口）。
fn parse_port(text: &str) -> Option<u16> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse::<u16>().ok().filter(|port| *port > 0)
}

/// 极简百分号解码。
///
/// 非法序列（`%zz`、截断的 `%4`）**原样保留** —— `decodeURIComponent` 会抛异常，
/// 而中继收到一个畸形路径时该做的是「按字面量处理」而不是崩掉。
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = &text[index + 1..index + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> RelayTarget {
        match resolve_target(raw) {
            TargetResolve::Target(target) => target,
            other => panic!("expected a target for {raw}, got {other:?}"),
        }
    }

    #[test]
    fn path_form_yields_host_and_port() {
        let target = parse("/imap.qq.com:993?tls=1");
        assert_eq!(target.host, "imap.qq.com");
        assert_eq!(target.port, 993);
        assert!(target.tls);
    }

    #[test]
    fn query_form_is_supported() {
        let target = parse("/tunnel?host=imap.qq.com&port=993&tls=1");
        assert_eq!(target.host, "imap.qq.com");
        assert_eq!(target.port, 993);
        assert!(target.tls);
    }

    #[test]
    fn query_wins_over_path() {
        let target = parse("/path.example.com:143?host=imap.qq.com&port=993");
        assert_eq!(target.host, "imap.qq.com");
        assert_eq!(target.port, 993);
    }

    #[test]
    fn tls_defaults_to_on() {
        assert!(parse("/imap.qq.com:993").tls);
        assert!(parse("/imap.qq.com:143?tls=1").tls);
        // 只有字面 `0` 才算明文（`false` / `no` 都仍是 TLS）—— 与 TS 版一致
        assert!(!parse("/imap.qq.com:143?tls=0").tls);
        assert!(parse("/imap.qq.com:143?tls=false").tls);
    }

    #[test]
    fn port_defaults_follow_tls() {
        assert_eq!(parse("/imap.qq.com").port, DEFAULT_TLS_PORT);
        assert_eq!(parse("/imap.qq.com?tls=0").port, DEFAULT_PLAIN_PORT);
    }

    #[test]
    fn an_out_of_range_port_falls_back_to_the_default() {
        // 端口是数字但越界 → 保留 host、端口回落（与 TS 版一致）
        assert_eq!(parse("/imap.qq.com:70000").port, DEFAULT_TLS_PORT);
        assert_eq!(parse("/imap.qq.com:0").port, DEFAULT_TLS_PORT);
        assert_eq!(parse("/imap.qq.com:993?port=nope").port, DEFAULT_TLS_PORT);
    }

    #[test]
    fn ipv6_targets_keep_their_brackets() {
        let target = parse("/[::1]:993?tls=1");
        assert_eq!(target.host, "::1");
        assert_eq!(target.port, 993);
        let target = parse("/[2001:db8::1]");
        assert_eq!(target.host, "2001:db8::1");
        assert_eq!(target.port, DEFAULT_TLS_PORT);
    }

    #[test]
    fn percent_encoded_paths_are_decoded() {
        let target = parse("/imap%2Eqq%2Ecom:993");
        assert_eq!(target.host, "imap.qq.com");
    }

    #[test]
    fn no_target_is_not_an_error() {
        // watch 请求故意不带目标：调用方要留着这条连接等第一帧
        assert_eq!(resolve_target("/"), TargetResolve::NoTarget);
        assert_eq!(resolve_target(""), TargetResolve::NoTarget);
        assert_eq!(resolve_target("/?tls=1"), TargetResolve::NoTarget);
    }

    #[test]
    fn plaintext_on_993_is_rejected() {
        assert_eq!(
            resolve_target("/imap.qq.com:993?tls=0"),
            TargetResolve::Rejected(RejectReason::PlaintextOnImplicitTlsPort)
        );
    }

    #[test]
    fn token_rides_along_and_empty_means_none() {
        assert_eq!(
            parse("/imap.qq.com:993?token=s3cret").token.as_deref(),
            Some("s3cret")
        );
        assert_eq!(parse("/imap.qq.com:993?token=").token, None);
        assert_eq!(parse("/imap.qq.com:993").token, None);
    }

    #[test]
    fn a_path_with_a_broken_port_is_not_a_target() {
        // `/imap.qq.com:` 在 TS 的正则里不匹配 → 没有目标（留给第一帧）
        assert_eq!(resolve_target("/imap.qq.com:"), TargetResolve::NoTarget);
    }
}
