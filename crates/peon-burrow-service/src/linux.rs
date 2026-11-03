//! Linux：**systemd user / system**（退路是 XDG autostart）。
//!
//! | 级别 | unit 文件 | 命令 |
//! | --- | --- | --- |
//! | [`ServiceLevel::User`]（默认） | `~/.config/systemd/user/<name>.service` | `systemctl --user …` |
//! | [`ServiceLevel::System`] | `/etc/systemd/system/<name>.service` | `systemctl …` |
//!
//! 崩溃恢复靠 unit 里的 `Restart=always` + `RestartSec=2`（`adr-0003 § 1`）。
//! ⚠️ 那两行**必须显式写**：systemd 的默认 `Restart=no`，
//! 少写一行就等于「崩了就一直躺着」，而这只有真的崩一次才看得出来 ——
//! 所以 [`crate::ServiceHost::verify_restart_policy`] 要把 unit 读回来解析。
//!
//! # XDG autostart 退路（**已文档化，未实现**）
//!
//! 没有 systemd 的机器（Alpine、部分容器、WSL 的某些配置）上，用户级自启只能靠
//! `~/.config/autostart/<name>.desktop`（XDG Desktop Application Autostart 规范）。
//! 路径由 [`xdg_autostart_path`] 给出，`.desktop` 正文模板由 [`xdg_autostart_body`] 给出
//! —— 两者都是纯函数，可直接单测；但**本 crate 不自动切换到这条退路**：
//!
//! | 为什么不做自动切换 |
//! | --- |
//! | `.desktop` 只能由桌面环境在登录时拉起，**没有失败重启**能力（没有 `Restart=` 的对应物）→ 恰好违背 [`crate::ServiceHost::verify_restart_policy`] 的验收要求 |
//! | 「有没有 systemd」需要探测（`systemctl --user is-system-running`），而探测结果会影响用户看到的状态，属于产品层决策 |
//!
//! 结论：调用方自己决定什么时候用退路，并用 `doctor` 把「你现在没有崩溃恢复」明确告诉用户。

use crate::host::{RunMode, ServiceLevel};
use std::path::PathBuf;

/// Linux 的路径约定（`service-lifecycle.md § 3` 第 2 步）。
pub fn default_install_dir(level: ServiceLevel) -> Option<PathBuf> {
    match level {
        ServiceLevel::User => std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(".local/share/peon-burrow/bin")),
        ServiceLevel::System => Some(PathBuf::from("/usr/local/libexec/peon-burrow")),
    }
}

/// 运行形态说明（日志用）。
pub fn describe_run_mode(mode: RunMode) -> &'static str {
    match mode {
        RunMode::Service => "systemd 形态（无终端，日志落盘）",
        RunMode::Foreground => "前台形态（有终端）",
    }
}

mod unit;
pub use unit::{
    RESTART_SECS, UNIT_SUFFIX, is_valid_unit_name, unit_body, unit_name, unit_path,
    xdg_autostart_body, xdg_autostart_path,
};

mod inspect;
pub use inspect::{
    SystemdState, inspect_exec_start, inspect_restart_line, inspect_restart_properties,
    inspect_systemd_state,
};

mod host;
pub use host::LinuxHost;

#[cfg(test)]
mod tests;
