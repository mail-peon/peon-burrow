//! 自更新接线：清单检查、**节流戳**、控制面钩子。
//!
//! 分三层（`ai-docs/design/update-flow.md`）：
//! - `peon-burrow-update` 只管「拿清单 → 四道闸 → 换文件」，不认识配置；
//! - 这里把配置翻成它要的 `UpdateContext`（这是**唯一**做映射的地方）；
//! - 何时检查（节流）与「更新完怎么重启」是产品层的事。

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use peon_burrow_ipc_types::{HandlerFuture, IpcError, IpcErrorCode};
use peon_burrow_update::{Channel, UpdateContext, UpdateStatus};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::config::{Config, UpdateChannel};
use crate::control::{Lifecycle, UpdateAction, UpdateHook};
use crate::paths::Paths;

/// 节流戳：上次检查的时间与当时看到的最新版本。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateStamp {
    /// 上次检查的 Unix 秒。
    pub last_check_unix: u64,
    /// 当时看到的最新版本（用来在启动日志里提醒）。
    pub latest: Option<String>,
}

/// 读戳（读不动就当没有过 —— 宁可多检查一次，也不要因为一个坏文件永远不检查）。
pub fn read_stamp(paths: &Paths) -> UpdateStamp {
    std::fs::read(paths.update_stamp())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// 写戳（失败只记日志：它只是个节流提示）。
pub fn write_stamp(paths: &Paths, stamp: &UpdateStamp) {
    match serde_json::to_vec(stamp) {
        Ok(bytes) => {
            if let Err(error) = std::fs::write(paths.update_stamp(), bytes) {
                warn!(%error, "写更新节流戳失败（不影响运行）");
            }
        }
        Err(error) => warn!(%error, "序列化更新节流戳失败"),
    }
}

/// 现在该不该检查。
pub fn due_for_check(paths: &Paths, config: &Config, force: bool) -> bool {
    if force {
        return true;
    }
    let stamp = read_stamp(paths);
    if stamp.last_check_unix == 0 {
        return true;
    }
    let interval = Duration::from_secs(config.update.check_interval_hours.saturating_mul(3600));
    let elapsed = Duration::from_secs(now_unix().saturating_sub(stamp.last_check_unix));
    elapsed >= interval
}

/// 配置 → `UpdateContext`（`None` = 关掉自更新，或者定位不了安装目录）。
pub fn context(config: &Config, paths: &Paths) -> Option<UpdateContext> {
    if !config.update.enabled {
        return None;
    }

    let current = semver::Version::parse(crate::VERSION).ok()?;
    let install_dir = std::env::current_exe()
        .ok()?
        .parent()
        .map(std::path::Path::to_path_buf)?;

    let mut context = UpdateContext::new(current, install_dir);
    context.channel = match config.update.channel {
        UpdateChannel::Stable => Channel::Stable,
        UpdateChannel::Beta => Channel::Beta,
    };
    context.base_url = config.update.base_url.clone();
    // 校验和从不跳过（四道闸里的第一、二道）；签名要等有验签器才打开
    debug_assert!(config.update.verify_checksum, "校验和必须始终开启");
    let _ = paths;
    Some(context)
}

/// 启动时检查一次（不阻塞启动，也不因为网络问题影响中继）。
pub async fn startup_check(context: UpdateContext, paths: Paths, config: Config) {
    let stamp = read_stamp(&paths);
    if let Some(latest) = &stamp.latest {
        info!(latest = %latest, current = %context.current_version, "上次检查时发现有新版本，运行 `burrow update --apply` 更新");
    }

    if !due_for_check(&paths, &config, false) {
        return;
    }

    match peon_burrow_update::check(&context).await {
        Ok(status) => {
            record(&paths, &status);
            if status.available {
                info!(
                    latest = %status.latest,
                    current = %status.current,
                    "有新版本可用：`burrow update --apply` 可以直接替换（替换后需要重启服务）"
                );
            } else {
                info!(current = %status.current, "已是最新版本");
            }
        }
        // 检查更新失败**绝不能**影响中继：网络、镜像站、GitHub 限流都会让这里失败
        Err(error) => warn!(%error, "检查更新失败（不影响运行）"),
    }
}

/// 检查结果 → 状态。
pub fn describe(status: &UpdateStatus) -> Value {
    json!({
        "available": status.available,
        "current": status.current.to_string(),
        "latest": status.latest.to_string(),
        "asset": status.asset.as_ref().map(|asset| json!({
            "target": asset.target,
            "name": asset.name,
            "size": asset.size,
        })),
    })
}

/// 控制面钩子：`updateCheck` / `updateApply`。
///
/// 应用成功后**不自己重启**：通过 `lifecycle` 让进程以 `RestartRequested = 5` 结束，
/// 由服务管理器拉起新二进制 —— 这是升级「正在运行的程序」唯一可靠的做法。
pub fn hook(context: UpdateContext, lifecycle: watch::Sender<Option<Lifecycle>>) -> UpdateHook {
    let context = Arc::new(context);
    Arc::new(move |action| {
        let context = Arc::clone(&context);
        let lifecycle = lifecycle.clone();
        Box::pin(async move {
            match action {
                UpdateAction::Check { .. } => peon_burrow_update::check(&context)
                    .await
                    .map(|status| describe(&status))
                    .map_err(map_error),
                UpdateAction::Apply => {
                    let applied = peon_burrow_update::apply(&context)
                        .await
                        .map_err(map_error)?;
                    info!(from = %applied.from, to = %applied.to, "更新已就位，准备重启");
                    // 让 run() 以 RestartRequested 退出
                    lifecycle.send_replace(Some(Lifecycle::Restart));
                    Ok(json!({
                        "from": applied.from.to_string(),
                        "to": applied.to.to_string(),
                        "asset": applied.asset,
                        "restarting": true,
                    }))
                }
            }
        })
    })
}

/// 控制面 hook 的类型别名（让 `run()` 不必 import 这些细节）。
pub type Hook = UpdateHook;

/// 错误码映射：**平台没有资产**是可预期的，不该报成内部错误。
fn map_error(error: peon_burrow_update::UpdateError) -> IpcError {
    let code = if error.is_platform_unsupported() {
        IpcErrorCode::Busy
    } else {
        IpcErrorCode::Internal
    };
    IpcError::new(code, error.to_string())
}

/// 记录一次检查结果。
pub fn record(paths: &Paths, status: &UpdateStatus) {
    write_stamp(
        paths,
        &UpdateStamp {
            last_check_unix: now_unix(),
            latest: Some(status.latest.to_string()),
        },
    );
}

/// 当前 Unix 秒。
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

/// 让 `HandlerFuture` 在类型上被用到（钩子的返回类型）。
#[allow(dead_code)]
fn _future_marker(future: HandlerFuture) -> HandlerFuture {
    future
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CliOverrides, Config, EnvSource};
    use peon_burrow_update::FetchFuture;

    fn fixture(extra: &str) -> (tempfile::TempDir, Paths, Config) {
        let directory = tempfile::tempdir().expect("tempdir");
        let paths = Paths::for_test(directory.path().join("data"));
        paths.ensure_dirs().expect("dirs");
        let config = Config::merge(
            crate::config::test_support::file_config(extra),
            &EnvSource::default(),
            &CliOverrides::default(),
            Vec::new(),
        )
        .expect("merge");
        (directory, paths, config)
    }

    /// 打桩抓取器：任何 URL 都回同一段内容，**不碰网络**。
    fn canned(body: String) -> peon_burrow_update::Fetcher {
        Arc::new(move |_url: &str, _max: usize| -> FetchFuture {
            let body = body.clone();
            Box::pin(async move { Ok(body.into_bytes()) })
        })
    }

    fn manifest_for_this_host(version: &str) -> String {
        format!(
            r#"{{"schema":1,"version":"{version}","channel":"stable","releasedAt":"2026-10-09T00:00:00Z","notesUrl":"https://example.invalid","assets":[{{"target":"{}","name":"burrow-{}","size":3,"sha256":"0000000000000000000000000000000000000000000000000000000000000000"}}]}}"#,
            peon_burrow_update::host_triple().expect("测试机应当有目标三元组"),
            peon_burrow_update::host_triple().expect("测试机应当有目标三元组")
        )
    }

    #[test]
    fn the_context_maps_channel_and_mirror() {
        let (_directory, paths, config) =
            fixture("[update]\nchannel = \"beta\"\nbase_url = \"https://mirror.example\"\n");
        let context = context(&config, &paths).expect("enabled by default");
        assert_eq!(context.channel, Channel::Beta);
        assert_eq!(context.base_url.as_deref(), Some("https://mirror.example"));
        assert_eq!(context.current_version.to_string(), crate::VERSION);
    }

    #[test]
    fn a_disabled_update_section_yields_no_context() {
        let (_directory, paths, config) = fixture("[update]\nenabled = false\n");
        assert!(context(&config, &paths).is_none());
    }

    #[test]
    fn the_throttle_uses_the_configured_interval() {
        let (_directory, paths, config) = fixture("[update]\ncheck_interval_hours = 24\n");
        assert!(due_for_check(&paths, &config, false), "没有戳时应当检查");

        write_stamp(
            &paths,
            &UpdateStamp {
                last_check_unix: now_unix(),
                latest: Some("9.9.9".to_owned()),
            },
        );
        assert!(!due_for_check(&paths, &config, false), "刚检查过就不该再查");
        assert!(due_for_check(&paths, &config, true), "force 必须绕过节流");

        // 戳里记的是 25 小时前
        write_stamp(
            &paths,
            &UpdateStamp {
                last_check_unix: now_unix() - 25 * 3600,
                latest: None,
            },
        );
        assert!(due_for_check(&paths, &config, false), "过了间隔就该再查");
    }

    #[test]
    fn a_broken_stamp_is_treated_as_never_checked() {
        let (_directory, paths, config) = fixture("");
        std::fs::write(paths.update_stamp(), b"not json").expect("write");
        assert!(due_for_check(&paths, &config, false));
        assert!(read_stamp(&paths).latest.is_none());
    }

    #[tokio::test]
    async fn the_hook_reports_an_available_update_without_touching_the_network() {
        let (_directory, paths, config) = fixture("");
        let mut context = context(&config, &paths).expect("context");
        context.fetcher = Some(canned(manifest_for_this_host("99.0.0")));

        let (lifecycle, _rx) = watch::channel(None::<Lifecycle>);
        let hook = hook(context, lifecycle);
        let value = hook(UpdateAction::Check { force: true })
            .await
            .expect("check");

        assert_eq!(value["available"], json!(true));
        assert_eq!(value["latest"], json!("99.0.0"));
        assert_eq!(
            value["asset"]["target"],
            json!(peon_burrow_update::host_triple())
        );
    }

    #[tokio::test]
    async fn the_hook_says_nothing_to_do_on_the_same_version() {
        let (_directory, paths, config) = fixture("");
        let mut context = context(&config, &paths).expect("context");
        context.fetcher = Some(canned(manifest_for_this_host(crate::VERSION)));

        let (lifecycle, _rx) = watch::channel(None::<Lifecycle>);
        let hook = hook(context, lifecycle);
        let value = hook(UpdateAction::Check { force: true })
            .await
            .expect("check");
        assert_eq!(value["available"], json!(false));
    }

    #[tokio::test]
    async fn a_platform_without_an_asset_is_reported_as_busy_not_internal() {
        let (_directory, paths, config) = fixture("");
        let mut context = context(&config, &paths).expect("context");
        // 清单里没有本平台的资产 → update crate 返回「本平台暂无更新」
        context.fetcher = Some(canned(
            r#"{"schema":1,"version":"99.0.0","channel":"stable","releasedAt":"2026-10-09T00:00:00Z","notesUrl":"https://example.invalid","assets":[{"target":"sparc-unknown-none","name":"x","size":1,"sha256":"0000000000000000000000000000000000000000000000000000000000000000"}]}"#
                .to_owned(),
        ));

        let (lifecycle, _rx) = watch::channel(None::<Lifecycle>);
        let hook = hook(context, lifecycle);
        let error = hook(UpdateAction::Check { force: true })
            .await
            .expect_err("no asset");
        assert_eq!(
            error.code,
            IpcErrorCode::Busy,
            "可预期的平台缺失不该报内部错误"
        );
    }

    #[tokio::test]
    async fn a_garbage_manifest_surfaces_as_an_error() {
        let (_directory, paths, config) = fixture("");
        let mut context = context(&config, &paths).expect("context");
        // 打桩返回一段垃圾：无论是解析失败还是下载失败，产品层都必须把它翻成一条错误响应
        context.fetcher = Some(canned("这不是 JSON".to_owned()));

        let (lifecycle, _rx) = watch::channel(None::<Lifecycle>);
        let hook = hook(context, lifecycle);
        let error = hook(UpdateAction::Check { force: true })
            .await
            .expect_err("should fail");
        assert_eq!(error.code, IpcErrorCode::Internal);
        assert!(!error.message.is_empty(), "错误必须有可读文案");
    }

    #[tokio::test]
    async fn a_successful_check_writes_the_stamp() {
        let (_directory, paths, config) = fixture("");
        let mut context = context(&config, &paths).expect("context");
        context.fetcher = Some(canned(manifest_for_this_host("99.0.0")));

        let status = peon_burrow_update::check(&context).await.expect("check");
        record(&paths, &status);

        let stamp = read_stamp(&paths);
        assert_eq!(stamp.latest.as_deref(), Some("99.0.0"));
        assert!(stamp.last_check_unix > 0);
    }

    /// CI 里 `scripts/make-manifest.sh` 生成的清单必须能被自更新解析。
    ///
    /// 这是**跨仓库的契约**：脚本一侧改了字段名（或把资产指向压缩包），线上老版本只会看到
    /// 「更新检查失败」，而那种 bug 在本地测试里是完全看不见的。
    #[test]
    fn the_ci_manifest_shape_parses() {
        let manifest = format!(
            r#"{{
  "schema": 1,
  "version": "0.2.0",
  "channel": "stable",
  "releasedAt": "2026-10-09T00:00:00Z",
  "notesUrl": "https://github.com/mail-peon/peon-burrow/releases/tag/v0.2.0",
  "assets": [
    {{"target":"{}","name":"burrow-{}","size":200,"sha256":"{}"}}
  ]
}}"#,
            peon_burrow_update::host_triple().expect("本机应当有 triple"),
            peon_burrow_update::host_triple().expect("本机应当有 triple"),
            "ab".repeat(32)
        );

        let parsed = peon_burrow_update::Manifest::parse(
            "https://example.invalid/latest.json",
            manifest.as_bytes(),
        )
        .expect("CI 清单必须能解析");
        assert_eq!(parsed.version.to_string(), "0.2.0");
        let asset = parsed
            .asset_for_target(&peon_burrow_update::host_triple().expect("triple"))
            .expect("本平台应当有资产");
        // ⚠️ 自更新**不解压**：指向 .zip/.tar.gz 会被它以「不支持的资产格式」拒掉
        assert!(
            !asset.is_archive_name(),
            "清单里的资产必须是裸二进制（自更新不解压），而不是 {}",
            asset.name
        );
    }

    #[test]
    fn the_clock_is_plausible() {
        // 2023-11-14 之后（写这个测试的时间）；只用来抓「时间戳算成了 0」这类错误
        assert!(now_unix() > 1_700_000_000);
    }
}
