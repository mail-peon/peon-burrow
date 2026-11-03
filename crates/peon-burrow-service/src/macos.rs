//! macOS：用户级走 **LaunchAgent**，系统级走 **LaunchDaemon**。
//!
//! | 级别 | 落盘位置 | 拉起方式 |
//! | --- | --- | --- |
//! | [`ServiceLevel::User`]（默认） | `~/Library/LaunchAgents/<label>.plist` | `launchctl bootstrap gui/<uid>` + `kickstart` |
//! | [`ServiceLevel::System`] | `/Library/LaunchDaemons/<label>.plist` | `launchctl bootstrap system` |
//!
//! 崩溃恢复靠 plist 里的 `KeepAlive`（`SuccessfulExit=false`）：
//! **只对「非正常退出」生效** —— 这正是自更新要用非零退出码结束的原因
//! （`service-lifecycle.md § 5`）。
//!
//! ⚠️ `KeepAlive` 里**不能**同时写 `SuccessfulExit=false` 和 `Crashed=true`：
//! 后者在旧版本上会被忽略，而且语义重叠。只写一个。
//!
//! ⚠️ 为什么先做 plist 而不是 macOS 13+ 的 `SMAppService`：
//! plist 兼容面大（`adr-0003` 的「后续」第 1 条已定），
//! `SMAppService` 的优势是能在「系统设置 → 登录项」里正确显示，属于后续优化。
//!
//! ⚠️ `launchctl kickstart` 用 `-k`（先杀再起）还是不带 `-k`：
//! 这里用**不带** `-k`，因为「启动一个已经在跑的」应当是无害的幂等操作
//! （`service-lifecycle.md § 1`：「启动被调用而它已经在跑」是常态）。

use crate::host::{RunMode, ServiceLevel};
use std::path::PathBuf;

/// macOS 的路径约定（`service-lifecycle.md § 3` 第 2 步）。
pub fn default_install_dir(level: ServiceLevel) -> Option<PathBuf> {
    match level {
        ServiceLevel::User => std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join("Library/Application Support/peon-burrow/bin")),
        ServiceLevel::System => Some(PathBuf::from("/usr/local/libexec/peon-burrow")),
    }
}

/// 运行形态说明（日志用）。
pub fn describe_run_mode(mode: RunMode) -> &'static str {
    match mode {
        RunMode::Service => "launchd 形态（无终端，日志落盘）",
        RunMode::Foreground => "前台形态（有终端）",
    }
}

mod paths;
pub use paths::{
    LAUNCH_AGENT_DIR, LAUNCH_DAEMON_DIR, domain, label_of, launch_agent_path, launch_daemon_path,
    plist_path,
};

mod plist;
pub use plist::{
    LaunchdState, inspect_binary_path, inspect_keep_alive, inspect_launchd, inspect_run_at_load,
    plist_body, restart_policy_description, with_run_at_load, xml_escape,
};

mod host;
pub use host::MacOsHost;

#[cfg(test)]
mod tests;
