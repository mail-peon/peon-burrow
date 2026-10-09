//! 闸 2 与闸 3：`sha256` 与签名。
//!
//! ## 校验和 **不是** 签名
//!
//! | | 谁算的 | 放在哪 | 防什么 |
//! | --- | --- | --- | --- |
//! | `size` / `sha256` | CI，从**它自己构建的资产**算出 | 同一份清单里 | 资产被替换 / 被截断（**含镜像站作恶**） |
//! | `signature` | CI 的私钥（只在 secrets 里） | 清单里的 `signature` 字段 | 「同一个 GitHub 账号发的假货」/ 账号被拿下 |
//!
//! 清单本身也在 GitHub / 镜像站上：**能改资产的人多半也能改清单**，所以 sha256 挡不住
//! 「有 GitHub 账号权限的攻击者」。只有签名能。这也是 `require_signature = true` 时
//! 缺签名 / 缺 verifier 一律 **fail closed** 的原因（`adr-0005-self-update.md § 3`）。
//!
//! ## 本 crate 不内置签名算法
//!
//! 没有 zipsign/minisign 依赖（见 crate 文档「刻意留的缺口」），所以签名只留一个**注入点**：
//! [`SignatureVerifier`]。调用方把自己的实现（minisign / zipsign / 自签）塞进
//! [`crate::UpdateContext::verifier`]；本 crate 只负责「按顺序调用 + 失败即拒绝 + 写日志」。

use sha2::{Digest, Sha256};

use crate::manifest::Asset;

/// 签名校验的注入点。
///
/// 实现方需要自己决定用什么算法、用哪些公钥（公钥列表在
/// [`crate::UpdateContext::pubkeys`] 里，由调用方在构造实现时喂进去）。
///
/// # 约定
///
/// - 返回 `Ok(())` = 签名可信 → 放行；
/// - 返回 `Err(原因)` = 拒绝，原因会变成 [`crate::UpdateError::SignatureInvalid`] 里的文案；
/// - **不允许「没签名就放行」**：`require_signature = true` 时，`asset.signature` 为 `None`
///   必须返回 `Err`（这属于「缺签名 = 拒绝」，见 `adr-0005 § 3`）。
///
/// ```no_run
/// use peon_burrow_update::{Asset, SignatureVerifier};
///
/// /// 极简示例：把清单里的签名当成「必须存在且等于公钥」来用（**不是**真签名算法）。
/// #[derive(Debug)]
/// struct SignatureMustExist;
///
/// impl SignatureVerifier for SignatureMustExist {
///     fn verify(&self, asset: &Asset, _bytes: &[u8]) -> Result<(), String> {
///         if asset.signature.as_deref().is_some_and(|s| !s.is_empty()) {
///             Ok(())
///         } else {
///             Err("清单里没有签名".to_string())
///         }
///     }
/// }
/// ```
pub trait SignatureVerifier: Send + Sync {
    /// 校验 `bytes` 是否是 `asset` 声称的那份、且由我们签名的资产。
    ///
    /// # Errors
    ///
    /// 任何不可信的情况都返回 `Err`（文案会进日志）。
    fn verify(&self, asset: &Asset, bytes: &[u8]) -> Result<(), String>;
}

/// 算出 `bytes` 的小写十六进制 sha256。
///
/// ```no_run
/// # use peon_burrow_update::sha256_hex;
/// assert_eq!(
///     sha256_hex(b""),
///     "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
/// );
/// ```
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize().as_slice())
}

/// 把清单里的 `sha256` 规整成小写十六进制，并校验它真的是 64 位十六进制。
///
/// # Errors
///
/// [`crate::UpdateError::InvalidSha256`]：长度不对或含非十六进制字符 → 清单本身不可信，
/// 不进第 2 道闸的比较。
pub(crate) fn normalize_sha256(value: &str) -> Result<String, crate::UpdateError> {
    let normalized = value.trim().to_ascii_lowercase();
    let ok = normalized.len() == 64 && normalized.bytes().all(|b| b.is_ascii_hexdigit());
    if ok {
        Ok(normalized)
    } else {
        Err(crate::UpdateError::InvalidSha256 {
            value: value.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_the_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha256_normalisation_accepts_upper_case_and_rejects_garbage() {
        assert_eq!(normalize_sha256(&"A".repeat(64)).unwrap(), "a".repeat(64));
        assert!(normalize_sha256(&format!(" {}", "a".repeat(64))).is_ok());
        assert!(normalize_sha256("abcd").is_err());
        assert!(normalize_sha256(&"z".repeat(64)).is_err());
    }
}
