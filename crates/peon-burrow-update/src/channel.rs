//! 渠道与 URL 策略：**一个固定清单 URL**，不碰 GitHub API。
//!
//! 为什么不调 GitHub API 列表接口（`/releases`）「自己找最新版本」：
//!
//! - 匿名 API 是 **60 次/小时/IP**，而一个出口 IP 后面可能是整个办公室；
//! - 镜像站大多只代理 `download` 路径，**不代理 API** → 走镜像就没法查版本；
//! - `releases/latest/download/...` 是**重定向**，不计 rate limit，且镜像站只要替换
//!   `https://github.com` 前缀就能整段代理。
//!
//! 所以：清单 URL 是**静态**的，镜像只改前缀（`ai-docs/decisions/adr-0005-self-update.md § 1、§ 2`）。

use std::fmt;

/// GitHub 原始前缀；设了 `base_url` 时整段被替换。
pub const GITHUB_ORIGIN: &str = "https://github.com";

/// 本仓库在 GitHub 上的 `owner/repo`。
pub const REPO: &str = "mail-peon/peon-burrow";

/// 清单文件名（固定叫这个，镜像站按路径代理）。
pub const MANIFEST_FILE: &str = "latest.json";

/// 更新渠道。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Channel {
    /// 正式渠道：`releases/latest/download/latest.json`（`latest` 是 GitHub 的重定向）。
    Stable,
    /// 预发布渠道：滚动 tag `beta-latest`（预发布版本不出现在 `latest` 里）。
    Beta,
}

impl Channel {
    /// 配置里用的字符串（`stable` / `beta`）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Beta => "beta",
        }
    }

    /// 清单在 GitHub 上的路径（以 `/` 开头，拼接在前缀后面）。
    ///
    /// 两条路径**都是固定 URL**：不需要 API、不计 rate limit、镜像可整段代理。
    #[must_use]
    pub fn manifest_path(self) -> &'static str {
        match self {
            // 仓库定的取值：owner = mail-peon，repo = peon-burrow
            Channel::Stable => "/mail-peon/peon-burrow/releases/latest/download/latest.json",
            // 滚动 tag：预发布不会出现在 latest 里，所以 beta 用固定 tag
            Channel::Beta => "/mail-peon/peon-burrow/releases/download/beta-latest/latest.json",
        }
    }

    /// 从配置字符串解析；`stable` / `beta`，大小写不敏感，其余返回 `None`。
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "stable" => Some(Channel::Stable),
            "beta" => Some(Channel::Beta),
            _ => None,
        }
    }
}

impl fmt::Display for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 清单 URL：`base_url` **替换** `https://github.com` 前缀，`None` 就是 GitHub 本身。
///
/// ```no_run
/// # use peon_burrow_update::{manifest_url, Channel};
/// assert_eq!(
///     manifest_url(None, Channel::Stable),
///     "https://github.com/mail-peon/peon-burrow/releases/latest/download/latest.json"
/// );
/// assert_eq!(
///     manifest_url(Some("https://gh-proxy.com/https://github.com/"), Channel::Beta),
///     "https://gh-proxy.com/https://github.com/mail-peon/peon-burrow/releases/download/beta-latest/latest.json"
/// );
/// ```
#[must_use]
pub fn manifest_url(base_url: Option<&str>, channel: Channel) -> String {
    let base = match base_url {
        Some(base) => base.trim_end_matches('/'),
        None => GITHUB_ORIGIN,
    };
    let mut url = String::with_capacity(base.len() + channel.manifest_path().len());
    url.push_str(base);
    url.push_str(channel.manifest_path());
    url
}

/// 资产 URL：与清单**同一个目录**，只换文件名。
///
/// GitHub 上 `releases/latest/download/<name>` 取的就是最新 release 的资产，
/// 所以「清单在哪个目录，资产就在哪个目录」这条规则同时覆盖 stable 与 beta，
/// 也天然支持镜像（镜像只代理这一段路径）。
#[must_use]
pub fn asset_url(base_url: Option<&str>, channel: Channel, asset_name: &str) -> String {
    let mut url = manifest_url(base_url, channel);
    // `manifest_url` 一定以 MANIFEST_FILE 结尾（ASCII），截断是安全的
    let keep = url.len().saturating_sub(MANIFEST_FILE.len());
    url.truncate(keep);
    url.push_str(asset_name);
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_url_is_the_redirect_not_the_api() {
        let url = manifest_url(None, Channel::Stable);
        assert_eq!(
            url,
            "https://github.com/mail-peon/peon-burrow/releases/latest/download/latest.json"
        );
        assert!(!url.contains("/api/"), "绝不能走 GitHub API：{url}");
        assert!(!url.contains("/releases?"), "绝不能列 release：{url}");
    }

    #[test]
    fn beta_url_uses_the_rolling_tag() {
        assert_eq!(
            manifest_url(None, Channel::Beta),
            "https://github.com/mail-peon/peon-burrow/releases/download/beta-latest/latest.json"
        );
    }

    #[test]
    fn base_url_replaces_the_github_prefix() {
        let mirror = "https://gh-proxy.com/https://github.com";
        assert_eq!(
            manifest_url(Some(mirror), Channel::Stable),
            "https://gh-proxy.com/https://github.com/mail-peon/peon-burrow/releases/latest/download/latest.json"
        );
        // 结尾多余的 `/` 要能容忍
        assert_eq!(
            manifest_url(Some(&format!("{mirror}/")), Channel::Stable),
            manifest_url(Some(mirror), Channel::Stable)
        );
    }

    #[test]
    fn asset_url_sits_next_to_the_manifest() {
        let name = "peon-burrow-x86_64-pc-windows-msvc.exe";
        assert_eq!(
            asset_url(None, Channel::Stable, name),
            format!("https://github.com/mail-peon/peon-burrow/releases/latest/download/{name}")
        );
        assert_eq!(
            asset_url(Some("https://mirror.invalid"), Channel::Beta, name),
            format!(
                "https://mirror.invalid/mail-peon/peon-burrow/releases/download/beta-latest/{name}"
            )
        );
    }

    #[test]
    fn channel_round_trips_through_config_strings() {
        assert_eq!(Channel::parse("Beta"), Some(Channel::Beta));
        assert_eq!(Channel::parse(" stable "), Some(Channel::Stable));
        assert_eq!(Channel::parse("nightly"), None);
        assert_eq!(Channel::Stable.to_string(), "stable");
    }
}
