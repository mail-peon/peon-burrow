//! 服务托管的**单一 trait** 与它周边的类型、错误、计划模型。
//!
//! 平台差异全部收在 [`ServiceHost`] 这个 trait 之后（布局铁律：
//! 不拆成 `service-windows` / `-macos` / `-linux` 三个 crate，否则会得到三个版本号、
//! 三份 CI 矩阵和一堆 `cfg` 转发）。
//!
//! 两种安装级别（[`ServiceLevel`]）的行为对照表在 `ai-docs/service-lifecycle.md § 2`：
//!
//! | 级别 | Windows | macOS | Linux |
//! | --- | --- | --- | --- |
//! | [`ServiceLevel::User`]（默认，零提权） | 任务计划程序 XML | LaunchAgent plist | systemd `--user` |
//! | [`ServiceLevel::System`]（一次提权） | SCM（`sc.exe`） | LaunchDaemon plist | systemd system unit |

use std::path::PathBuf;

use crate::runner::{CommandSpec, FileInstall, Runner};

pub use peon_burrow_ipc_types::{Autostart, ServiceLevel, ServiceStatus};

/// 默认服务名（注册进 SCM / 计划任务 / launchd / systemd 的那个名字）。
pub const DEFAULT_SERVICE_NAME: &str = "peon-burrow";

/// 失败后的重启间隔（`service-lifecycle.md § 2`：间隔 1 分钟 × 3 次）。
///
/// 秒数是各平台统一的内部表示，落地时各平台自己换算：
/// Windows 任务 XML 用 ISO-8601（`PT1M`）、SCM 用**毫秒**、systemd 用秒、launchd 不用间隔。
pub const RESTART_INTERVAL_SECS: u64 = 60;

/// 失败后最多重启几次。
pub const RESTART_RETRY_COUNT: u32 = 3;

/// 一次 `install` / `start` / `stop` / `set_autostart` 需要执行的**变更计划**。
///
/// 这是「可测」的关键：平台模块只负责**生成**计划（纯函数，测试直接断言命令行与文件正文），
/// [`Runner`] 只负责**执行**计划。真实执行只发生在 [`Plan::execute`]，
/// 而调用它的是 [`ServiceHost::install`] 等方法 —— 调用方显式要求才会动机器。
#[derive(Debug, Clone, Default)]
pub struct Plan {
    /// 要落盘的文件（systemd unit / launchd plist）。
    pub files: Vec<FileInstall>,
    /// 要执行的命令，**按加入顺序**（顺序即语义）。
    commands: Vec<(CommandSpec, BestEffort)>,
    /// 要删掉的文件（必须在 [`Plan::commands`] **之后**执行：
    /// 得先 `bootout` / `disable` 再删 plist / unit）。
    removals: Vec<PathBuf>,
}

/// 这条命令失败了要不要让整个计划失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BestEffort {
    /// 默认：失败就中断（`service-lifecycle.md § 3` 第 9a 步不容许静默吞错）。
    No,
    /// 幂等清理：失败也无所谓（服务本来就没加载时 `launchctl bootout` 会报错）。
    Yes,
}

impl Plan {
    /// 空计划。
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一份要落盘的文件。
    #[must_use]
    pub fn file(mut self, file: FileInstall) -> Self {
        self.files.push(file);
        self
    }

    /// 追加一条要执行的命令（失败即中断）。
    #[must_use]
    pub fn command(mut self, spec: CommandSpec) -> Self {
        self.commands.push((spec, BestEffort::No));
        self
    }

    /// 追加一条「失败也无所谓」的命令。
    ///
    /// 只用于**幂等清理**（例如卸载时的 `launchctl bootout`：服务本来就没加载时它会报错，
    /// 而那种情况恰恰是我们想要的结果）。业务命令不要用它 —— 静默吞掉失败正是
    /// `service-lifecycle.md § 3` 第 9a 步要防的事。
    #[must_use]
    pub fn command_ignoring_failure(mut self, spec: CommandSpec) -> Self {
        self.commands.push((spec, BestEffort::Yes));
        self
    }

    /// 追加一个要删除的文件（在命令之后执行）。
    #[must_use]
    pub fn remove(mut self, path: PathBuf) -> Self {
        self.removals.push(path);
        self
    }

    /// 计划里的命令（不含内部标记，测试与日志用）。
    pub fn command_specs(&self) -> Vec<&CommandSpec> {
        self.commands.iter().map(|(spec, _)| spec).collect()
    }

