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

use std::path::PathBuf;

use crate::host::{
    Autostart, InstallOptions, Plan, RESTART_RETRY_COUNT, RunMode, ServiceError, ServiceHost,
    ServiceLevel, ServiceStatus,
};
use crate::runner::{CommandSpec, FileInstall, Runner};

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

/// 生成 plist 正文（**纯函数**，测试直接断言）。
///
/// 关键字段：
/// - `ProgramArguments` —— 二进制 + 参数（launchd 不做 shell 展开）；
/// - `RunAtLoad` —— 登录 / 加载时启动（自启）；
/// - `KeepAlive.SuccessfulExit = false` —— **失败重启策略**，也是 `verify_restart_policy` 读的那一项；
/// - `ProcessType = Background` —— 告诉 launchd 这是后台任务（便于它安排资源）；
/// - `ThrottleInterval` —— 崩溃循环时的最小重启间隔，避免把 CPU 打满。
pub fn plist_body(opts: &InstallOptions) -> String {
    let arguments = plist_program_arguments(opts);
    let run_at_load = opts.autostart != Autostart::Off;
    let description = format!(
        "peon-burrow relay ({})",
        match opts.mode {
            RunMode::Service => "service",
            RunMode::Foreground => "foreground",
        }
    );

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
{arguments}    </array>
    <key>RunAtLoad</key>
    <{run_at_load}/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ThrottleInterval</key>
    <integer>5</integer>
    <key>ProcessType</key>
    <string>Background</string>
    <key>WorkingDirectory</key>
    <string>{working_dir}</string>
    <!-- {description} -->
</dict>
</plist>
"#,
        label = xml_escape(&label_of(&opts.name)),
        run_at_load = if run_at_load { "true" } else { "false" },
        description = description,
        working_dir = xml_escape(
            &opts
                .binary_path
                .parent()
                .map(|parent| parent.display().to_string())
                .unwrap_or_default()
        ),
    )
}

/// plist 的 `ProgramArguments` 数组元素（第一项 = 二进制，后面是参数）。
fn plist_program_arguments(opts: &InstallOptions) -> String {
    let mut arguments = format!(
        "        <string>{}</string>\n",
        xml_escape(&opts.binary_path.display().to_string())
    );
    for arg in &opts.args {
        arguments.push_str(&format!("        <string>{}</string>\n", xml_escape(arg)));
    }
    arguments
}

/// XML 文本转义（路径可能带 `&`）。
pub fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 从 plist 正文里读 `RunAtLoad` 的真实值（`<true/>` / `<false/>`）。
pub fn inspect_run_at_load(plist: &str) -> Option<bool> {
    let value = value_after_key(plist, "RunAtLoad")?;
    parse_plist_bool(value)
}

/// 从 plist 正文里读 `KeepAlive`：只要这个键存在（字典或 `<true/>`）就算配上了。
pub fn inspect_keep_alive(plist: &str) -> bool {
    match value_after_key(plist, "KeepAlive") {
        Some(value) => {
            let trimmed = value.trim_start();
            trimmed.starts_with("<dict>") || parse_plist_bool(trimmed) == Some(true)
        }
        None => false,
    }
}

/// 从 plist 正文里读 `ProgramArguments` 的第一个 `<string>`（也就是二进制路径）。
pub fn inspect_binary_path(plist: &str) -> Option<String> {
    let array = value_after_key(plist, "ProgramArguments")?;
    let open = "<string>";
    let close = "</string>";
    let start = array.find(open)? + open.len();
    let end = array[start..].find(close)? + start;
    let value = array[start..end].trim();
    (!value.is_empty()).then(|| value.to_owned())
}

/// 取 `<key>name</key>` 后面的那一小段（够读到 `<true/>`、`<dict>` 或 `<string>x</string>`）。
fn value_after_key<'a>(plist: &'a str, key: &str) -> Option<&'a str> {
    let marker = format!("<key>{key}</key>");
    let start = plist.find(&marker)? + marker.len();
    Some(&plist[start..])
}

fn parse_plist_bool(value: &str) -> Option<bool> {
    let head = value.trim_start();
    if head.starts_with("<true/>") {
        Some(true)
    } else if head.starts_with("<false/>") {
        Some(false)
    } else {
        None
    }
}

/// `launchctl print` 的输出里我们关心的部分。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LaunchdState {
    /// launchd 认识这个服务（`launchctl print` 成功了）。
    pub loaded: bool,
    /// 有活着的进程。
    pub running: bool,
    /// 进程号（拿得到时）。
    pub pid: Option<u32>,
}

