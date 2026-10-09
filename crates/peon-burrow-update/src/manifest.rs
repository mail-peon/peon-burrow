//! `latest.json` 的形状 —— **这是契约**（`ai-docs/design/update-flow.md § 2.2`）。
//!
//! ```json
//! {
//!   "schema": 1,
//!   "version": "0.2.0",
//!   "channel": "stable",
//!   "releasedAt": "2026-10-09T04:00:00Z",
//!   "notesUrl": "https://github.com/mail-peon/peon-burrow/releases/tag/v0.2.0",
//!   "assets": [
//!     { "target": "x86_64-pc-windows-msvc", "name": "burrow-x86_64-pc-windows-msvc.exe",
//!       "size": 5242880, "sha256": "…64 hex…", "signature": "untrusted comment: minisign…" }
//!   ]
//! }
//! ```
//!
//! 字段名是 **camelCase**（`releasedAt` / `notesUrl`），与清单契约一致；
//! `signature` 可以缺省（缺省 = `None`，`require_signature = true` 时会因此被拒）。

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::error::UpdateError;

/// 本 crate 支持的清单 `schema`。改了清单格式就要 +1，并且**两边都要能读**。
pub const MANIFEST_SCHEMA: u32 = 1;

/// 一份发布清单。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    /// 清单元数据版本；不认识就整份拒绝（[`Manifest::parse`]）。
    pub schema: u32,
    /// 这份清单描述的版本（语义化版本）。
    pub version: Version,
    /// 清单所属渠道（`stable` / `beta`）——与请求的渠道不符时整份作废。
    pub channel: String,
    /// 发布时间（RFC 3339，CI 生成；本 crate 只展示不解析）。
    pub released_at: String,
    /// release notes 的地址（给 GUI 跳转用）。
    pub notes_url: String,
    /// 各平台资产。
    pub assets: Vec<Asset>,
}

impl Manifest {
    /// 解析并校验一份清单。
    ///
    /// `url` 只用于错误信息（调用方已经把字节下下来了）。
    ///
    /// # Errors
    ///
    /// - [`UpdateError::Manifest`]：不是合法 JSON / 缺字段 / 版本号不是语义化版本；
    /// - [`UpdateError::UnsupportedSchema`]：`schema` 不是 [`MANIFEST_SCHEMA`]。
    pub fn parse(url: &str, body: &[u8]) -> Result<Self, UpdateError> {
        let manifest: Manifest =
            serde_json::from_slice(body).map_err(|source| UpdateError::Manifest {
                url: url.to_string(),
                source,
            })?;
        if manifest.schema != MANIFEST_SCHEMA {
            return Err(UpdateError::UnsupportedSchema {
                found: manifest.schema,
                supported: MANIFEST_SCHEMA,
            });
        }
        Ok(manifest)
    }

    /// 按 host triple **精确匹配**资产。
    ///
    /// 匹配不到返回 `None`：调用方据此报「本平台暂无更新」，**不猜、不下载**
    /// （`ai-docs/design/update-flow.md § 8` 第 4 条）。
    #[must_use]
    pub fn asset_for_target(&self, target: &str) -> Option<&Asset> {
        self.assets.iter().find(|asset| asset.target == target)
    }
}

/// 一个平台资产。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    /// host triple（与 [`crate::host_triple`] 的返回值精确比较）。
    pub target: String,
    /// 文件名，同时是「资产 URL 的最后一段」。
    pub name: String,
    /// 字节数：下载前展示、下载后校验（防被塞巨型文件）。
    pub size: u64,
    /// 小写十六进制 sha256（CI 从**它自己构建的资产**算出）。
    ///
    /// ⚠️ **校验和不是签名**：清单本身也在 GitHub / 镜像站上，能改资产的人多半也能改清单。
    pub sha256: String,
    /// minisign / zipsign 签名（可选，但 `require_signature = true` 时**缺 = 拒绝**）。
    pub signature: Option<String>,
}

impl Asset {
    /// 文件名看起来是压缩包吗？
    ///
    /// 本 crate **不解压**（`apply` 只支持裸二进制），所以在下载之前就能拒掉这一类。
    #[must_use]
    pub fn is_archive_name(&self) -> bool {
        let name = self.name.to_ascii_lowercase();
        const ARCHIVE_SUFFIXES: [&str; 8] = [
            ".zip", ".tar", ".tar.gz", ".tgz", ".tar.xz", ".txz", ".gz", ".7z",
        ];
        ARCHIVE_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
    }
}

