use crate::{
    Asset, Channel, Fetcher, STAGING_DIR, SignatureVerifier, UpdateError, asset_url, manifest_url,
};
use semver::Version;
use std::path::PathBuf;
use std::sync::Arc;

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

    /// 可注入的抓取函数（[`Fetcher`]），`None` = 用内置的 [`crate::get`]。
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

/// [`crate::check`] 的结果。
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

/// [`crate::apply`] 成功后的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// 替换前的版本。
    pub from: Version,
    /// 替换后的版本（**新代码还没在内存里生效**：见 [`crate::apply`] 的说明）。
    pub to: Version,
    /// 被复制进安装目录的资产文件名。
    pub asset: String,
}