    /// 计划是否什么都不做（卸载时「本来就没装」会走到这里）。
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.commands.is_empty() && self.removals.is_empty()
    }

    /// 执行这个计划。
    ///
    /// 顺序即语义：
    /// 1. 先写文件（`systemctl` 必须能读到刚写的 unit 文件）；
    /// 2. 再跑命令（按加入顺序）；
    /// 3. 最后删文件（`bootout` / `disable` 之后再删 plist / unit）。
    ///
    /// 中途失败会立刻停下并把失败动作写进错误文案 —— 剩下的步骤不再执行，
    /// 调用方（`service install`）据此决定要不要回滚。
    pub fn execute(self, runner: &dyn Runner) -> Result<(), ServiceError> {
        for file in &self.files {
            runner.install_file(file)?;
        }
        for (spec, best_effort) in &self.commands {
            match (runner.run(spec), best_effort) {
                (Ok(_), _) => {}
                (Err(_), BestEffort::Yes) => {
                    tracing::debug!(
                        event = "service.command_ignored",
                        purpose = %spec.purpose,
                        "幂等清理命令失败（可忽略）"
                    );
                }
                (Err(error), BestEffort::No) => return Err(error),
            }
        }
        for path in &self.removals {
            runner.remove_file(path)?;
        }
        Ok(())
    }
}

/// 跑一条只读命令，拿回它的标准输出。
///
/// `status` / `verify_restart_policy` 的「读」那一半都走这里：
/// 它们不该被当成变更动作，所以不经过 [`Plan`]。
pub(crate) fn read_only(runner: &dyn Runner, spec: &CommandSpec) -> Result<String, ServiceError> {
    runner.run(spec)
}

/// 三个平台宿主共享的安装前检查。
///
/// 顺序是固定的（三处各写一遍迟早会漏一条）：
/// 1. **配置组合合法**（[`InstallOptions::validate`]：用户级做不到开机即起…）；
/// 2. **宿主级别对得上**（拿用户级宿主去装系统服务是调用方的 bug，早报早好，且此时一条命令都还没发）；
/// 3. **安装目录里的副本**：注册进服务管理器的必须是安装目录里的二进制，
///    不能是临时目录里正在双击运行的那个 exe（`service-lifecycle.md § 3` 第 3 步：
///    临时目录会被清理，重启后服务就指向一个不存在的路径 —— 「装完看着挺好，重启就没了」）。
///    本 crate 只**提示**，复制动作由产品层做（库不碰用户的安装目录）。
pub(crate) fn prepare_install(
    opts: &InstallOptions,
    level: ServiceLevel,
) -> Result<(), ServiceError> {
    opts.validate()?;
    if opts.level != level {
        return Err(ServiceError::invalid_options(format!(
            "这个宿主负责 {level:?} 级安装，装不了 {:?} 级",
            opts.level
        )));
    }
    Ok(())
}

/// 服务托管的错误。
///
/// 文案规范见 `ai-docs/design/logging-and-diagnostics.md § 4`：
/// **说人话、给下一步、不甩锅、可复制**。所以：
///
/// - `Display` 给的是「发生了什么」（人话，不出现裸的 `EADDRINUSE` 这类原生码）；
/// - [`ServiceError::action`] 给的是「接下来做什么」（至少一条可执行动作）；
/// - 原生命令的原始输出放在 `detail` 字段里，不丢证据，但不作为主信息。
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    /// 这个平台上没有可用的服务机制。
    ///
    /// 不属于三大平台时才会出现；`doctor` 应当据此建议用户用前台 `run`。
    #[error("当前系统（{os}）没有可用的服务机制")]
    UnsupportedPlatform {
        /// 操作系统名。
        os: &'static str,
    },

    /// 选定的安装级别与自启方式不匹配（例如用户级 + 开机启动）。
    #[error("安装级别与自启方式不匹配：{message}")]
    InvalidOptions {
        /// 具体哪里不匹配。
        message: String,
    },

    /// 服务还没安装。
    #[error("服务「{name}」尚未安装")]
    NotInstalled {
        /// 服务名。
        name: String,
    },

    /// 服务已经安装了（重复安装要先卸载，或明确要求覆盖）。
    #[error("服务「{name}」已经安装过了")]
    AlreadyInstalled {
        /// 服务名。
        name: String,
    },

    /// 外部命令非零退出。
    #[error("执行「{purpose}」失败{code_text}：{message}")]
    CommandFailed {
        /// 那条命令在干什么。
        purpose: String,
        /// 退出码的可读形式（`（退出码 5）` / 空串）。
        code_text: String,
        /// 工具自己的输出（原样保留，附在冒号后面）。
        message: String,
    },

    /// 命令成功了，但输出空得没法用。
    #[error("执行「{purpose}」有返回，但内容为空，读不到需要的信息")]
    EmptyOutput {
        /// 那条命令在干什么。
        purpose: String,
    },

    /// 命令成功了，但输出不是 UTF-8。
    #[error("执行「{purpose}」的输出不是 UTF-8 文本，无法解析")]
    NonUtf8Output {
        /// 那条命令在干什么。
        purpose: String,
    },

    /// 已经把注册信息读回来了，但里面没有失败重启策略。
    ///
    /// 这是 `service install` **自检**（`service-lifecycle.md § 3` 第 9a 步）的专用错误：
    /// 装完看着挺好、崩了不回来，是这条链路最容易漏的一环。
    #[error("「{name}」已注册，但缺少失败重启策略（期望：{expected}，实际读到：{found}）")]
    RestartPolicyMissing {
        /// 服务名。
        name: String,
        /// 期望写入的策略。
        expected: String,
        /// 实际读回来的内容（可能是空字符串）。
        found: String,
    },

    /// 端口探测失败（连占用者都问不出来）。
    #[error("探测端口 {port} 失败：{detail}")]
    ProbeFailed {
        /// 被探测的端口。
        port: u16,
        /// 失败原因。
        detail: String,
    },

    /// 本机 IO 失败。
    #[error("{action}失败：{source}")]
    Io {
        /// 哪个动作失败了（用中文说明，例如「创建目录」）。
        action: &'static str,
        /// 底层错误。
        #[source]
        source: std::io::Error,
    },
}