/// 解析 `launchctl print gui/<uid>/<label>` 的输出。
///
/// 真实输出（macOS 13/14，节选；原文用 TAB 缩进，这里换成空格以免踩到 `tabs_in_doc_comments`）：
///
/// ```text
/// gui/501/cn.example.relay = {
///     active count = 1
///     path = /Users/me/Library/LaunchAgents/cn.example.relay.plist
///     state = running
///
///     program = /Users/me/.local/share/peon-burrow/bin/burrow
///     arguments = {
///         /Users/me/.local/share/peon-burrow/bin/burrow
///         run
///     }
///     pid = 4242
/// }
/// ```
///
/// ⚠️ 没有 `state = running` 但文件已加载 = 「已安装·未运行」，是**正常状态**
/// （`service-lifecycle.md § 1`），不是错误。
pub fn inspect_launchd(stdout: &str) -> LaunchdState {
    let mut state = LaunchdState {
        loaded: true,
        ..LaunchdState::default()
    };
    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix("state = ") {
            state.running = value.trim() == "running";
        } else if let Some(value) = trimmed.strip_prefix("pid = ") {
            state.pid = value.trim().parse().ok();
            // 有 pid 就是有活着的进程
            state.running = true;
        } else if let Some(value) = trimmed.strip_prefix("active count = ")
            && value.trim().parse::<u32>().is_ok_and(|count| count > 0)
        {
            state.running = true;
        }
    }
    state
}

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

/// macOS 宿主（用户级 = LaunchAgent，系统级 = LaunchDaemon）。
#[derive(Debug)]
pub struct MacOsHost {
    runner: Box<dyn Runner>,
    name: String,
    level: ServiceLevel,
}

impl MacOsHost {
    /// 用户级（默认）+ 真执行器。
    pub fn user() -> Self {
        Self::new(crate::host::DEFAULT_SERVICE_NAME, ServiceLevel::User)
    }

    /// 系统级 + 真执行器。
    pub fn system() -> Self {
        Self::new(crate::host::DEFAULT_SERVICE_NAME, ServiceLevel::System)
    }

    /// 指定级别 + 真执行器。
    pub fn new(name: impl Into<String>, level: ServiceLevel) -> Self {
        Self::with_runner(name, level, crate::runner::RealRunner)
    }

    /// 指定级别 + 注入的执行器（**测试用**）。
    pub fn with_runner(
        name: impl Into<String>,
        level: ServiceLevel,
        runner: impl Runner + 'static,
    ) -> Self {
        Self {
            runner: Box::new(runner),
            name: name.into(),
            level,
        }
    }

    /// 服务名。
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 安装级别。
    pub fn level(&self) -> ServiceLevel {
        self.level
    }

    /// plist 路径（`$HOME` 读不到时返回 `None`）。
    pub fn plist_path(&self) -> Option<PathBuf> {
        plist_path(&self.name, self.level)
    }

    fn runner(&self) -> &dyn Runner {
        self.runner.as_ref()
    }

    /// `launchctl print <domain>/<label>`。
    fn print(&self) -> Result<String, ServiceError> {
        let spec = CommandSpec::new(
            "launchctl",
            [
                "print".to_owned(),
                format!("{}/{}", domain(self.level), label_of(&self.name)),
            ],
            "读取 launchd 里的服务状态",
        );
        crate::host::read_only(self.runner(), &spec)
    }

    /// 把 plist 读回来（`launchctl print` 读不到 `KeepAlive`，自检要直接看正文）。
    fn read_plist(&self) -> Result<String, ServiceError> {
        let path = self.plist_path().ok_or_else(|| {
            ServiceError::invalid_options("读不到 $HOME，无法定位 LaunchAgent 的 plist 路径")
        })?;
        let spec = CommandSpec::new(
            "cat",
            [path.display().to_string()],
            "读取 LaunchAgent 的 plist 正文",
        );
        crate::host::read_only(self.runner(), &spec)
    }

    /// 用户级要 `bootstrap gui/<uid>`；系统级 `bootstrap system`。
    fn bootstrap(&self) -> (CommandSpec, CommandSpec) {
        let path = self
            .plist_path()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let target = format!("{}/{}", domain(self.level), label_of(&self.name));
        (
            CommandSpec::new(
                "launchctl",
                ["bootstrap".to_owned(), domain(self.level), path],
                "把 plist 加载进 launchd",
            ),
            CommandSpec::new(
                "launchctl",
                ["kickstart".to_owned(), target],
                "让 launchd 立刻启动这个服务",
            ),
        )
    }

