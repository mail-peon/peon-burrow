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

pub use channel::{Channel, GITHUB_ORIGIN, MANIFEST_FILE, REPO, asset_url, manifest_url};
pub use download::{FetchFuture, Fetcher, MAX_REDIRECTS, REQUEST_TIMEOUT, get};
pub use error::UpdateError;
pub use host::{host_triple, triple_for};
pub use manifest::{Asset, MANIFEST_SCHEMA, Manifest, looks_like_archive};
pub use replace::{cleanup_stale_backups, replace_binary};
pub use smoke::SMOKE_TEST_TIMEOUT;
pub use verify::{SignatureVerifier, sha256_hex};

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

mod apply;
mod check;
mod context;
pub use apply::apply;
pub use check::check;
pub use context::{Applied, UpdateContext, UpdateStatus};