impl ServiceError {
    /// 构造 [`ServiceError::CommandFailed`]（退出码会自动写进文案）。
    pub fn command_failed(
        purpose: impl Into<String>,
        code: Option<i32>,
        message: impl Into<String>,
    ) -> Self {
        let purpose = purpose.into();
        let code_text = match code {
            Some(code) => format!("（退出码 {code}）"),
            None => String::new(),
        };
        Self::CommandFailed {
            purpose,
            code_text,
            message: message.into(),
        }
    }

    /// 构造 [`ServiceError::InvalidOptions`]。
    pub fn invalid_options(message: impl Into<String>) -> Self {
        Self::InvalidOptions {
            message: message.into(),
        }
    }

    /// 「接下来做什么」—— 至少一条可执行动作，文案已满足「可复制」。
    ///
    /// GUI / CLI 应当把 `Display` 与这个方法的返回值**一起**展示：
    /// 前者说清发生了什么，后者告诉用户下一步点哪里。
    pub fn action(&self) -> String {
        match self {
            Self::UnsupportedPlatform { .. } => {
                "改用前台运行：burrow run；需要常驻请用系统自带的任务计划 / systemd / launchd 手动登记"
                    .to_owned()
            }
            Self::InvalidOptions { .. } => {
                "用户级安装请用 --level user（登录时启动）；要「开机即起」请显式选择 --level system".to_owned()
            }
            Self::NotInstalled { name } => format!(
                "先安装：burrow service install --name {name}；只想起一次请用 burrow run"
            ),
            Self::AlreadyInstalled { name } => format!(
                "先卸载再装：burrow service uninstall --name {name}，然后重新 install"
            ),
            Self::CommandFailed { purpose, .. } => {
                if purpose.contains("提权") || purpose.contains("权限") {
                    "以管理员（Windows）/ root（macOS、Linux）身份重试这条命令".to_owned()
                } else {
                    "把上面这条命令原样复制到终端执行一次，按它的报错处理；仍不行请带上完整输出提 issue"
                        .to_owned()
                }
            }
            Self::EmptyOutput { .. } | Self::NonUtf8Output { .. } => {
                "确认系统命令可执行（Windows：schtasks / sc；macOS：launchctl；Linux：systemctl），然后重试"
                    .to_owned()
            }
            Self::RestartPolicyMissing { name, .. } => format!(
                "重新安装一次即可重写策略：burrow service install --name {name}（会覆盖注册信息）"
            ),
            Self::ProbeFailed { port, .. } => format!(
                "手动查看占用者：Windows 用 netstat -ano -p TCP | findstr {port}；macOS / Linux 用 lsof -nP -iTCP:{port} -sTCP:LISTEN"
            ),
            Self::Io { action, .. } => format!(
                "确认「{action}」涉及的文件或目录存在且当前用户可写，然后重试；细节见上面的原始错误"
            ),
        }
    }

    /// 用户可见的完整两行文案：`做了什么错` + `下一步`。
    pub fn user_message(&self) -> String {
        format!("{self}\n下一步：{}", self.action())
    }
}