/// 内容魔数看起来是压缩包吗？
///
/// 名字可以骗人（`burrow.exe` 里塞一个 zip），所以下载后再看一眼：
/// 这一类资产本 crate 处理不了，宁可**明确报错**也不要让一个压缩包去过冒烟测试。
#[must_use]
pub fn looks_like_archive(bytes: &[u8]) -> bool {
    if bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06") {
        return true; // zip（含空 zip）
    }
    if bytes.starts_with(&[0x1f, 0x8b]) {
        return true; // gzip（=.tar.gz / .gz）
    }
    if bytes.starts_with(b"7z\xbc\xaf\x27\x1c") {
        return true; // 7z
    }
    if bytes.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0x00]) {
        return true; // xz
    }
    if bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        return true; // zstd
    }
    if bytes.starts_with(&[0x42, 0x5a, 0x68]) {
        return true; // bzip2
    }
    // tar：魔数在偏移 257
    bytes.len() > 262 && &bytes[257..262] == b"ustar"
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = r#"{
      "schema": 1,
      "version": "0.2.0",
      "channel": "stable",
      "releasedAt": "2026-10-09T04:00:00Z",
      "notesUrl": "https://example.invalid/notes",
      "assets": [
        { "target": "x86_64-pc-windows-msvc", "name": "burrow.exe", "size": 3,
          "sha256": "aaaa", "signature": "untrusted comment: minisign signature" },
        { "target": "aarch64-apple-darwin", "name": "burrow", "size": 4, "sha256": "bbbb" }
      ]
    }"#;

    #[test]
    fn parses_the_camel_case_contract() {
        let manifest = Manifest::parse("https://example.invalid/latest.json", BODY.as_bytes())
            .expect("清单应当解析成功");
        assert_eq!(manifest.schema, MANIFEST_SCHEMA);
        assert_eq!(manifest.version, Version::new(0, 2, 0));
        assert_eq!(manifest.channel, "stable");
        assert_eq!(manifest.released_at, "2026-10-09T04:00:00Z");
        assert_eq!(manifest.notes_url, "https://example.invalid/notes");
        assert_eq!(manifest.assets.len(), 2);
        // 缺 signature 的资产解析成 None（不是解析失败）
        assert_eq!(manifest.assets[1].signature, None);
        assert!(manifest.assets[0].signature.is_some());
    }

    #[test]
    fn rejects_unknown_schema() {
        let body = BODY.replace("\"schema\": 1", "\"schema\": 99");
        let err = Manifest::parse("u", body.as_bytes()).expect_err("schema 不认识就该拒");
        assert!(matches!(
            err,
            UpdateError::UnsupportedSchema {
                found: 99,
                supported: 1
            }
        ));
    }

    #[test]
    fn rejects_broken_json_and_bad_versions() {
        assert!(matches!(
            Manifest::parse("u", b"{").unwrap_err(),
            UpdateError::Manifest { .. }
        ));
        let body = BODY.replace("\"0.2.0\"", "\"not-a-version\"");
        assert!(matches!(
            Manifest::parse("u", body.as_bytes()).unwrap_err(),
            UpdateError::Manifest { .. }
        ));
    }

    #[test]
    fn asset_selection_is_exact_and_never_guesses() {
        let manifest = Manifest::parse("u", BODY.as_bytes()).unwrap();
        assert_eq!(
            manifest
                .asset_for_target("x86_64-pc-windows-msvc")
                .map(|a| a.name.as_str()),
            Some("burrow.exe")
        );
        assert!(manifest.asset_for_target("x86_64-pc-windows-gnu").is_none());
        assert!(
            manifest
                .asset_for_target("X86_64-PC-WINDOWS-MSVC")
                .is_none()
        );
    }

    #[test]
    fn archive_names_are_recognised() {
        let mut asset = Asset {
            target: "t".into(),
            name: "burrow.exe".into(),
            size: 0,
            sha256: String::new(),
            signature: None,
        };
        assert!(!asset.is_archive_name());
        asset.name = "burrow-X86_64-PC-WINDOWS-MSVC.ZIP".into();
        assert!(asset.is_archive_name());
        asset.name = "burrow.tar.gz".into();
        assert!(asset.is_archive_name());
    }

    #[test]
    fn archive_magic_is_recognised() {
        assert!(looks_like_archive(b"PK\x03\x04rest of zip"));
        assert!(looks_like_archive(&[0x1f, 0x8b, 0x08, 0x00]));
        assert!(looks_like_archive(&[0x28, 0xb5, 0x2f, 0xfd]));
        assert!(!looks_like_archive(b"MZ\x90\x00"));
        assert!(!looks_like_archive(b"\x7fELF"));
        assert!(!looks_like_archive(b""));
        // tar 魔数在 257
        let mut tar = vec![0u8; 300];
        tar[257..262].copy_from_slice(b"ustar");
        assert!(looks_like_archive(&tar));
    }
}
