//! Windows：用户级走**任务计划程序**，系统级走 **SCM**（`sc.exe`）。
//!
//! | 级别 | 机制 | 零提权 | 崩溃恢复 |
//! | --- | --- | --- | --- |
//! | [`ServiceLevel::User`]（默认） | 计划任务（`ONLOGON`） | ✅ | 任务的「失败后重新启动」（1 分钟 × 3 次） |
//! | [`ServiceLevel::System`] | SCM（`sc.exe create` / `sc.exe failure`） | ❌ 一次 UAC | `ServiceFailureActions`（重启 5 秒，重置 86400 秒） |
//!
//! # 为什么不用 `HKCU\…\Run`
//!
//! 中继是**控制台子系统**程序（`adr-0003 § 7`：不为服务单独构建 GUI 子系统二进制）。
//! `Run` 键在登录时会把它拉起来，于是弹出一个黑框窗口。
//! 计划任务能设「隐藏」，所以用户级自启走计划任务（`adr-0003 § 1`）。
//!
//! # 「隐藏」到底怎么实现（可核验）
//!
//! 任务 XML 的 `<Settings><Hidden>true</Hidden></Settings>` 是让**任务不在任务计划程序界面里
//! 显示**（经典含义）；对「控制台程序不弹窗」真正起作用的是**动作的窗口状态**：
//! `Register-ScheduledTask` 没有暴露命令行开关，所以这里生成 XML 时由
//! `Register-ScheduledTask` 的默认任务设置（`AllowDemandStart` + 无 `InteractiveToken` 窗口）
//! 兜底；同时用 `<LogonType>InteractiveToken</LogonType>` 而不是 `S4U`，
//! 避免「以 S4U 跑在会话 0 里、日志路径与用户目录对不上」这类更难查的问题。
//! 两者都写进 XML、都能在 `schtasks /Query /XML` 里核验 —— 这正是
//! `service-lifecycle.md § 3` 第 9a 步要求的「自检要能读回来」。
//!
//! ⚠️ 已知残余风险：Windows 10/11 上控制台宿主仍可能闪一次窗口。
//! 若在目标机器上复现，退路是 `Register-ScheduledTask -Settings` 里加
//! `-Hidden`，或改用 `win32` 的 `ITaskSettings::put_Hidden`（需要额外依赖，本 crate 不做）。
//!
//! # 为什么用 `Register-ScheduledTask` 而不是 `schtasks /Create /XML`
//!
//! `schtasks /XML` 的参数是**文件路径**，不是 XML 正文 —— 用它就必须先落一个临时文件
//! （Windows 临时目录会被清理，多一个失败点）。`powershell Register-ScheduledTask -Xml` 直接吃正文，
//! 少一个中间文件；而「命令与 XML 正文」都进了 [`crate::runner::Mutations`]，
//! 平台动作对照表要求的「注册任务 + 失败重启策略」一个都不少。

use crate::host::{RunMode, ServiceLevel};
use std::path::PathBuf;
use std::time::Duration;

/// 用户级任务的注册路径（任务计划程序里的「文件夹 \ 任务名」）。
pub const USER_TASK_FOLDER: &str = r"\peon-burrow";

/// 系统级服务的失败重启参数：延迟 5 秒（`service-lifecycle.md § 5`：重启间隔要够短）。
pub const SYSTEM_RESTART_DELAY_MS: u64 = 5_000;

/// 系统级服务的失败重启参数：失败计数重置周期 86400 秒。
pub const SYSTEM_RESTART_RESET_SECS: u32 = 86_400;

/// 停服务最多等多久（`sc stop` 只是**请求**，SCM 递完控制码就返回）。
///
/// 中继的关停是「先断开所有连接再 close」，实测亚秒级；10 秒足够覆盖慢机器，
/// 超过就说明它真的卡住了（那时必须报错，不能带着活进程去 `sc delete`）。
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// 等停时的轮询间隔。
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// 安装目录约定（`service-lifecycle.md § 3` 第 2 步）。
///
/// 本 crate 只提供**路径约定**，复制二进制由产品层做（库不碰用户的安装目录）。
pub fn default_install_dir(level: ServiceLevel) -> Option<PathBuf> {
    match level {
        ServiceLevel::User => std::env::var_os("LOCALAPPDATA")
            .map(|base| PathBuf::from(base).join("Programs").join("peon-burrow")),
        ServiceLevel::System => {
            std::env::var_os("ProgramFiles").map(|base| PathBuf::from(base).join("peon-burrow"))
        }
    }
}

/// 运行形态在 Windows 上的说明（日志用）。
pub fn describe_run_mode(mode: RunMode) -> &'static str {
    match mode {
        RunMode::Service => "计划任务 / SCM 形态（无终端，日志落盘）",
        RunMode::Foreground => "前台形态（有终端）",
    }
}

mod command;
pub use command::{
    command_line_of, current_user_id, exec_arguments_of, exec_command_of, restart_interval_iso8601,
    task_name, windows_command_line, xml_escape,
};

mod task;
pub use task::{
    inspect_autostart, inspect_command, inspect_last_result, parse_windows_program, task_xml,
    verify_task_xml, xml_section,
};

mod user;
pub use user::WindowsUserHost;

mod system;
pub use system::WindowsSystemHost;

mod scm;
pub use scm::{
    FailureActions, inspect_sc_autostart, inspect_sc_binary_path, parse_failure_actions,
    verify_failure_actions,
};

#[cfg(test)]
mod tests;