    /// `launchctl bootout <domain>/<label>`（停止并卸载）。
    fn bootout(&self) -> CommandSpec {
        CommandSpec::new(
            "launchctl",
            [
                "bootout".to_owned(),
                format!("{}/{}", domain(self.level), label_of(&self.name)),
            ],
            "把服务从 launchd 卸载（停止）",
        )
    }

    /// `launchctl print` 成功 = launchd 认识这个服务。
    fn is_loaded(&self) -> bool {
        self.print().is_ok()
    }

    /// plist 文件在不在（`cat` 得动 = 在）。
    fn plist_exists(&self) -> bool {
        self.read_plist().is_ok()
    }
}

impl ServiceHost for MacOsHost {
    fn status(&self) -> Result<ServiceStatus, ServiceError> {
        // 用户级定位不到路径（读不到 $HOME）= 没装过；
        // 系统级路径是固定的，永远能算出来。
        if self.plist_path().is_none() {
            return Ok(ServiceStatus::not_installed(&self.name));
        }

        // 权威来源是 launchd；plist 文件只是第二证据（`port-and-discovery.md § 6`）。
        let (installed, mut state, mut binary_path) = match self.print() {
            Ok(stdout) => {
                let state = inspect_launchd(&stdout);
                (state.loaded, state, None)
            }
            Err(_) => (false, LaunchdState::default(), None),
        };

        // plist 存在也算已安装（「已安装·未运行」是正常状态）。
        let plist = self.read_plist().ok();
        let installed = installed || plist.is_some();
        if !installed {
            return Ok(ServiceStatus::not_installed(&self.name));
        }
        state.loaded = installed;

        let autostart = plist
            .as_deref()
            .and_then(inspect_run_at_load)
            .map(|on| if on { Autostart::Logon } else { Autostart::Off })
            .unwrap_or(Autostart::Off);
        if binary_path.is_none() {
            binary_path = plist.as_deref().and_then(inspect_binary_path);
        }

        Ok(ServiceStatus {
            installed: true,
            running: state.running,
            level: Some(self.level),
            autostart: Some(autostart),
            name: self.name.clone(),
            binary_path,
            // 系统级要 sudo；用户级零提权。
            requires_elevation: self.level == ServiceLevel::System,
            restart_policy_configured: plist.as_deref().is_some_and(inspect_keep_alive),
            last_exit_code: None,
        })
    }

    fn install(&self, opts: &InstallOptions) -> Result<(), ServiceError> {
        crate::host::prepare_install(opts, self.level)?;

        let path = self.plist_path().ok_or_else(|| {
            ServiceError::invalid_options("读不到 $HOME，无法定位 LaunchAgent 的 plist 路径")
        })?;
        let file = FileInstall::new(
            path,
            plist_body(opts),
            if self.level == ServiceLevel::User {
                "写 LaunchAgent plist（登录自启 + KeepAlive）"
            } else {
                "写 LaunchDaemon plist（开机启动 + KeepAlive）"
            },
        );
        let (bootstrap, kickstart) = self.bootstrap();

        Plan::new()
            .file(file)
            .command(bootstrap)
            .command(kickstart)
            .execute(self.runner())?;

        tracing::info!(
            event = "service.install",
            mode = ?self.level,
            autostart = ?opts.autostart,
            path = %opts.binary_path.display(),
            "已注册 launchd 服务"
        );

        self.verify_restart_policy()
    }

    fn uninstall(&self) -> Result<(), ServiceError> {
        let Some(path) = self.plist_path() else {
            return Ok(());
        };

        // 顺序很重要：先 bootout（停止 + 卸载），再删 plist。
        // 已经没加载时 bootout 会报错，忽略即可（卸载是幂等的）。
        let mut plan = Plan::new();
        if self.is_loaded() {
            plan = plan.command_ignoring_failure(self.bootout());
        }
        if self.plist_exists() {
            plan = plan.remove(path);
        }
        plan.execute(self.runner())?;
        tracing::info!(event = "service.uninstall", mode = ?self.level, "已删除 launchd 服务");
        Ok(())
    }

