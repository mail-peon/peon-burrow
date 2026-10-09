//! 自更新失败的原因：**一条错误 = 一道闸**。
//!
//! 分层原则（`ai-docs/modules.md § 11 C3`）：错误分类而不是字符串匹配。调用方据此决定
//! 「重试 / 退避 / 放弃」，而不是去 `grep` 错误文案。

use std::path::PathBuf;

/// 自更新过程中的失败原因。
///
/// 每一条都指向**具体是哪一步**失败了：网络（[`UpdateError::Download`]）、
/// 清单（[`UpdateError::Manifest`] / [`UpdateError::ChannelMismatch`] / [`UpdateError::UnsupportedSchema`]）、
/// 平台（[`UpdateError::UnknownHost`] / [`UpdateError::NoAssetForTarget`]）、
/// 四道闸（[`UpdateError::SizeMismatch`] → [`UpdateError::Sha256Mismatch`] →
/// [`UpdateError::SignatureVerifierMissing`] / [`UpdateError::SignatureInvalid`] →
/// [`UpdateError::SmokeTestFailed`]）、
/// 替换（[`UpdateError::Replace`] / [`UpdateError::RollbackFailed`]）。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum UpdateError {
    // ---- 平台 / 清单选择 ----
    /// 当前 `arch` + `os` 组合不在本 crate 认识的 host triple 表里。
    ///
    /// 这是**本平台暂无更新**的一种，不是「猜一个 target」的理由。
    #[error("无法识别当前平台（arch={arch}, os={os}）：本平台暂无更新")]
    UnknownHost {
        /// `std::env::consts::ARCH`。
        arch: String,
        /// `std::env::consts::OS`。
        os: String,
    },

    /// 清单里没有本平台 `target` 的资产。
    ///
    /// 按设计**不猜、不下载**（`ai-docs/design/update-flow.md § 8` 第 4 条）。
    #[error("本平台暂无更新：清单里没有 target = {triple} 的资产")]
    NoAssetForTarget {
        /// 当前平台的 host triple（如 `x86_64-pc-windows-msvc`）。
        triple: String,
    },

    /// 清单里的 `channel` 与请求的渠道不符 → 整份清单作废（防止 beta 清单被 stable 用上）。
    #[error("清单渠道不匹配：清单是 {manifest}，请求的是 {requested}（该清单被忽略）")]
    ChannelMismatch {
        /// 清单里写的渠道。
        manifest: String,
        /// 调用方请求的渠道（`stable` / `beta`）。
        requested: &'static str,
    },

    /// 清单的 `schema` 不是本 crate 认识的版本。
    #[error("清单 schema={found} 不受支持（本 crate 只认 schema={supported}）")]
    UnsupportedSchema {
        /// 清单里写的 `schema`。
        found: u32,
        /// 本 crate 支持的 `schema`（[`crate::MANIFEST_SCHEMA`]）。
        supported: u32,
    },

    /// 清单不是合法 JSON，或缺少必填字段。
    #[error("清单解析失败：{url}：{source}")]
    Manifest {
        /// 清单 URL（写进日志用）。
        url: String,
        /// `serde_json` 的原始错误。
        #[source]
        source: serde_json::Error,
    },

    // ---- 网络 ----
    /// URL 我们处理不了（缺 scheme、非 http/https、主机名或端口不合法）。
    #[error("URL 不可用：{url}：{reason}")]
    BadUrl {
        /// 出错的 URL。
        url: String,
        /// 具体原因。
        reason: String,
    },

    /// 配了代理，但内置实现这一版还不支持 —— **明确报错，绝不静默直连**。
    ///
    /// 要支持代理就是「先 CONNECT 建隧道，再在隧道里发 GET」；在那之前，
    /// 「配了代理却没走代理」必须是一个看得见的错误。
    #[error(
        "本版暂不支持代理（{proxy}）：请等 CONNECT 隧道支持，或用 UpdateContext::fetcher 注入自己的传输栈"
    )]
    ProxyUnsupported {
        /// 调用方给（并被拒绝）的代理 URL。
        proxy: String,
    },

    /// 网络读写失败（DNS / 连接被拒 / 连接中断 / TLS 握手 / 写入失败）。
    #[error("网络请求失败：{url}：{source}")]
    Download {
        /// 出错的 URL。
        url: String,
        /// 底层 io 错误（TLS 握手的失败文案也在这里）。
        #[source]
        source: std::io::Error,
    },

    /// 整次抓取超过 [`crate::REQUEST_TIMEOUT`]。
    #[error("请求超时（超过 {seconds} 秒）：{url}")]
    Timeout {
        /// 出错的 URL。
        url: String,
        /// 超时秒数。
        seconds: u64,
    },

    /// TLS 握手 / 服务器名失败。
    #[error("TLS 失败：{url}：{reason}")]
    Tls {
        /// 出错的 URL。
        url: String,
        /// rustls 给的文案。
        reason: String,
    },

    /// rustls 客户端配置建不起来（理论上不会发生：ring provider 一定有安全默认版本）。
    #[error("TLS 配置失败：{reason}")]
    TlsConfig {
        /// 原因。
        reason: String,
    },

    /// HTTP 状态码不是 200（重定向跟完之后仍然不是）。
    #[error("HTTP 状态码 {status}：{url}")]
    HttpStatus {
        /// 出错的 URL。
        url: String,
        /// 状态码。
        status: u16,
    },

    /// 响应本身不合法（状态行/头看不懂、chunked 坏了、重定向超限或缺少 Location、包体被截断）。
    #[error("HTTP 响应不合法：{url}：{reason}")]
    HttpProtocol {
        /// 出错的 URL。
        url: String,
        /// 具体原因。
        reason: String,
    },

    /// 响应体超过了上限：直接断开，**不把内存交给对端**。
    #[error("响应体过大：{url} 超过 {limit} 字节上限")]
    BodyTooLarge {
        /// 出错的 URL。
        url: String,
        /// 允许的最大字节数。
        limit: u64,
    },

    /// 清单声明的资产比 [`crate::MAX_ASSET_BYTES`] 还大 → 直接拒绝。
    #[error("资产业余大小 {size} 字节超过上限 {max} 字节")]
    AssetTooLarge {
        /// 清单声明的 `size`。
        size: u64,
        /// 上限。
        max: u64,
    },

    // ---- 四道闸 ----
    /// 闸 1：下载到的字节数与清单 `size` 不符（被截断 / 被塞大文件）。
    #[error("大小校验失败：清单声明 {expected} 字节，实际 {actual} 字节")]
    SizeMismatch {
        /// 清单里的 `size`。
        expected: u64,
        /// 实际收到的字节数。
        actual: u64,
    },

    /// 清单里的 `sha256` 不是 64 位十六进制 → 清单本身不可信。
    #[error("清单里的 sha256 不是 64 位十六进制：{value}")]
    InvalidSha256 {
        /// 清单里原样的值。
        value: String,
    },

    /// 闸 2：sha256 不匹配 → **拒绝**，原二进制毫发无损。
    #[error("校验和不匹配：清单 {expected}，实际 {actual}")]
    Sha256Mismatch {
        /// 清单里的 sha256（小写）。
        expected: String,
        /// 实际算出的 sha256（小写）。
        actual: String,
    },

    /// 闸 3：`require_signature = true` 但没注入 [`crate::SignatureVerifier`] → **fail closed**。
    #[error(
        "配置要求验签，但没有注入签名校验器（SignatureVerifier）：拒绝安装。\
         校验和不是签名（清单本身也可能被改），所以这里不降级 —— \
         要么注入 verifier，要么显式把 require_signature 设为 false"
    )]
    SignatureVerifierMissing,

    /// 闸 3：签名校验器拒绝了这份资产。
    #[error("签名校验失败：{reason}")]
    SignatureInvalid {
        /// 校验器给出的原因。
        reason: String,
    },

    /// 资产看起来是压缩包 → 本 crate 不支持解压（见 crate 文档「不支持压缩包」）。
    #[error("不支持的资产格式（本 crate 只支持裸二进制，不解压）：{name}{detail}")]
    UnsupportedAssetFormat {
        /// 资产文件名。
        name: String,
        /// 补充说明（如「内容魔数像 zip」）。
        detail: String,
    },

    /// 闸 4：冒烟测试起不来（spawn 失败，或阻塞任务 panic）。
    #[error("冒烟测试无法执行：{path}：{reason}")]
    SmokeTestUnrunnable {
        /// 被执行的 staging 二进制。
        path: PathBuf,
        /// 失败原因。
        reason: String,
    },

    /// 闸 4：冒烟测试非零退出。
    #[error("冒烟测试失败（退出码 {code}）：{path}：{output}")]
    SmokeTestFailed {
        /// 被执行的 staging 二进制。
        path: PathBuf,
        /// 退出码（被信号杀死时为 -1）。
        code: i32,
        /// stdout/stderr 的片段（截断过）。
        output: String,
    },

    /// 闸 4：冒烟测试超时（超过 [`crate::SMOKE_TEST_TIMEOUT`]）。
    #[error("冒烟测试超时（超过 {seconds} 秒）：{path}")]
    SmokeTestTimeout {
        /// 被执行的 staging 二进制。
        path: PathBuf,
        /// 超时秒数。
        seconds: u64,
    },

    /// 闸 4：新二进制跑起来了，但报的版本与清单不一致。
    #[error("冒烟测试版本不一致：期望 {expected}，实际 {actual}（输出：{output}）")]
    SmokeTestVersionMismatch {
        /// 清单里的版本。
        expected: String,
        /// 二进制自报的版本（解析不出来时是原始字符串）。
        actual: String,
        /// stdout 的片段（截断过）。
        output: String,
    },

    // ---- 流程 / 文件 ----
    /// [`crate::apply`] 被调用时其实没有可用更新（同一个清单已经比当前版本旧或相同）。
    #[error("没有可应用的更新：当前 {current}，清单 {latest}（不降级）")]
    NoUpdateAvailable {
        /// 当前版本。
        current: semver::Version,
        /// 清单里的版本。
        latest: semver::Version,
    },

    /// 安装目录里找不到要被替换的二进制。
    #[error("安装目录里没有这个二进制：{path}")]
    MissingBinary {
        /// 期望存在的路径。
        path: PathBuf,
    },

    /// 被替换的文件名既没显式给，也无法从 `current_exe()` 推断。
    #[error("无法确定要替换的二进制文件名：{reason}（请在 UpdateContext::binary_name 里显式给出）")]
    CannotDetermineBinary {
        /// 推断失败的原因。
        reason: String,
    },

    /// 替换失败：**已经回滚**，原二进制还在。
    #[error("替换二进制失败（已回滚，原文件未受影响）：{target}：{source}")]
    Replace {
        /// 被替换的路径。
        target: PathBuf,
        /// 复制新二进制时的 io 错误。
        #[source]
        source: std::io::Error,
    },

    /// 替换失败且**回滚也失败** —— 这是最坏的情况，日志里必须同时给出两个原因。
    #[error(
        "替换二进制失败且回滚失败：{target}（备份在 {backup}）。\
         复制失败：{replace_error}；回滚失败：{source}"
    )]
    RollbackFailed {
        /// 被替换的路径。
        target: PathBuf,
        /// 备份（旧二进制）所在路径。
        backup: PathBuf,
        /// 复制新二进制时的错误。
        replace_error: String,
        /// 回滚时的 io 错误。
        #[source]
        source: std::io::Error,
    },

    /// 其它文件操作失败（建目录 / 落盘 staging / 清理）。
    #[error("文件操作失败：{path}：{source}")]
    Io {
        /// 出错的路径。
        path: PathBuf,
        /// io 错误。
        #[source]
        source: std::io::Error,
    },
}