/// 失败之后怎么把服务拉回来。
///
/// 这是**一个类型而不是散落的 if**（`adr-0003 § 8`）：`install` 时写入、`doctor` 时自检、
/// 自更新时读取，三处共用同一份定义。
///
/// ⚠️ 自更新要重启服务时，进程必须以**非零退出码**结束（`ExitCode::RestartRequested = 5`）——
/// SCM 的 failure actions 与 launchd 的 `SuccessfulExit=false` 都只对非正常退出生效。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartStrategy {
    /// Windows 系统服务：SCM 的 failure actions（`sc failure`）。
    ScmFailureActions,
    /// Windows 用户级：计划任务的「失败后重新启动」。
    ScheduledTask,
    /// Linux：systemd 的 `Restart=always`。
    Systemd,
    /// macOS：launchd 的 `KeepAlive`。
    Launchd,
    /// 没有崩溃恢复能力（平台不支持，或调用方明确不要）。
    None,
}

impl RestartStrategy {
    /// 一行说明（日志、`doctor`、GUI 都用它，不要再各写一份）。
    pub fn description(self) -> &'static str {
        match self {
            Self::ScmFailureActions => "Windows 服务失败后由 SCM 自动重启",
            Self::ScheduledTask => "Windows 计划任务失败后按间隔重启",
            Self::Systemd => "systemd 自动重启（Restart=always）",
            Self::Launchd => "launchd 崩溃后自动重启（KeepAlive）",
            Self::None => "没有崩溃恢复：进程挂了不会自动回来",
        }
    }
}

/// 这个服务以什么形态跑。
///
/// 目前只用于日志与注册信息的记录（`run --foreground` 与服务态共用同一个二进制）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    /// 服务 / 计划任务形态（没有终端，日志落盘）。
    Service,
    /// 前台形态（有终端，日志打到 stdout）。
    Foreground,
}

impl RunMode {
    /// 写进日志与注册信息的名字（`camelCase`，与发现文件的 `mode` 字段一致）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Service => "service",
            Self::Foreground => "foreground",
        }
    }
}

/// `install` 的输入。
///
/// 字段全公开：调用方（CLI / 安装器 GUI）自己组装，
/// 库不读环境变量、不猜路径（布局铁律 L4「注入而非全局」）。
#[derive(Debug, Clone)]
pub struct InstallOptions {
    /// 服务名（注册进 SCM / 计划任务 / launchd / systemd 的名字）。
    pub name: String,
    /// 安装级别：用户级（零提权，默认）还是系统级（一次提权）。
    pub level: ServiceLevel,
    /// 自启方式。
    pub autostart: Autostart,
    /// 要注册的二进制路径。
    ///
    /// ⚠️ 必须是**安装目录里的副本**，不能是临时目录里正在被双击运行的那个 exe
    /// （`service-lifecycle.md § 3` 第 3 步：临时目录会被清理，重启后服务就指向一个不存在的路径）。
    pub binary_path: PathBuf,
    /// 传给二进制的参数（例如 `["run"]`）。
    pub args: Vec<String>,
    /// 运行形态（写进注册信息，便于 `doctor` 判断）。
    pub mode: RunMode,
}

impl InstallOptions {
    /// 最简构造：给定名字与二进制路径，其余取默认（用户级 · 登录自启 · 服务形态）。
    pub fn new(name: impl Into<String>, binary_path: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            level: ServiceLevel::User,
            autostart: Autostart::Logon,
            binary_path: binary_path.into(),
            // 服务形态默认带 `run`：CLI 的默认子命令就是跑中继。
            args: vec!["run".to_owned()],
            mode: RunMode::Service,
        }
    }

    /// 平台的失败重启策略（`install` 写进去、自检读回来的是同一个值）。
    pub fn restart_strategy(&self) -> RestartStrategy {
        match self.level {
            ServiceLevel::User => match std::env::consts::OS {
                "windows" => RestartStrategy::ScheduledTask,
                "macos" => RestartStrategy::Launchd,
                "linux" => RestartStrategy::Systemd,
                _ => RestartStrategy::None,
            },
            ServiceLevel::System => match std::env::consts::OS {
                "windows" => RestartStrategy::ScmFailureActions,
                "macos" => RestartStrategy::Launchd,
                "linux" => RestartStrategy::Systemd,
                _ => RestartStrategy::None,
            },
        }
    }

    /// 校验组合是否合法。
    ///
    /// 两条规则：
    /// - **开机启动（[`Autostart::Boot`]）必须系统级**：用户级机制（计划任务 `ONLOGON`、
    ///   LaunchAgent、systemd `--user`）都绑在登录会话上，写「开机启动」是骗用户；
    /// - **系统级的「登录时启动」（[`Autostart::Logon`]）没有意义**：系统服务本来就不依赖登录。
    pub fn validate(&self) -> Result<(), ServiceError> {
        if self.binary_path.as_os_str().is_empty() {
            return Err(ServiceError::invalid_options(
                "binary_path 是空的，注册一个空路径等于装了个永远不会启动的服务",
            ));
        }
        if self.name.trim().is_empty() {
            return Err(ServiceError::invalid_options("服务名不能为空"));
        }
        match (self.level, self.autostart) {
            (ServiceLevel::User, Autostart::Boot) => Err(ServiceError::invalid_options(
                "用户级安装做不到「开机即起」：它绑在登录会话上。要开机即起请用系统级（会弹一次提权）",
            )),
            (ServiceLevel::System, Autostart::Logon) => Err(ServiceError::invalid_options(
                "系统级服务不依赖登录，请用 Autostart::Boot 或 Autostart::Off",
            )),
            _ => Ok(()),
        }
    }
}