    fn start(&self) -> Result<(), ServiceError> {
        if self.is_loaded() {
            let target = format!("{}/{}", domain(self.level), label_of(&self.name));
            let spec = CommandSpec::new(
                "launchctl",
                ["kickstart".to_owned(), target],
                "启动 launchd 服务",
            );
            return Plan::new().command(spec).execute(self.runner());
        }
        // 没加载过：必须 bootstrap（`RunAtLoad=false` 时还要 kickstart）。
        let (bootstrap, kickstart) = self.bootstrap();
        Plan::new()
            .command(bootstrap)
            .command(kickstart)
            .execute(self.runner())
    }

    fn stop(&self) -> Result<(), ServiceError> {
        if !self.is_loaded() {
            return Ok(());
        }
        let spec = self.bootout();
        match Plan::new().command(spec).execute(self.runner()) {
            Ok(()) => Ok(()),
            Err(error) => {
                // 已经没了（竞态）不算失败。
                let text = error.to_string().to_lowercase();
                if text.contains("no such process") || text.contains("could not find service") {
                    Ok(())
                } else {
                    Err(error)
                }
            }
        }
    }

    fn set_autostart(&self, on: bool) -> Result<(), ServiceError> {
        let path = self.plist_path().ok_or_else(|| {
            ServiceError::invalid_options("读不到 $HOME，无法定位 LaunchAgent 的 plist 路径")
        })?;
        let plist = self.read_plist()?;
        let updated =
            with_run_at_load(&plist, on).ok_or_else(|| ServiceError::RestartPolicyMissing {
                name: self.name.clone(),
                expected: "<key>RunAtLoad</key>".to_owned(),
                found: "plist 里没有 RunAtLoad 键，无法开关自启".to_owned(),
            })?;

        let file = FileInstall::new(path, updated, "改写 plist 的 RunAtLoad（自启开关）");
        let mut plan = Plan::new().file(file);
        // 改完要重新加载才生效；没加载过就不用 reload（下次登录自然会读到新值）。
        if self.is_loaded() {
            let (bootstrap, _) = self.bootstrap();
            plan = plan.command(self.bootout()).command(bootstrap);
        }
        plan.execute(self.runner())
    }

    fn verify_restart_policy(&self) -> Result<(), ServiceError> {
        // plist 正文是 KeepAlive 的唯一真相；`launchctl print` 不打印它。
        let plist = self.read_plist()?;
        if inspect_keep_alive(&plist) {
            return Ok(());
        }
        Err(ServiceError::RestartPolicyMissing {
            name: label_of(&self.name),
            expected: "<key>KeepAlive</key>（SuccessfulExit=false，失败后自动重启）".to_owned(),
            found: plist
                .lines()
                .map(str::trim)
                .find(|line| line.contains("KeepAlive"))
                .unwrap_or("plist 里没有 KeepAlive")
                .to_owned(),
        })
    }
}

/// 改写 plist 里的 `RunAtLoad`（保留其余内容不动）。
///
/// 用文本改写而不是序列化整个 plist：调用方可能是**手改过**的 plist
/// （例如加了环境变量或 `StandardOutPath`），整篇重写会把用户的东西抹掉。
pub fn with_run_at_load(plist: &str, on: bool) -> Option<String> {
    let marker = "<key>RunAtLoad</key>";
    let index = plist.find(marker)?;
    let after = index + marker.len();
    let (start, end) = locate_plist_scalar(&plist[after..]).map(|(s, e)| (after + s, after + e))?;
    let replacement = if on { "<true/>" } else { "<false/>" };
    let mut updated = String::with_capacity(plist.len());
    updated.push_str(&plist[..start]);
    updated.push_str(replacement);
    updated.push_str(&plist[end..]);
    Some(updated)
}

/// 找 `<key>…</key>` 后面那个值的范围（返回相对于输入起点的偏移）。
fn locate_plist_scalar(value: &str) -> Option<(usize, usize)> {
    let trimmed_start = value.len() - value.trim_start().len();
    let head = &value[trimmed_start..];
    for tag in ["<true/>", "<false/>"] {
        if head.starts_with(tag) {
            return Some((trimmed_start, trimmed_start + tag.len()));
        }
    }
    None
}

