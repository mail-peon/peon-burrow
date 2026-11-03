use crate::host::ServiceLevel;
use crate::runner::{CommandSpec, Runner};
use std::path::PathBuf;

/// 用户级 LaunchAgent 目录（相对 `$HOME`）。
pub const LAUNCH_AGENT_DIR: &str = "Library/LaunchAgents";

/// 系统级 LaunchDaemon 目录。
pub const LAUNCH_DAEMON_DIR: &str = "/Library/LaunchDaemons";

/// launchd 的标签（plist 文件名 = `<label>.plist`，`launchctl` 也按它找服务）。
///
/// macOS 惯例是反向域名；本 crate 与中继无关，用 `<name>` 直接作标签，
/// 调用方要 namespace 就在 `name` 里带上（例如 `cn.example.my-daemon`）。
pub fn label_of(name: &str) -> String {
    name.to_owned()
}

/// 用户级 plist 的完整路径。
///
/// `$HOME` 读不到时返回 `None`（用 `directories` 会多一个依赖，
/// 而调用方本来就该在自己的组装层决定路径 —— 布局铁律 L4「注入而非全局」）。
pub fn launch_agent_path(name: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(LAUNCH_AGENT_DIR)
            .join(format!("{}.plist", label_of(name))),
    )
}

/// 系统级 plist 的完整路径。
pub fn launch_daemon_path(name: &str) -> PathBuf {
    PathBuf::from(LAUNCH_DAEMON_DIR).join(format!("{}.plist", label_of(name)))
}

/// 落盘位置（按级别）。
pub fn plist_path(name: &str, level: ServiceLevel) -> Option<PathBuf> {
    match level {
        ServiceLevel::User => launch_agent_path(name),
        ServiceLevel::System => Some(launch_daemon_path(name)),
    }
}

/// `launchctl` 的 domain 参数。
///
/// - 用户级：`gui/<uid>`（**不是** `user/<uid>`：GUI 会话里的 agent 用 `gui` domain，
///   而 `user` domain 不会加载 `RunAtLoad`）；
/// - 系统级：`system`。
///
/// uid 让系统自己算（`id -u`），不猜、不从环境变量编。
pub fn domain(level: ServiceLevel) -> String {
    match level {
        ServiceLevel::User => format!("gui/{}", current_uid()),
        ServiceLevel::System => "system".to_owned(),
    }
}

/// 当前 uid：`id -u`。
///
/// 直接用 `id` 而不是 `libc::getuid()`：本 crate 的依赖白名单里没有 `libc`，
/// 而 `id -u` 在 macOS 上一定有。读不到时退成 `0`（系统级语义），
/// 并让后续 `launchctl` 自己报「找不到服务」—— 不抛一个用户看不懂的错误。
fn current_uid() -> u32 {
    let spec = CommandSpec::new("id", ["-u"], "读取当前用户 uid");
    crate::runner::RealRunner
        .run(&spec)
        .ok()
        .and_then(|stdout| stdout.trim().parse().ok())
        .unwrap_or(0)
}
