use crate::error::IoContext;
use crate::{
    ASSET_DOWNLOAD_SLACK, Applied, Asset, MAX_ASSET_BYTES, UpdateContext, UpdateError,
    UpdateStatus, check, cleanup_stale_backups, download, looks_like_archive, replace,
    replace_binary, sha256_hex, smoke, verify,
};
use std::fs;
use std::path::Path;

/// 应用更新：下载 → 四道闸 → 原子替换安装目录里的二进制。
///
/// ⚠️ **替换成功后，内存里跑的仍然是旧代码**。重启**不是本 crate 的事**：
/// 返回 [`Applied`] 之后，调用方必须立刻以「非正常退出」的方式结束自己，
/// 让服务管理器（SCM / systemd / launchd / 任务计划程序）把新版本拉起来
/// （`update-flow.md § 5、§ 6`）。
///
/// 任何一步失败都**不会动原二进制**（替换阶段失败会自动回滚），临时目录会被删掉。
///
/// # Errors
///
/// 除了 [`check`] 的全部错误，还有：
///
/// - [`UpdateError::SignatureVerifierMissing`]：要求验签但没有注入校验器（**fail closed**，
///   在下载之前就会拒绝）；
/// - [`UpdateError::UnsupportedAssetFormat`] / [`UpdateError::AssetTooLarge`]：格式或体积不接受；
/// - [`UpdateError::SizeMismatch`] / [`UpdateError::Sha256Mismatch`] / [`UpdateError::InvalidSha256`]：
///   第 1、2 道闸；
/// - [`UpdateError::SignatureInvalid`]：第 3 道闸；
/// - [`UpdateError::SmokeTestFailed`] / [`UpdateError::SmokeTestTimeout`] /
///   [`UpdateError::SmokeTestVersionMismatch`] / [`UpdateError::SmokeTestUnrunnable`]：第 4 道闸；
/// - [`UpdateError::MissingBinary`] / [`UpdateError::Replace`] / [`UpdateError::RollbackFailed`] /
///   [`UpdateError::Io`]：替换与文件操作；
/// - [`UpdateError::NoUpdateAvailable`]：清单已经不比当前版本新（不降级）。
///
/// ```no_run
/// # use peon_burrow_update::{apply, UpdateContext};
/// # async fn run(ctx: &UpdateContext) -> Result<(), Box<dyn std::error::Error>> {
/// let applied = apply(ctx).await?;
/// println!("{} → {}（{}）", applied.from, applied.to, applied.asset);
/// // ⚠️ 单进程里"重启"不是本 crate 的事：交给服务管理器
/// # Ok(())
/// # }
/// ```
pub async fn apply(ctx: &UpdateContext) -> Result<Applied, UpdateError> {
    // fail closed 前置检查：不要等资产都下下来了才说「没注入 verifier」
    if ctx.require_signature && ctx.verifier.is_none() {
        return Err(UpdateError::SignatureVerifierMissing);
    }

    // 重新 check（而不是让调用方把 UpdateStatus 传进来）：少一个「检查与下载之间清单变了」的缝
    let status = check(ctx).await?;
    let asset = match status.asset.clone() {
        Some(asset) if status.available => asset,
        _ => {
            return Err(UpdateError::NoUpdateAvailable {
                current: status.current,
                latest: status.latest,
            });
        }
    };

    // 不支持解压：名字像压缩包就直接拒（内容魔数在拿到字节后再看一眼）
    if asset.is_archive_name() {
        return Err(UpdateError::UnsupportedAssetFormat {
            name: asset.name.clone(),
            detail: "（文件名后缀是压缩包，本 crate 只支持裸二进制）".to_string(),
        });
    }
    if asset.size > MAX_ASSET_BYTES {
        return Err(UpdateError::AssetTooLarge {
            size: asset.size,
            max: MAX_ASSET_BYTES,
        });
    }

    let target = ctx.target_binary()?;
    if !target.is_file() {
        return Err(UpdateError::MissingBinary { path: target });
    }

    let staging = ctx.staging_root().join(status.latest.to_string());
    match apply_asset(ctx, &status, &asset, &staging).await {
        Ok(applied) => {
            // 收尾：顺手清掉遗留的旧二进制（正在运行的那个删不掉，正常），再删 staging
            let removed = cleanup_stale_backups(&ctx.install_dir);
            if removed > 0 {
                tracing::debug!(event = "update.cleanup", removed, "清掉遗留的旧二进制");
            }
            discard_staging(&staging);
            Ok(applied)
        }
        Err(error) => {
            // 半成品即删：下一次重来（update-flow.md § 4 ①）
            discard_staging(&staging);
            Err(error)
        }
    }
}

