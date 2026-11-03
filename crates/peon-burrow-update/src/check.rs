use crate::{
    MAX_MANIFEST_BYTES, Manifest, UpdateContext, UpdateError, UpdateStatus, download, host_triple,
};

/// 检查有没有更新（只读：**不碰任何文件**）。
///
/// 流程：拼固定清单 URL → 下载 → 解析校验 `schema` → 校验渠道 → 按 host triple 挑资产
/// → 与当前版本比大小。
///
/// # Errors
///
/// - [`UpdateError::UnknownHost`] / [`UpdateError::NoAssetForTarget`]：**本平台暂无更新**，
///   不是网络故障（用 [`UpdateError::is_platform_unsupported`] 区分，别拿去退避重试）；
/// - [`UpdateError::ChannelMismatch`] / [`UpdateError::UnsupportedSchema`] /
///   [`UpdateError::Manifest`]：清单不可信 / 不匹配；
/// - [`UpdateError::Download`] / [`UpdateError::Timeout`] / [`UpdateError::Tls`] /
///   [`UpdateError::HttpStatus`] / [`UpdateError::HttpProtocol`] / [`UpdateError::BadUrl`] /
///   [`UpdateError::ProxyUnsupported`] / [`UpdateError::BodyTooLarge`]：网络问题，
///   **值得退避重试**（其中 `ProxyUnsupported` / `BadUrl` 是配置问题，重试也没用）。
///
/// 「没有更新」是 `Ok(UpdateStatus { available: false, .. })`：
/// 版本相同、或本地版本比清单还新（**不降级**，`update-flow.md § 8` 第 14 条）。
///
/// ```no_run
/// # use peon_burrow_update::{check, Channel, UpdateContext};
/// # use semver::Version;
/// # use std::path::PathBuf;
/// # async fn run(ctx: &UpdateContext) -> Result<(), Box<dyn std::error::Error>> {
/// let status = check(ctx).await?;
/// if !status.available {
///     println!("已是最新：{}", status.current);
/// }
/// # Ok(())
/// # }
/// ```
pub async fn check(ctx: &UpdateContext) -> Result<UpdateStatus, UpdateError> {
    let triple = host_triple().ok_or_else(|| UpdateError::UnknownHost {
        arch: std::env::consts::ARCH.to_string(),
        os: std::env::consts::OS.to_string(),
    })?;

    let url = ctx.manifest_url();
    tracing::debug!(event = "update.check", %url, "拉取清单");
    let body = download::fetch(ctx, &url, MAX_MANIFEST_BYTES).await?;
    let manifest = Manifest::parse(&url, &body)?;

    // 渠道不符 → 整份清单作废（防止 beta 清单被 stable 用上）
    let requested = ctx.channel.as_str();
    if manifest.channel != requested {
        return Err(UpdateError::ChannelMismatch {
            manifest: manifest.channel.clone(),
            requested,
        });
    }

    let asset = manifest.asset_for_target(&triple).cloned();
    let latest = manifest.version.clone();

    // 只升不降：版本相同或更旧都算「没有更新」
    if latest <= ctx.current_version {
        tracing::info!(
            event = "update.check",
            available = false,
            current = %ctx.current_version,
            latest = %latest,
            "没有更新的版本（不降级）"
        );
        return Ok(UpdateStatus {
            available: false,
            current: ctx.current_version.clone(),
            latest,
            asset: None,
        });
    }

    // 版本更新了但清单里没有本平台的资产 → 报「本平台暂无更新」，不猜、不下载
    let asset = asset.ok_or(UpdateError::NoAssetForTarget { triple })?;

    tracing::info!(
        event = "update.check",
        available = true,
        current = %ctx.current_version,
        latest = %latest,
        asset = %asset.name,
        size = asset.size,
        "发现新版本"
    );

    Ok(UpdateStatus {
        available: true,
        current: ctx.current_version.clone(),
        latest,
        asset: Some(asset),
    })
}
