use crate::error::UpdateError;

/// 解析出来的目标地址。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Endpoint {
    /// 是否走 TLS。
    pub(super) https: bool,
    /// 主机名（IPv6 字面量不带方括号）。
    pub(super) host: String,
    /// 端口（缺省时按 scheme 填 443 / 80）。
    pub(super) port: u16,
    /// 请求路径（以 `/` 开头，原样发送）。
    pub(super) path: String,
}

impl Endpoint {
    /// `Host` 头：非默认端口才带端口。
    pub(super) fn host_header(&self) -> String {
        if self.port == default_port(self.https) {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// scheme 的默认端口。
pub(super) fn default_port(https: bool) -> u16 {
    if https { 443 } else { 80 }
}

/// 解析 `http(s)://host[:port]/path`。
///
/// 刻意不引 `url` crate：我们只需要「scheme / host / port / path」四件事，
/// 而且是**发送方**，路径按原样送出去最不容易出意外（清单与资产名都是 ASCII 安全字符）。
pub(super) fn parse_url(url: &str) -> Result<Endpoint, UpdateError> {
    let bad = |reason: &str| UpdateError::BadUrl {
        url: url.to_string(),
        reason: reason.to_string(),
    };

    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| bad("缺少 scheme://（形如 https://host/path）"))?;
    let https = match scheme {
        "https" => true,
        "http" => false,
        other => return Err(bad(&format!("只支持 http / https，收到 {other}"))),
    };

    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err(bad("没有主机名"));
    }
    let path = path.split('#').next().unwrap_or("/").to_string();

    let (host, port) = split_authority(authority).map_err(|reason| bad(&reason))?;
    Ok(Endpoint {
        https,
        host,
        port: port.unwrap_or_else(|| default_port(https)),
        path,
    })
}

/// 把 `authority` 拆成 `(host, Option<port>)`；支持 `[::1]:8443` 这种 IPv6 字面量。
pub(super) fn split_authority(authority: &str) -> Result<(String, Option<u16>), String> {
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| format!("IPv6 字面量缺少 ']'：{authority}"))?;
        let port = match tail.strip_prefix(':') {
            Some(port) => Some(parse_port(port)?),
            None if tail.is_empty() => None,
            None => return Err(format!("主机名后面有多余内容：{authority}")),
        };
        (host.to_string(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host.to_string(), Some(parse_port(port)?)),
            None => (authority.to_string(), None),
        }
    };

    if host.is_empty() {
        return Err(format!("主机名为空：{authority}"));
    }
    Ok((host, port))
}

/// 解析端口。
pub(super) fn parse_port(port: &str) -> Result<u16, String> {
    port.parse::<u16>()
        .map_err(|_| format!("端口不是 0-65535 的数字：{port}"))
}

/// 把 `Location` 解析成绝对 URL（支持绝对地址与以 `/` 开头的相对地址）。
pub(super) fn resolve_location(base: &str, location: &str) -> Result<String, UpdateError> {
    if location.starts_with("http://") || location.starts_with("https://") {
        return Ok(location.to_string());
    }
    if location.starts_with('/') {
        let (scheme, rest) = base.split_once("://").ok_or_else(|| UpdateError::BadUrl {
            url: base.to_string(),
            reason: "缺少 scheme://".to_string(),
        })?;
        let authority = rest.split('/').next().unwrap_or("");
        return Ok(format!("{scheme}://{authority}{location}"));
    }
    Err(UpdateError::BadUrl {
        url: location.to_string(),
        reason: "不支持这种相对重定向（只支持绝对地址或 / 开头）".to_string(),
    })
}