/// 下载 + 四道闸 + 替换（[`apply`] 的主体，拆出来是为了统一清理 staging）。
async fn apply_asset(
    ctx: &UpdateContext,
    status: &UpdateStatus,
    asset: &Asset,
    staging: &Path,
) -> Result<Applied, UpdateError> {
    let url = ctx.asset_url(&asset.name);
    tracing::info!(event = "update.download", %url, size = asset.size, "下载资产");
    // 上限 = 清单声明的 size + 余量：多的那点交给下面第 1 道闸去报「大小不符」
    let cap = asset.size.saturating_add(ASSET_DOWNLOAD_SLACK);
    let bytes = download::fetch(ctx, &url, cap).await?;

    // ---- 闸 1：size ----
    let actual_size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if actual_size != asset.size {
        return Err(UpdateError::SizeMismatch {
            expected: asset.size,
            actual: actual_size,
        });
    }

    // ---- 闸 2：sha256 ----
    let expected = verify::normalize_sha256(&asset.sha256)?;
    let actual = sha256_hex(&bytes);
    if actual != expected {
        return Err(UpdateError::Sha256Mismatch { expected, actual });
    }
    tracing::debug!(event = "update.verify", asset = %asset.name, sha256 = %actual, "校验和匹配");

    // ---- 格式预检：本 crate 不解压 ----
    if looks_like_archive(&bytes) {
        return Err(UpdateError::UnsupportedAssetFormat {
            name: asset.name.clone(),
            detail: "（内容魔数像压缩包，本 crate 只支持裸二进制）".to_string(),
        });
    }

    // ---- 闸 3：签名（require_signature 时，缺 verifier 或签名 = 拒绝）----
    let mut signature_verified = false;
    if ctx.require_signature {
        // 前面已经 fail closed 过一次，这里是双保险（未来若有人改动前置检查也不会漏）
        let verifier = ctx
            .verifier
            .as_ref()
            .ok_or(UpdateError::SignatureVerifierMissing)?;
        tracing::debug!(
            event = "update.verify",
            asset = %asset.name,
            pubkeys = ctx.pubkeys.len(),
            has_signature = asset.signature.is_some(),
            "调用注入的签名校验器"
        );
        verifier
            .verify(asset, &bytes)
            .map_err(|reason| UpdateError::SignatureInvalid { reason })?;
        signature_verified = true;
    }

    // 落盘到 staging（校验通过之后才写，写坏也不会污染安装目录）
    fs::create_dir_all(staging).with_path(staging)?;
    let staged = staging.join(&asset.name);
    fs::write(&staged, &bytes).with_path(&staged)?;
    replace::ensure_executable(&staged)?;

    // ---- 闸 4：冒烟测试 ----
    if ctx.skip_smoke_test {
        tracing::warn!(
            event = "update.smoke",
            asset = %asset.name,
            "调用方显式跳过了冒烟测试（skip_smoke_test = true），风险自负"
        );
    } else {
        smoke::smoke_test(&staged, &status.latest).await?;
    }

    // ---- 第 5 道工序：原子替换 ----
    let target = ctx.target_binary()?;
    let backup = replace_binary(&target, &staged)?;
    tracing::info!(
        event = "update.apply",
        from = %status.current,
        to = %status.latest,
        asset = %asset.name,
        verified = signature_verified,
        target = %target.display(),
        backup = %backup.display(),
        "替换完成：运行中的仍是旧代码，必须立刻退出让服务管理器把新版本拉起来"
    );

    Ok(Applied {
        from: status.current.clone(),
        to: status.latest.clone(),
        asset: asset.name.clone(),
    })
}

/// 删掉 staging 目录（尽力而为：删不掉只记 debug，不改变调用方的结果）。
fn discard_staging(staging: &Path) {
    if !staging.exists() {
        return;
    }
    match fs::remove_dir_all(staging) {
        Ok(()) => {
            tracing::debug!(event = "update.cleanup", path = %staging.display(), "清掉 staging")
        }
        Err(error) => {
            tracing::debug!(event = "update.cleanup", path = %staging.display(), %error, "staging 没删掉")
        }
    }
}