impl UpdateError {
    /// 这条错误是不是「**本平台就是这样，重试也没用**」。
    ///
    /// 调用方用它决定「退避重试」还是「直接记一条 info」：
    /// [`UpdateError::NoAssetForTarget`] / [`UpdateError::UnknownHost`] 属于前者（不是网络故障），
    /// 其余（网络、磁盘、锁）都值得退避重试。
    ///
    /// ```no_run
    /// # use peon_burrow_update::UpdateError;
    /// # fn demo(err: &UpdateError) {
    /// if !err.is_platform_unsupported() {
    ///     // 退避重试：1h → 6h → 24h
    /// }
    /// # }
    /// ```
    #[must_use]
    pub fn is_platform_unsupported(&self) -> bool {
        matches!(
            self,
            UpdateError::NoAssetForTarget { .. } | UpdateError::UnknownHost { .. }
        )
    }
}

/// 给 `std::io::Result` 补上「出错的路径」的小工具（避免每个调用点手写 `map_err`）。
pub(crate) trait IoContext<T> {
    /// 把 io 错误包成 [`UpdateError::Io`]，并带上路径。
    fn with_path(self, path: impl Into<PathBuf>) -> Result<T, UpdateError>;
}

impl<T> IoContext<T> for std::io::Result<T> {
    fn with_path(self, path: impl Into<PathBuf>) -> Result<T, UpdateError> {
        self.map_err(|source| UpdateError::Io {
            path: path.into(),
            source,
        })
    }
}
