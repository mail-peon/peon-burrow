//! 跨平台服务托管：注册、启停、自启、**失败重启自检**、端口探测。
//!
//! **与中继无关** —— 任何「要把自己的 daemon 装成服务」的项目都能用：
//!
//! ```no_run
//! use peon_burrow_service::{native_for, Autostart, InstallOptions, ServiceHost, ServiceLevel};
//!
//! # fn main() -> Result<(), peon_burrow_service::ServiceError> {
//! // 默认：用户级自启（零提权）。装一次、重启机器、什么都不用管。
//! let host = native_for(ServiceLevel::User);
//! let opts = InstallOptions::new("my-daemon", "/home/me/.local/bin/my-daemon");
//! host.install(&opts)?;
//! # Ok(())
//! # }
//! ```
//!
//! 只想**看**状态、不想动任何东西：
//!
//! ```
//! use peon_burrow_service::{probe_port, ServiceStatus};
//!
//! # fn main() -> Result<(), peon_burrow_service::ServiceError> {
//! let port = probe_port(41316)?;
//! if !port.free {
//!     println!("{}", port.describe(41316));
//! }
//! let _: ServiceStatus = ServiceStatus::not_installed("my-daemon");
//! # Ok(())
//! # }
//! ```
//!
//! # 两种安装级别
//!
//! | 级别 | 提权 | Windows | macOS | Linux |
//! | --- | --- | --- | --- | --- |
//! | [`ServiceLevel::User`]（**默认**） | 零 | 计划任务（`ONLOGON` + 失败重启） | LaunchAgent（`RunAtLoad` + `KeepAlive`） | `systemd --user`（`Restart=always`） |
//! | [`ServiceLevel::System`] | **一次** UAC / `sudo` | SCM（`sc.exe`） | LaunchDaemon | systemd system unit |
//!
//! 依据 [`adr-0003`](https://github.com/mail-peon/peon-burrow)：用户级是默认（零摩擦），
//! 系统服务是「无人登录的机器」这类场景的选项。
//!
//! # 四条使用上的硬约束
//!
//! 1. **注册的必须是安装目录里的二进制副本**，不是临时目录里双击运行的那个
//!    （`service-lifecycle.md § 3` 第 3 步：「装完看着挺好，重启就没了」的经典成因）。
//!    本 crate 提供 [`default_install_dir`] 给出各平台的约定路径，复制动作由调用方做 ——
//!    库不碰用户的安装目录。
//! 2. **安装后必须自检**：[`ServiceHost::install`] 内部已经调用了
//!    [`ServiceHost::verify_restart_policy`]，失败就报错 + 让调用方回滚。
//!    「崩了不回来」只有在真的崩一次时才暴露，所以这一步不能省。
//! 3. **卸载前先停止**：Windows 上 `sc delete` 只是打标记，正在跑的进程还会继续跑
//!    （表现为「卸载了但扩展还能收信」）。
//! 4. **自更新靠非零退出码重启**：进程以 `ExitCode::RestartRequested = 5` 结束，
//!    由平台调度器拉起来（SCM 的 failure actions、launchd 的 `SuccessfulExit=false`
//!    都只对非正常退出生效）。
//!
//! # 测试怎么做到「一次都不动机器」
//!
//! 每个平台模块都把动作拆成两半：
//!
//! - **生成**：纯函数（`windows::task_xml`、`macos::plist_body`、`linux::unit_body`）
//!   → 测试直接断言生成的**命令行、任务 XML、plist 正文、unit 正文**；
//! - **执行**：[`runner::Runner`] trait。生产用 [`runner::RealRunner`]（真的 `Command`），
//!   测试注入 [`runner::FakeRunner`]（只记录、不执行）。
//!
//! 平台宿主还有 `WindowsUserHost::with_runner` / `MacOsHost::with_runner` /
//! `LinuxHost::with_runner` 这样的注入点，
//! 让「安装 → 注册 → 自检」整条链路都能在假执行器上跑完。
//!
//! ⚠️ 上面这些平台模块名没法写成文档链接：它们在 `#[cfg(target_os = …)]` 后面，
//! 在别的平台上根本不存在，写链接会让 `cargo doc -D warnings` 直接失败。
//!
//! 只有 `status()` / `verify_restart_policy()` 的读取动作和 `probe_port()` 会碰系统
//! —— 它们本身就是只读的。
//!
//! # 平台动作对照
//!
//! | 动作 | Windows 用户级 | Windows 系统级 | macOS | Linux |
//! | --- | --- | --- | --- | --- |
//! | 安装 | `Register-ScheduledTask -Xml`（任务 XML 含 `Hidden`） | `sc create` + `sc failure` | 写 plist + `launchctl bootstrap` | 写 unit + `daemon-reload` + `enable` + `start` |
//! | 启动 | `schtasks /Run` | `sc start` | `launchctl kickstart` | `systemctl start` |
//! | 停止 | `schtasks /End` | `sc stop` | `launchctl bootout` | `systemctl stop` |
//! | 自启 | `/Change /Enable` \| `/Disable` | `sc config start= auto\|demand` | plist 的 `RunAtLoad` | `systemctl enable` \| `disable` |
//! | 卸载 | `schtasks /Delete /F` | `sc delete`（先停） | `bootout` + 删 plist | `stop` + `disable` + 删 unit + `daemon-reload` |
//! | 状态 | `schtasks /Query /XML` | `sc query` + `sc qfailure` | `launchctl print` + plist | `systemctl show` + `cat` |
//! | 崩溃恢复 | 任务 XML 的 `RestartOnFailure`（1 分钟 × 3） | `sc failure`（5 秒） | `KeepAlive` | `Restart=always` + `RestartSec=2` |

