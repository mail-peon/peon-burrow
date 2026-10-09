//! peon-burrow 的**自更新**：静态清单 → 四道闸 → 原子替换。
//!
//! 一句话原则：**清单固定、下载必校验、替换可失败、重启交给系统**
//! （`ai-docs/design/update-flow.md`）。
//!
//! 本 crate **与中继无关**：给它一个 [`UpdateContext`]（版本 / 安装目录 / 渠道 / 镜像 / 代理 / 签名要求），
//! 它就能给**任何**本地工具做自更新 —— 自己不去读配置文件、不读环境变量、不问服务管理器
//! （依赖倒置，`ai-docs/modules.md § 6`），所以测试可以用假 context 全程离线跑。
//!
//! # 清单 URL（固定，不碰 GitHub API）
//!
//! | 渠道 | URL |
//! | --- | --- |
//! | stable | `https://github.com/mail-peon/peon-burrow/releases/latest/download/latest.json` |
//! | beta | `https://github.com/mail-peon/peon-burrow/releases/download/beta-latest/latest.json` |
//! | 镜像 | `base_url` **替换** `https://github.com` 前缀 |
//!
//! 为什么不调 GitHub API 的 `/releases` 列表「自己找最新版本」：匿名 API 是
//! **60 次/小时/IP**（一个出口 IP 后面可能是整个办公室），而且**镜像站大多只代理
//! `download` 路径、不代理 API**。`releases/latest/download/...` 是重定向，不计 rate limit，
//! 镜像只要替换前缀就能整段代理（`ai-docs/decisions/adr-0005-self-update.md § 1`）。
//!
//! # 传输（自己写的最小 HTTPS GET）
//!
//! 不引 `reqwest`（它 0.13 的 `rustls` feature 会拉 **aws-lc-rs**，Windows 上没有 C 工具链就编不过），
//! 用 `tokio` + `tokio-rustls` + `rustls`（**ring** provider）+ `webpki-roots` 自己发一条
//! `GET … Connection: close`，见 [`get`]。
//!
//! 抓取是**可注入**的：`ctx.fetcher = Some(...)`（[`Fetcher`]）之后 `check` / `apply`
//! 一行网络都不碰 —— 这就是本 crate 的测试能完全脱网的原因。
//!
//! # 四道闸（顺序固定，全部必过）
//!
//! | # | 闸 | 防什么 |
//! | --- | --- | --- |
//! | 1 | `size` | 下载被截断 / 被塞巨型文件 |
//! | 2 | `sha256` | 资产被替换（**含镜像站作恶**） |
//! | 3 | 签名（`require_signature` 时） | 「同一个 GitHub 账号发的假货」 |
//! | 4 | 冒烟测试 `<staging> version --json` | 下到了一个能跑但**不是我们的**东西 |
//!
//! 任何一道失败 → **拒绝替换，原二进制毫发无损**，错误里写明是哪一道
//! （见 [`UpdateError`]）。第 3 道闸**fail closed**：`require_signature = true` 而没注入
//! [`SignatureVerifier`] 时直接拒绝，不降级 —— 因为**校验和不是签名**（清单本身也在
//! GitHub / 镜像站上）。
//!
//! # 替换正在运行的自己（Windows 的关键）
//!
//! Windows **不允许删除**正在运行的可执行文件，但**允许重命名**它：
//! `burrow.exe` → `burrow.exe.old-<pid>` → 新二进制复制到 `burrow.exe`。由此推出两条硬约束：
//!
//! 1. [`apply`] 返回后，**内存里跑的仍然是旧代码** —— 调用方必须立刻退出，
//!    否则用户以为更新了其实没有；
//! 2. 安装目录里任何**被加载的 DLL / 资源**都会阻止重命名 → 被更新的程序应当是
//!    **单一自包含 exe**。
//!
//! # 重启不是本 crate 的事
//!
//! [`apply`] 只替换文件并返回 [`Applied`]，**重启由调用方决定**：服务态让服务管理器
//! （Windows SCM 的 failure actions / `systemd Restart=always` / `launchd KeepAlive` /
//! 任务计划程序的「失败后重新启动」）把新版本拉起来；前台 CLI 下次启动即新版本。
//! 这样权限需求最小，也不会让服务状态错乱（`update-flow.md § 6`）。
//!
//! # 刻意留的缺口（首版限制）
//!
//! - **不解压**：资产必须是**裸二进制**。文件名像压缩包（`.zip` / `.tar.gz` / …）或内容魔数
//!   像压缩包时直接报错，绝不尝试解压（解压要处理 zip-slip、权限位、旁挂 DLL）。
//! - **不内置签名算法**：没有 zipsign / minisign 依赖，签名只留
//!   [`SignatureVerifier`] 这个注入点（配置了 `require_signature` 时由调用方提供实现）。
//! - **不支持代理**：内置实现收到 `proxy` 会返回 [`UpdateError::ProxyUnsupported`]
//!   （明确报错，不静默直连）；要走代理就用 [`Fetcher`] 注入自己的传输栈。
//! - **不节流、不退避、不重启**：检查频率与重启策略属于产品层。
//! - **不降级**：清单版本 ≤ 当前版本时 `available = false`，绝不回退到旧版本。
//!
//! # 最小用法
//!
//! ```no_run
//! use peon_burrow_update::{apply, check, Channel, UpdateContext};
//! use semver::Version;
//! use std::path::PathBuf;
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let mut ctx = UpdateContext::new(
//!     Version::parse("0.1.0")?,
//!     PathBuf::from(r"C:\Program Files\Peon Burrow"),
//! );
//! ctx.channel = Channel::Stable;
//! ctx.require_signature = false; // ⚠️ 只有签名工具链还没接上时才这样降级（ADR-0005 § 3）
//!
//! let status = check(&ctx).await?;
//! if status.available {
//!     let applied = apply(&ctx).await?;
//!     println!("{} → {}（{}）", applied.from, applied.to, applied.asset);
//!     // ⚠️ 此时内存里还是旧代码：立刻（非正常）退出，让服务管理器把新版本拉起来
//! }
//! # Ok(())
//! # }
//! ```

pub mod channel;
pub mod error;
pub mod host;
pub mod manifest;
pub mod replace;
pub mod verify;

pub(crate) mod download;
pub(crate) mod smoke;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use semver::Version;

pub use channel::{Channel, GITHUB_ORIGIN, MANIFEST_FILE, REPO, asset_url, manifest_url};
pub use download::{FetchFuture, Fetcher, MAX_REDIRECTS, REQUEST_TIMEOUT, get};
pub use error::UpdateError;
pub use host::{host_triple, triple_for};
pub use manifest::{Asset, MANIFEST_SCHEMA, Manifest, looks_like_archive};
pub use replace::{cleanup_stale_backups, replace_binary};
pub use smoke::SMOKE_TEST_TIMEOUT;
pub use verify::{SignatureVerifier, sha256_hex};

use crate::error::IoContext;

/// 清单响应的字节上限（一份清单几十 KB 而已；坏镜像不该能把内存吃光）。
pub const MAX_MANIFEST_BYTES: u64 = 256 * 1024;

/// 资产的**绝对**字节上限；清单自己声明的 `size` 还会再收一次。
pub const MAX_ASSET_BYTES: u64 = 512 * 1024 * 1024;

/// 下载资产时比「清单声明的 `size`」多允许的字节数。
///
/// 多下一点再交给第 1 道闸（`size`）判断，是为了让「资产被塞大」也报
/// [`UpdateError::SizeMismatch`]（一句人话），而不是底层网络层的
/// [`UpdateError::BodyTooLarge`]（那是「我们防爆的闸」，不是「清单对不上」）。
pub const ASSET_DOWNLOAD_SLACK: u64 = 1024 * 1024;

/// staging 目录名（相对 [`UpdateContext::install_dir`]）。
///
/// 下载物先落在这里（`<install_dir>/.peon-burrow-update/<version>/<asset>`），
/// 校验、冒烟测试都在这里完成；成功或失败后整个目录都会被删掉。
pub const STAGING_DIR: &str = ".peon-burrow-update";

/// 一次更新所需的**全部**输入。
///
/// 本 crate 不自己去问配置、不问服务管理器 —— `install_dir` 只有调用方知道
/// （用户级与系统级是不同路径），所以这里全是值（`ai-docs/modules.md § 6`）。
///
/// 用 [`UpdateContext::new`] 起手，再按需覆盖字段。
/// 字段全是可以廉价复制的句柄（`Arc` / `PathBuf` / 值），所以 `Clone` 很便宜：
/// 控制面钩子与启动检查各持一份，互不干扰。
#[derive(Clone)]
pub struct UpdateContext {
    /// 当前版本。与清单版本比较决定是否更新，且**只升不降**。
    pub current_version: Version,

    /// 安装目录：被替换的二进制所在的目录。
    ///
    /// ⚠️ 必须是**可写**的（用户级安装天然满足；系统服务由 LocalSystem 自己写，不需要再提权）。
    pub install_dir: PathBuf,

    /// 更新渠道（[`Channel::Stable`] / [`Channel::Beta`]）。
    pub channel: Channel,

    /// 镜像站前缀，**替换** `https://github.com`；`None` = 直接用 GitHub。
    ///
    /// 例：`Some("https://gh-proxy.com/https://github.com".into())`。
    pub base_url: Option<String>,

    /// 代理：**从配置来，绝不读环境变量**（系统服务不继承用户环境）。
    ///
    /// ⚠️ 内置抓取实现这一版**还不支持代理**：给了值就返回
    /// [`UpdateError::ProxyUnsupported`]（明确报错，绝不静默直连）。
    /// 要走代理，用 [`UpdateContext::fetcher`] 注入自己的传输栈。
    pub proxy: Option<String>,

    /// 是否强制验签。
    ///
    /// `true`（推荐，ADR-0005 的安全默认值）时：没有 [`UpdateContext::verifier`]
    /// 或资产没有 `signature` → **拒绝安装**（fail closed），不降级成「只校校验和」。
    pub require_signature: bool,

    /// 可信公钥列表（minisign / zipsign 公钥；格式由调用方的 verifier 定义）。
    ///
    /// 本 crate **不解析**它：注入 [`UpdateContext::verifier`] 时由调用方把它一起构造进去，
    /// 这里只作为「配了哪些公钥」的可观测字段（会写进 `update.verify` 日志）。
    pub pubkeys: Vec<String>,

    /// 安装目录里**要被替换的文件名**（如 `burrow.exe`）。
    ///
    /// `None` 时用 `std::env::current_exe()` 的文件名推断（正在运行的自己就是被替换的目标）；
    /// 两者都不行就报 [`UpdateError::CannotDetermineBinary`]。
    /// 单测里显式给一个临时目录下的名字即可完全不碰真实安装目录。
    pub binary_name: Option<String>,

    /// 签名校验器（第 3 道闸的实现方）。
    ///
    /// 本 crate 不内置任何签名算法，所以这是**唯一**的验签入口；
    /// 见 [`SignatureVerifier`] 与「校验和不是签名」的说明。
    pub verifier: Option<Arc<dyn SignatureVerifier>>,

    /// **跳过第 4 道闸（冒烟测试）**，默认 `false`。
    ///
    /// 只在「本平台没法把新二进制跑起来」时才该打开（例如交叉编译产物、受限沙箱、
    /// 单元测试里的假二进制）。打开等于放弃「下到了一个能跑但不是我们的东西」这道闸，
    /// 会打一条 `warn` —— **生产环境不要开**。
    pub skip_smoke_test: bool,

    /// 可注入的抓取函数（[`Fetcher`]），`None` = 用内置的 [`get`]。
    ///
    /// 两个用途：
    ///
    /// 1. **测试脱网**：喂一份伪造的清单/资产字节，`check` / `apply` 一行网络都不碰；
    /// 2. **换传输栈**：自签证书的内网镜像、mTLS、真要走代理 —— 都在这里面自己做，
    ///    此时 [`UpdateContext::proxy`] 由 fetcher 自己解释（内置实现只认 `None`）。
    pub fetcher: Option<Fetcher>,
}

impl UpdateContext {
    /// 用安全默认值起手：`channel = Stable`、走 github.com、`require_signature = true`
    /// （ADR-0005 的安全默认值：缺签名 = 拒绝）、不跳冒烟测试。
    ///
    /// ⚠️ 签名工具链还没接上的话，必须**显式**写 `ctx.require_signature = false`，
    /// 并在 release notes 里说明这次降级（`adr-0005 § 3`）。
    #[must_use]
    pub fn new(current_version: Version, install_dir: PathBuf) -> Self {
        Self {
            current_version,
            install_dir,
            channel: Channel::Stable,
            base_url: None,
            proxy: None,
            require_signature: true,
            pubkeys: Vec::new(),
            binary_name: None,
            verifier: None,
            skip_smoke_test: false,
            fetcher: None,
        }
    }

    /// 安装目录里要被替换的二进制路径。
    ///
    /// # Errors
    ///
    /// [`UpdateError::CannotDetermineBinary`]：既没给 `binary_name`，`current_exe()` 也问不出来。
    pub fn target_binary(&self) -> Result<PathBuf, UpdateError> {
        match &self.binary_name {
            Some(name) => Ok(self.install_dir.join(name)),
            None => {
                let exe = std::env::current_exe().map_err(|error| {
                    UpdateError::CannotDetermineBinary {
                        reason: error.to_string(),
                    }
                })?;
                let name = exe
                    .file_name()
                    .ok_or_else(|| UpdateError::CannotDetermineBinary {
                        reason: format!("{} 没有文件名", exe.display()),
                    })?;
                Ok(self.install_dir.join(name))
            }
        }
    }

    /// 本次更新的 staging 根目录（`<install_dir>/.peon-burrow-update`）。
    #[must_use]
    pub fn staging_root(&self) -> PathBuf {
        self.install_dir.join(STAGING_DIR)
    }

    /// 清单 URL（按渠道与镜像策略拼出来）。
    #[must_use]
    pub fn manifest_url(&self) -> String {
        manifest_url(self.base_url.as_deref(), self.channel)
    }

    /// 某个资产的下载 URL（与清单同一个目录）。
    #[must_use]
    pub fn asset_url(&self, asset_name: &str) -> String {
        asset_url(self.base_url.as_deref(), self.channel, asset_name)
    }
}

impl std::fmt::Debug for UpdateContext {
    /// 手写 `Debug`：**代理可能带密码**，绝不能原样打进日志。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpdateContext")
            .field("current_version", &self.current_version)
            .field("install_dir", &self.install_dir)
            .field("channel", &self.channel)
            .field("base_url", &self.base_url)
            .field("proxy", &self.proxy.as_ref().map(|_| "<已配置>"))
            .field("require_signature", &self.require_signature)
            .field("pubkeys", &self.pubkeys.len())
            .field("binary_name", &self.binary_name)
            .field("verifier", &self.verifier.as_ref().map(|_| "<已注入>"))
            .field("skip_smoke_test", &self.skip_smoke_test)
            .field("fetcher", &self.fetcher.as_ref().map(|_| "<已注入>"))
            .finish()
    }
}

/// [`check`] 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateStatus {
    /// 有没有可用的更新。`false` 时 [`UpdateStatus::asset`] 一定是 `None`，**不动任何文件**。
    pub available: bool,
    /// 当前版本（原样回显，方便日志）。
    pub current: Version,
    /// 清单里的版本。
    pub latest: Version,
    /// 本平台要下载的资产；`available = false` 时为 `None`。
    pub asset: Option<Asset>,
}

/// [`apply`] 成功后的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// 替换前的版本。
    pub from: Version,
    /// 替换后的版本（**新代码还没在内存里生效**：见 [`apply`] 的说明）。
    pub to: Version,
    /// 被复制进安装目录的资产文件名。
    pub asset: String,
}

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