/// 「把自家的 daemon 装成服务」的全部能力。
///
/// 实现只有三个（`windows` / `macos` / `linux` 三个平台模块），
/// 由 [`crate::native`] 按平台挑一个给调用方。
///
/// 约定：
/// - **读类方法**（[`ServiceHost::status`]、[`ServiceHost::verify_restart_policy`]）不改机器，
///   测试可以随便跑；
/// - **写类方法**（install / uninstall / start / stop / set_autostart）走 [`crate::runner::Runner`]，
///   测试注入 [`crate::runner::FakeRunner`] 断言生成的命令行与文件正文，一次都不真执行；
/// - 「已安装但未运行」是**正常状态**（`service-lifecycle.md § 1`），不是错误。
pub trait ServiceHost: Send + Sync {
    /// 读注册状态（已注册？在跑？自启？失败重启策略写进去了吗？）。
    ///
    /// 没安装时返回 [`ServiceStatus::not_installed`]，**不返回错误** ——
    /// GUI 要能靠这一个调用画出四种组合（已注册 × 控制面可连）。
    fn status(&self) -> Result<ServiceStatus, ServiceError>;

    /// 注册服务 / 计划任务，并写入失败重启策略。
    ///
    /// 实现内部会调 [`ServiceHost::verify_restart_policy`] 自检
    /// （`service-lifecycle.md § 3` 第 9a 步）：策略没写进去就报错，而不是假装装好了。
    fn install(&self, opts: &InstallOptions) -> Result<(), ServiceError>;

    /// 反注册。
    ///
    /// ⚠️ 调用方应当**先** [`ServiceHost::stop`]：Windows 上 `DeleteService` 只是打标记，
    /// 正在跑的进程还会继续跑（表现为「卸载了但扩展还能收信」）。
    fn uninstall(&self) -> Result<(), ServiceError>;

    /// 启动。
    ///
    /// 「已经在跑」应当在实现里被当成成功（幂等）：用户点两次、看门狗拉一次、
    /// 自更新后重启一次，都是常态。
    fn start(&self) -> Result<(), ServiceError>;

    /// 停止。没在跑时也返回成功（幂等）。
    fn stop(&self) -> Result<(), ServiceError>;

    /// 开 / 关自启。
    fn set_autostart(&self, on: bool) -> Result<(), ServiceError>;

    /// 安装后自检（`service-lifecycle.md § 3` 第 9a 步）：**失败重启策略是否真的写进去了**。
    ///
    /// 各平台的读法不同，但都必须真的把注册信息读回来解析：
    ///
    /// | 平台 | 读法 | 判定 |
    /// | --- | --- | --- |
    /// | Windows 用户级 | `schtasks /Query /XML` | 有 `<RestartOnFailure>` 且间隔 / 次数非零 |
    /// | Windows 系统级 | `sc.exe qfailure` | 有重启动作、延迟 ≤ 5 秒（更新重启能在扩展察觉前恢复） |
    /// | Linux | `systemctl show -p Restart -p RestartUSec` | `Restart=` 是 `always` / `on-failure` |
    /// | macOS | `launchctl print <domain>/<label>` 或 plist 正文 | `KeepAlive` 存在 |
    ///
    /// 为什么不是形式主义：不同平台的失败重启配置项很容易写漏
    /// （`sc create` 就根本写不了 failure actions），而它的失效只在「服务崩了」时才暴露。
    fn verify_restart_policy(&self) -> Result<(), ServiceError>;
}