#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod host;
pub mod probe;
pub mod runner;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

pub use host::{
    Autostart, DEFAULT_SERVICE_NAME, InstallOptions, Plan, RESTART_INTERVAL_SECS,
    RESTART_RETRY_COUNT, RestartStrategy, RunMode, ServiceError, ServiceHost, ServiceLevel,
    ServiceStatus,
};
pub use probe::{
    DEFAULT_PORT, IpAddr, PROBE_HOST, PortStatus, parse_lsof_pids, parse_netstat_listeners,
    parse_powershell_pid, probe_addr, probe_port,
};
pub use runner::{CommandSpec, FakeRunner, FileInstall, Mutation, Mutations, RealRunner, Runner};

/// 平台默认实现：按**安装级别**挑一个宿主。
///
/// 用户级（[`ServiceLevel::User`]）是默认选择 —— 零提权，见 [`adr-0003`]。
///
/// [`adr-0003`]: https://github.com/mail-peon/peon-burrow/blob/main/ai-docs/decisions/adr-0003-service-model.md
pub fn native_for(level: ServiceLevel) -> Box<dyn ServiceHost> {
    #[cfg(target_os = "windows")]
    {
        match level {
            ServiceLevel::User => Box::new(windows::WindowsUserHost::new()) as Box<dyn ServiceHost>,
            ServiceLevel::System => {
                Box::new(windows::WindowsSystemHost::new()) as Box<dyn ServiceHost>
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::MacOsHost::new(DEFAULT_SERVICE_NAME, level)) as Box<dyn ServiceHost>
    }
    #[cfg(target_os = "linux")]
    {
        Box::new(linux::LinuxHost::new(DEFAULT_SERVICE_NAME, level)) as Box<dyn ServiceHost>
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        let _ = level;
        Box::new(UnsupportedHost) as Box<dyn ServiceHost>
    }
}

/// 平台默认实现（用户级，零提权）。
///
/// 等价于 [`native_for`]`(`[`ServiceLevel::User`]`)`。
/// 要装系统服务，显式调 [`native_for`]`(`[`ServiceLevel::System`]`)` ——
/// 「会弹 UAC」这件事必须由调用方明确决定，不能藏在默认值里。
pub fn native() -> Box<dyn ServiceHost> {
    native_for(ServiceLevel::User)
}

/// 各平台的安装目录约定（`service-lifecycle.md § 3` 第 2 步）。
///
/// | 平台 | 用户级 | 系统级 |
/// | --- | --- | --- |
/// | Windows | `%LOCALAPPDATA%\Programs\peon-burrow\` | `%ProgramFiles%\peon-burrow\` |
/// | macOS | `~/Library/Application Support/peon-burrow/bin/` | `/usr/local/libexec/peon-burrow/` |
/// | Linux | `~/.local/share/peon-burrow/bin/` | `/usr/local/libexec/peon-burrow/` |
///
/// ⚠️ 本 crate **只给约定**，复制二进制是产品层的事（库不去动用户的安装目录）。
/// 环境变量读不到时返回 `None`。
pub fn default_install_dir(level: ServiceLevel) -> Option<std::path::PathBuf> {
    #[cfg(target_os = "windows")]
    {
        windows::default_install_dir(level)
    }
    #[cfg(target_os = "macos")]
    {
        macos::default_install_dir(level)
    }
    #[cfg(target_os = "linux")]
    {
        linux::default_install_dir(level)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        let _ = level;
        None
    }
}

/// 三大平台之外的兜底宿主：每个动作都返回 [`ServiceError::UnsupportedPlatform`]。
///
/// 存在它的意义是**保持 API 形状**：`probe_port` / `RestartStrategy` 这些与平台无关的能力
/// 在其它系统上依然能用，调用方不必为「非三大平台」写两套代码。
#[derive(Debug, Clone, Copy, Default)]
pub struct UnsupportedHost;

impl ServiceHost for UnsupportedHost {
    fn status(&self) -> Result<ServiceStatus, ServiceError> {
        Ok(ServiceStatus::not_installed(DEFAULT_SERVICE_NAME))
    }

    fn install(&self, _opts: &InstallOptions) -> Result<(), ServiceError> {
        Err(self.unsupported())
    }

    fn uninstall(&self) -> Result<(), ServiceError> {
        Err(self.unsupported())
    }

    fn start(&self) -> Result<(), ServiceError> {
        Err(self.unsupported())
    }

    fn stop(&self) -> Result<(), ServiceError> {
        Err(self.unsupported())
    }

    fn set_autostart(&self, _on: bool) -> Result<(), ServiceError> {
        Err(self.unsupported())
    }

    fn verify_restart_policy(&self) -> Result<(), ServiceError> {
        Err(self.unsupported())
    }
}

impl UnsupportedHost {
    fn unsupported(&self) -> ServiceError {
        ServiceError::UnsupportedPlatform {
            os: std::env::consts::OS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_hosts_are_constructible_and_read_only_status_is_safe() {
        // 这里刻意**只**调 status()：它不改机器（`modules.md § 13`：服务层的集成测试只跑只读路径）。
        let host = native_for(ServiceLevel::User);
        let status = host.status().expect("读状态不该失败");
        assert_eq!(status.name, DEFAULT_SERVICE_NAME);

        let system = native_for(ServiceLevel::System);
        assert_eq!(
            system.status().map(|status| status.name).ok(),
            Some(DEFAULT_SERVICE_NAME.to_owned())
        );
    }

    #[test]
    fn native_is_the_user_level_default() {
        // native() 与 native_for(User) 必须是同一件事：系统级是**显式**选择。
        assert_eq!(
            native().status().map(|status| status.level).ok(),
            native_for(ServiceLevel::User)
                .status()
                .map(|status| status.level)
                .ok()
        );
    }

    #[test]
    fn install_options_defaults_are_the_zero_elevation_path() {
        let opts = InstallOptions::new("demo", "/tmp/demo");
        assert_eq!(opts.level, ServiceLevel::User);
        assert_eq!(opts.autostart, Autostart::Logon);
        assert_eq!(opts.mode, RunMode::Service);
        assert_eq!(opts.args, vec!["run".to_owned()]);
        opts.validate().expect("默认组合必须合法");
    }

    #[test]
    fn install_options_reject_combinations_that_lie_to_the_user() {
        let mut opts = InstallOptions::new("demo", "/tmp/demo");
        // 「用户级 + 开机启动」做不到（绑登录会话），必须明确拒绝而不是假装做到
        opts.autostart = Autostart::Boot;
        let error = opts.validate().expect_err("用户级 + Boot 必须被拒");
        assert!(matches!(error, ServiceError::InvalidOptions { .. }));
        assert!(
            error.action().contains("--level system"),
            "{}",
            error.action()
        );

        // 「系统级 + 登录启动」没有意义
        let mut system = InstallOptions::new("demo", "/tmp/demo");
        system.level = ServiceLevel::System;
        assert!(system.validate().is_err());

        // 空路径 / 空名字
        assert!(InstallOptions::new("demo", "").validate().is_err());
        assert!(InstallOptions::new("   ", "/tmp/demo").validate().is_err());
    }

    #[test]
    fn restart_strategy_follows_the_level() {
        let user = InstallOptions::new("demo", "/tmp/demo");
        assert_eq!(
            user.restart_strategy(),
            match std::env::consts::OS {
                "windows" => RestartStrategy::ScheduledTask,
                "macos" => RestartStrategy::Launchd,
                "linux" => RestartStrategy::Systemd,
                _ => RestartStrategy::None,
            }
        );
        assert!(!user.restart_strategy().description().is_empty());

        let mut system = InstallOptions::new("demo", "/tmp/demo");
        system.level = ServiceLevel::System;
        assert_eq!(
            system.restart_strategy(),
            match std::env::consts::OS {
                "windows" => RestartStrategy::ScmFailureActions,
                "macos" => RestartStrategy::Launchd,
                "linux" => RestartStrategy::Systemd,
                _ => RestartStrategy::None,
            }
        );
    }

    #[test]
    fn error_messages_are_actionable() {
        // 文案规范（logging-and-diagnostics.md § 4）：说人话 + 给下一步
        let error = ServiceError::NotInstalled {
            name: "demo".to_owned(),
        };
        assert!(error.to_string().contains("demo"));
        assert!(!error.to_string().contains("EADDRINUSE"));
        assert!(error.action().contains("burrow service install"));
        assert!(error.user_message().contains("下一步："));

        let failed = ServiceError::command_failed("注册服务", Some(1060), "指定的服务未安装");
        let text = failed.to_string();
        assert!(text.contains("注册服务"));
        assert!(
            text.contains("1060"),
            "原始退出码要保留（可复制给工单）：{text}"
        );
        assert!(text.contains("指定的服务未安装"), "原始输出不能吞掉");
    }
}