/// 让 `RESTART_RETRY_COUNT` 在 macOS 上有意义（plist 的 `KeepAlive` 没有次数上限，
/// 由 launchd 自己节流；`ThrottleInterval` 就是那个节流间隔）。
///
/// 这个常量存在是为了让三平台共享同一份「失败重启」描述，
/// 自检时把它写进错误文案，便于用户对照 `service-lifecycle.md § 2` 的表格。
pub fn restart_policy_description() -> String {
    format!(
        "KeepAlive 常驻（launchd 不限次数，ThrottleInterval=5s 节流；参考其他平台：{} 次）",
        RESTART_RETRY_COUNT
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::FakeRunner;

    fn sample_opts() -> InstallOptions {
        let mut opts = InstallOptions::new(
            "cn.example.relay",
            "/Users/me/.local/share/peon-burrow/bin/burrow",
        );
        opts.args = vec!["run".to_owned()];
        opts
    }

    /// `launchctl print gui/501/cn.example.relay` 的真实形状（节选）。
    const PRINT_RUNNING: &str = "\
gui/501/cn.example.relay = {
\tactive count = 1
\tpath = /Users/me/Library/LaunchAgents/cn.example.relay.plist
\ttype = LaunchAgent
\tstate = running

\tprogram = /Users/me/.local/share/peon-burrow/bin/burrow
\targuments = {
\t\t/Users/me/.local/share/peon-burrow/bin/burrow
\t\trun
\t}
\tpid = 4242
}
";

    const PRINT_LOADED_IDLE: &str = "\
gui/501/cn.example.relay = {
\tactive count = 0
\tpath = /Users/me/Library/LaunchAgents/cn.example.relay.plist
\tstate = exited
}
";

    #[test]
    fn plist_carries_keep_alive_run_at_load_and_arguments() {
        let plist = plist_body(&sample_opts());
        assert!(
            plist.contains("<key>KeepAlive</key>"),
            "崩溃恢复靠 KeepAlive"
        );
        assert!(
            plist.contains("<key>SuccessfulExit</key>\n        <false/>"),
            "只对非正常退出生效 —— 自更新才能靠它重启"
        );
        assert!(plist.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert!(plist.contains("<string>/Users/me/.local/share/peon-burrow/bin/burrow</string>"));
        assert!(plist.contains("<string>run</string>"));
        assert!(plist.contains("<key>Label</key>\n    <string>cn.example.relay</string>"));
    }

    #[test]
    fn plist_autostart_off_flips_run_at_load_only() {
        let mut opts = sample_opts();
        opts.autostart = Autostart::Off;
        let plist = plist_body(&opts);
        assert!(plist.contains("<key>RunAtLoad</key>\n    <false/>"));
        assert_eq!(inspect_run_at_load(&plist), Some(false));
        assert!(inspect_keep_alive(&plist), "自启关掉不影响 KeepAlive");
    }

    #[test]
    fn plist_escapes_paths_with_ampersands() {
        let opts = InstallOptions::new("relay", "/Users/a & b/bin/burrow");
        let plist = plist_body(&opts);
        assert!(
            plist.contains("/Users/a &amp; b/bin/burrow"),
            "XML 必须转义"
        );
        assert_eq!(
            inspect_binary_path(&plist).as_deref(),
            Some("/Users/a &amp; b/bin/burrow"),
            "（读回来的是转义后的文本，调用方需要时自己反转义）"
        );
    }

    #[test]
    fn keep_alive_inspection_accepts_dict_and_true_but_not_false() {
        assert!(inspect_keep_alive(
            "<key>KeepAlive</key>\n<dict><key>SuccessfulExit</key><false/></dict>"
        ));
        assert!(inspect_keep_alive("<key>KeepAlive</key>\n<true/>"));
        assert!(!inspect_keep_alive("<key>KeepAlive</key>\n<false/>"));
        assert!(!inspect_keep_alive("<plist></plist>"));
    }

    #[test]
    fn with_run_at_load_rewrites_only_that_value() {
        let plist = plist_body(&sample_opts());
        let off = with_run_at_load(&plist, false).expect("有 RunAtLoad 就能改写");
        assert!(off.contains("<key>RunAtLoad</key>\n    <false/>"));
        assert!(
            off.contains("<key>EnvironmentVariables</key>")
                == plist.contains("<key>EnvironmentVariables</key>"),
            "其余内容必须原样保留"
        );
        assert_eq!(off.len(), plist.len(), "true/false 同长，长度不该变");
        assert!(inspect_keep_alive(&off));

        // 用户手改过的 plist（额外字段）不能被抹掉
        let custom = plist.replace(
            "<key>ThrottleInterval</key>",
            "<key>StandardOutPath</key>\n    <string>/tmp/x.log</string>\n    <key>ThrottleInterval</key>",
        );
        let rewritten = with_run_at_load(&custom, false).expect("改写");
        assert!(rewritten.contains("/tmp/x.log"), "用户加的行必须还在");
    }

    #[test]
    fn launchd_output_is_parsed_into_status() {
        let running = inspect_launchd(PRINT_RUNNING);
        assert!(running.loaded);
        assert!(running.running);
        assert_eq!(running.pid, Some(4242));

        let idle = inspect_launchd(PRINT_LOADED_IDLE);
        assert!(idle.loaded, "已加载 = 已安装");
        assert!(!idle.running, "「已安装·未运行」是正常状态");
        assert_eq!(idle.pid, None);
    }

    #[test]
    fn install_writes_plist_then_bootstraps_and_kickstarts() {
        let runner = FakeRunner::new()
            .expect("") // bootstrap
            .expect("") // kickstart
            .expect(plist_body(&sample_opts())); // 自检：cat plist

        let host = MacOsHost::with_runner("cn.example.relay", ServiceLevel::User, runner.clone());
        host.install(&sample_opts()).expect("install");

        let mutations = runner.mutations();
        assert_eq!(mutations.run_count(), 3);
        assert_eq!(mutations.files().len(), 1, "plist 必须先落盘");
        let lines = mutations.command_lines();
        assert!(
            lines[0].starts_with("launchctl bootstrap gui/"),
            "{}",
            lines[0]
        );
        assert!(
            lines[0].ends_with(".plist"),
            "bootstrap 要带上 plist 路径：{}",
            lines[0]
        );
        assert!(
            lines[1].starts_with("launchctl kickstart gui/"),
            "{}",
            lines[1]
        );
        assert!(
            lines[2].starts_with("cat "),
            "自检要读 plist 正文：{}",
            lines[2]
        );
        assert_eq!(runner.expectations_left(), 0);
    }

    #[test]
    fn install_fails_when_plist_has_no_keep_alive() {
        let mut plist = plist_body(&sample_opts());
        plist = plist.replace("<key>KeepAlive</key>", "<key>SomethingElse</key>");
        let runner = FakeRunner::new().expect("").expect("").expect(&plist);
        let host = MacOsHost::with_runner("cn.example.relay", ServiceLevel::User, runner);
        let error = host
            .install(&sample_opts())
            .expect_err("没有 KeepAlive 必须报错");
        assert!(matches!(error, ServiceError::RestartPolicyMissing { .. }));
        assert!(error.to_string().contains("KeepAlive"));
    }

    #[test]
    fn system_level_uses_launch_daemons_and_the_system_domain() {
        let runner = FakeRunner::new().expect("").expect("").expect("<plist/>");
        let host = MacOsHost::with_runner("cn.example.relay", ServiceLevel::System, runner.clone());
        let mut opts = sample_opts();
        opts.level = ServiceLevel::System;
        opts.autostart = Autostart::Boot;
        // 自检会因为没有 KeepAlive 失败，但我们要先看命令与路径
        let _ = host.install(&opts);

        let mutations = runner.mutations();
        match mutations.files().first() {
            Some(crate::runner::Mutation::Install(file)) => {
                assert_eq!(
                    file.path.display().to_string(),
                    "/Library/LaunchDaemons/cn.example.relay.plist"
                );
            }
            other => panic!("第 1 个动作应当是写 plist：{other:?}"),
        }
        let lines = mutations.command_lines();
        assert!(
            lines[0].starts_with("launchctl bootstrap system "),
            "{}",
            lines[0]
        );
        assert!(
            lines[1].starts_with("launchctl kickstart system/"),
            "{}",
            lines[1]
        );
    }

    #[test]
    fn user_level_requires_home() {
        // 不设 $HOME 时应当明确报错，而不是写到一个猜出来的路径
        let runner = FakeRunner::new();
        let host = MacOsHost::with_runner("relay", ServiceLevel::User, runner);
        if std::env::var_os("HOME").is_none() {
            let error = host
                .install(&sample_opts())
                .expect_err("没有 $HOME 必须报错");
            assert!(matches!(error, ServiceError::InvalidOptions { .. }));
        }
    }

    #[test]
    fn stop_is_idempotent_when_not_loaded() {
        let runner = FakeRunner::new().expect_failure("Could not find service \"relay\" in domain");
        let host = MacOsHost::with_runner("relay", ServiceLevel::User, runner);
        host.stop().expect("没加载时 stop 应当直接成功");
    }

    #[test]
    fn restart_policy_description_mentions_the_strategy() {
        assert!(restart_policy_description().contains("KeepAlive"));
    }
}
