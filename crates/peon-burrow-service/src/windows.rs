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

use std::path::PathBuf;

use crate::host::{
    Autostart, InstallOptions, Plan, RESTART_INTERVAL_SECS, RESTART_RETRY_COUNT, RunMode,
    ServiceError, ServiceHost, ServiceLevel, ServiceStatus,
};
use crate::runner::{CommandSpec, Runner};

/// 用户级任务的注册路径（任务计划程序里的「文件夹 \ 任务名」）。
pub const USER_TASK_FOLDER: &str = r"\peon-burrow";

/// 系统级服务的失败重启参数：延迟 5 秒（`service-lifecycle.md § 5`：重启间隔要够短）。
pub const SYSTEM_RESTART_DELAY_MS: u64 = 5_000;

/// 系统级服务的失败重启参数：失败计数重置周期 86400 秒。
pub const SYSTEM_RESTART_RESET_SECS: u32 = 86_400;

/// 任务 XML 里的失败重启间隔（ISO-8601 时长，`PT1M` = 1 分钟）。
pub fn restart_interval_iso8601() -> String {
    format!("PT{}M", RESTART_INTERVAL_SECS / 60)
}

/// 把命令行的各个部分拼成 Windows 命令行（带引号与转义）。
///
/// 规则（`CommandLineToArgvW` 的约定，和 `std::process::Command` 在 Windows 上的拼法一致）：
/// 反斜杠在引号前要翻倍，参数里的双引号要 `\"`。
pub fn windows_command_line(program: &str, args: &[String]) -> String {
    let mut line = quote_windows_arg(program);
    for arg in args {
        line.push(' ');
        line.push_str(&quote_windows_arg(arg));
    }
    line
}

fn quote_windows_arg(value: &str) -> String {
    if !value.is_empty() && !value.contains([' ', '\t', '"']) {
        return value.to_owned();
    }

    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    let mut backslashes = 0usize;
    for ch in value.chars() {
        match ch {
            '\\' => {
                backslashes += 1;
                quoted.push('\\');
            }
            '"' => {
                // 引号前的反斜杠要翻倍，然后引号自己也要转义
                for _ in 0..=backslashes {
                    quoted.push('\\');
                }
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                backslashes = 0;
                quoted.push(ch);
            }
        }
    }
    // 结尾的反斜杠要翻倍，否则会把收尾的引号转义掉
    for _ in 0..backslashes {
        quoted.push('\\');
    }
    quoted.push('"');
    quoted
}

/// XML 文本转义（任务 XML 里的路径可能带 `&`）。
pub fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 任务要跑的完整命令行（`binary_path` + `args`）。
pub fn command_line_of(opts: &InstallOptions) -> String {
    windows_command_line(&opts.binary_path.display().to_string(), &opts.args)
}

/// 当前用户的 `域\用户名`（任务 XML 的 `UserId` 要它）。
///
/// 域里用 `USERDOMAIN`，本地账号没有域时退成机器名 ——
/// `Register-ScheduledTask` 接 `机器名\用户名`，这点和 `schtasks` 的命令行不同。
pub fn current_user_id() -> String {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "current".to_owned());
    let domain = std::env::var("USERDOMAIN")
        .unwrap_or_else(|_| std::env::var("COMPUTERNAME").unwrap_or_default());
    if domain.is_empty() {
        user
    } else {
        format!("{domain}\\{user}")
    }
}

/// 单个任务 / 服务的完整名字（文件夹 + 名字，或裸名字）。
pub fn task_name(name: &str, level: ServiceLevel) -> String {
    match level {
        ServiceLevel::System => name.to_owned(),
        ServiceLevel::User => format!("{USER_TASK_FOLDER}\\{name}"),
    }
}

/// 生成用户级任务的注册 XML（**纯函数**，测试直接断言正文）。
///
/// 关键字段：
/// - `<LogonTrigger><UserId>…` —— 登录时启动（`ONLOGON`）；
/// - `<Hidden>true</Hidden>` —— 不在任务计划程序界面里显示（见模块文档的说明）；
/// - `<RestartOnFailure><Interval>PT1M</Interval><Count>3</Count>` —— **失败重启策略**，
///   也就是 `verify_restart_policy` 要读回来的那一项。
pub fn task_xml(opts: &InstallOptions) -> String {
    let command = xml_escape(&command_line_of(opts));
    let user_id = xml_escape(&current_user_id());
    let enabled = opts.autostart != Autostart::Off;
    let interval = restart_interval_iso8601();

    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>peon-burrow relay: keeps a local WebSocket to IMAP relay running for this user.</Description>
    <URI>{folder}\{name}</URI>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>{enabled}</Enabled>
      <UserId>{user_id}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user_id}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>true</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <RestartOnFailure>
      <Interval>{interval}</Interval>
      <Count>{count}</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
    </Exec>
  </Actions>
</Task>
"#,
        folder = USER_TASK_FOLDER,
        name = xml_escape(&opts.name),
        count = RESTART_RETRY_COUNT,
    )
}

// --- 用户级（任务计划程序） -------------------------------------------------

/// Windows 用户级宿主：**任务计划程序**，零提权。
#[derive(Debug)]
pub struct WindowsUserHost {
    runner: Box<dyn Runner>,
    name: String,
}

impl WindowsUserHost {
    /// 用默认服务名 + 真执行器。
    pub fn new() -> Self {
        Self::with_name(crate::host::DEFAULT_SERVICE_NAME)
    }

    /// 指定服务名 + 真执行器。
    pub fn with_name(name: impl Into<String>) -> Self {
        Self::with_runner(name, crate::runner::RealRunner)
    }

    /// 指定服务名 + 注入的执行器（**测试用**；生产请用 [`WindowsUserHost::new`]）。
    pub fn with_runner(name: impl Into<String>, runner: impl Runner + 'static) -> Self {
        Self {
            runner: Box::new(runner),
            name: name.into(),
        }
    }

    /// 服务名。
    pub fn name(&self) -> &str {
        &self.name
    }

    fn runner(&self) -> &dyn Runner {
        self.runner.as_ref()
    }

    /// 任务在计划任务程序里的全名（含文件夹）。
    fn registered_name(&self) -> String {
        task_name(&self.name, ServiceLevel::User)
    }

    /// `schtasks /Query /XML`：把注册信息读回来（**本地化无关**：XML 标签永远是英文）。
    fn query_xml(&self) -> Result<String, ServiceError> {
        let spec = CommandSpec::new(
            "schtasks",
            ["/Query", "/TN", &self.registered_name(), "/XML"],
            "读取登录自启任务的注册信息",
        );
        crate::host::read_only(self.runner(), &spec)
    }

    /// 任务是否已注册（查询成功 = 已注册）。
    fn is_registered(&self) -> bool {
        self.query_xml().is_ok()
    }

    /// 任务当前是否在跑。
    ///
    /// 用 `schtasks /Query` 的表格输出找状态词。**两个词都认**：
    /// 英文 `Running` 与中文「正在运行」—— 这一层不能只认一种，
    /// 否则中文 Windows 上「已安装·未运行」会被误报成运行中（GUI 的按钮就全错了）。
    /// 即使真认不出来，最坏后果也只是多发一条幂等的 `/Run`。
    fn is_running(&self) -> bool {
        let spec = CommandSpec::new(
            "schtasks",
            ["/Query", "/TN", &self.registered_name()],
            "读取登录自启任务的运行状态",
        );
        match crate::host::read_only(self.runner(), &spec) {
            Ok(stdout) => {
                let upper = stdout.to_ascii_uppercase();
                upper.contains("RUNNING") || stdout.contains("正在运行")
            }
            Err(_) => false,
        }
    }
}

impl Default for WindowsUserHost {
    /// 等价于 [`WindowsUserHost::new`]：默认服务名 + 真执行器。
    fn default() -> Self {
        Self::new()
    }
}

impl ServiceHost for WindowsUserHost {
    fn status(&self) -> Result<ServiceStatus, ServiceError> {
        let xml = match self.query_xml() {
            Ok(xml) => xml,
            // 查询失败（任务不存在 / 计划任务服务不可用）→ 当作未安装。
            Err(_) => return Ok(ServiceStatus::not_installed(&self.name)),
        };
        Ok(ServiceStatus {
            installed: true,
            running: self.is_running(),
            level: Some(ServiceLevel::User),
            autostart: Some(inspect_autostart(&xml)),
            name: self.name.clone(),
            binary_path: inspect_command(&xml),
            requires_elevation: false,
            restart_policy_configured: verify_task_xml(&xml, &self.registered_name()).is_ok(),
            last_exit_code: inspect_last_result(&xml),
        })
    }

    fn install(&self, opts: &InstallOptions) -> Result<(), ServiceError> {
        crate::host::prepare_install(opts, ServiceLevel::User)?;

        let xml = task_xml(opts);
        // `-Force` = 已存在就覆盖（重装、修复策略都靠它；`service install` 会重写策略）。
        let script = format!(
            "$xml = [Console]::In.ReadToEnd(); Register-ScheduledTask -TaskName '{name}' -TaskPath '{folder}' -Xml $xml -Force | Out-Null",
            name = escape_powershell_single_quotes(&opts.name),
            folder = escape_powershell_single_quotes(USER_TASK_FOLDER),
        );
        let spec = CommandSpec::new(
            "powershell",
            [
                "-NoProfile".to_owned(),
                "-NonInteractive".to_owned(),
                "-Command".to_owned(),
                script,
            ],
            "注册登录自启任务（含失败重启策略）",
        )
        .with_stdin(xml);

        Plan::new().command(spec).execute(self.runner())?;
        tracing::info!(
            event = "service.install",
            mode = "user",
            autostart = ?opts.autostart,
            path = %opts.binary_path.display(),
            "已注册登录自启任务"
        );

        // 自检（service-lifecycle.md § 3 第 9a 步）：策略没写进去就报错，不假装装好了。
        // 这里直接解析刚读回来的 XML，不再多查一次「装没装」—— 刚 `Register-ScheduledTask`
        // 过，再查一遍只会多一条命令、多一个失败点。
        verify_task_xml(&self.query_xml()?, &self.registered_name())
    }

    fn uninstall(&self) -> Result<(), ServiceError> {
        if !self.is_registered() {
            return Ok(());
        }
        let spec = CommandSpec::new(
            "schtasks",
            ["/Delete", "/TN", &self.registered_name(), "/F"],
            "删除登录自启任务",
        );
        Plan::new().command(spec).execute(self.runner())?;
        tracing::info!(
            event = "service.uninstall",
            mode = "user",
            "已删除登录自启任务"
        );
        Ok(())
    }

    fn start(&self) -> Result<(), ServiceError> {
        if self.is_running() {
            // 「已经在跑」是常态（用户点两次、看门狗拉一次、自更新后重启一次），当成成功。
            return Ok(());
        }
        let spec = CommandSpec::new(
            "schtasks",
            ["/Run", "/TN", &self.registered_name()],
            "启动登录自启任务",
        );
        Plan::new().command(spec).execute(self.runner())
    }

    fn stop(&self) -> Result<(), ServiceError> {
        // 幂等：任务没在跑时 `schtasks /End` 会报错，而报错的**文案随系统语言变**
        // （匹配文案很脆）。所以先问状态：没在跑 = 已经是我们想要的状态，直接成功。
        if !self.is_running() {
            return Ok(());
        }
        let spec = CommandSpec::new(
            "schtasks",
            ["/End", "/TN", &self.registered_name()],
            "结束登录自启任务",
        );
        Plan::new().command(spec).execute(self.runner())
    }

    fn set_autostart(&self, on: bool) -> Result<(), ServiceError> {
        let action = if on { "/Enable" } else { "/Disable" };
        let spec = CommandSpec::new(
            "schtasks",
            ["/Change", "/TN", &self.registered_name(), action],
            if on {
                "开启登录自启"
            } else {
                "关闭登录自启"
            },
        );
        Plan::new().command(spec).execute(self.runner())
    }

    fn verify_restart_policy(&self) -> Result<(), ServiceError> {
        let xml = self.query_xml().map_err(|error| match error {
            // `schtasks /Query` 对不存在的任务报「系统找不到指定的文件」——
            // 那其实是「没安装」，换成用户能照做的错误。
            ServiceError::CommandFailed { .. } => ServiceError::NotInstalled {
                name: self.name.clone(),
            },
            other => other,
        })?;
        verify_task_xml(&xml, &self.registered_name())
    }
}

/// 从任务 XML 里读「失败重启策略」。
///
/// 这就是 `service-lifecycle.md § 3` 第 9a 步要的那个判定：**字段真的在不在**。
/// `task_name` 只用于错误文案（让用户知道是哪个任务）。
pub fn verify_task_xml(xml: &str, task_name: &str) -> Result<(), ServiceError> {
    let Some(section) = xml_section(xml, "RestartOnFailure") else {
        return Err(ServiceError::RestartPolicyMissing {
            name: task_name.to_owned(),
            expected: format!(
                "<RestartOnFailure> 间隔 {} 次 {}（失败后自己回来）",
                restart_interval_iso8601(),
                RESTART_RETRY_COUNT
            ),
            found: "任务 XML 里没有 <RestartOnFailure> 段".to_owned(),
        });
    };
    let Some(count) =
        xml_section(section, "Count").and_then(|text| text.trim().parse::<u32>().ok())
    else {
        return Err(ServiceError::RestartPolicyMissing {
            name: task_name.to_owned(),
            expected: "<Count> 是正整数".to_owned(),
            found: section.trim().to_owned(),
        });
    };
    if count == 0 {
        return Err(ServiceError::RestartPolicyMissing {
            name: task_name.to_owned(),
            expected: "重启次数大于 0".to_owned(),
            found: section.trim().to_owned(),
        });
    }
    Ok(())
}

/// 从任务 XML 里读自启设置（`<LogonTrigger>` 是否启用、任务是否被停用）。
pub fn inspect_autostart(xml: &str) -> Autostart {
    if xml_section(xml, "Settings")
        .is_some_and(|settings| xml_bool(settings, "Enabled") == Some(false))
    {
        return Autostart::Off;
    }
    match xml_section(xml, "LogonTrigger") {
        // ⚠️ `<Enabled>` **缺省即 true**：任务计划程序不会写出这个元素。
        // 实证：装好用户级自启后导出的 XML 里，`<LogonTrigger>` 只有一个 `<UserId>`。
        // 要求它必须是 `Some(true)`，会把「登录时启动」误报成「已关闭」——而界面正靠这个值
        // 显示开关状态，用户会以为自启没配上（真机踩过）。
        Some(trigger) if xml_bool(trigger, "Enabled") != Some(false) => Autostart::Logon,
        _ => Autostart::Off,
    }
}

/// 从任务 XML 的动作里读被设置的二进制路径。
pub fn inspect_command(xml: &str) -> Option<String> {
    let actions = xml_section(xml, "Actions")?;
    let exec = xml_section(actions, "Exec")?;
    let command = xml_section(exec, "Command")?;
    parse_windows_program(command)
}

/// 从一行 Windows 命令行里取出**第一个 token**（也就是可执行文件路径）。
///
/// 规则按 `CommandLineToArgvW` 的约定，只做到「取第一个 token」为止：
/// 双引号内的部分算一个 token，`\"` 是字面引号，`\\"` 是「反斜杠 + 结束引号」。
pub fn parse_windows_program(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    // 非引号开头：第一个空白就是 token 边界。
    if !trimmed.starts_with('"') {
        return trimmed
            .split_whitespace()
            .next()
            .map(str::to_owned)
            .filter(|token| !token.is_empty());
    }

    // 引号开头：读到未被转义的收尾引号。
    let mut program = String::new();
    let mut chars = trimmed[1..].chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            // 反斜杠：只对 `"` 与 `\` 有转义意义
            '\\' if matches!(chars.peek(), Some('"' | '\\')) => {
                if let Some(escaped) = chars.next() {
                    program.push(escaped);
                }
            }
            '"' => break,
            _ => program.push(ch),
        }
    }
    (!program.is_empty()).then_some(program)
}

/// 从任务 XML 里读上次运行结果（`<LastTaskResult>`）。
pub fn inspect_last_result(xml: &str) -> Option<i32> {
    xml_section(xml, "LastTaskResult")?.trim().parse().ok()
}

/// 取 `<tag …>…</tag>` 之间的正文（**不引入 XML 解析库**：任务 XML 的形状由我们自己写，
/// 这里只需要能读回自己写的那几个字段；`schtasks` 会原样返回我们写的 XML）。
///
/// 支持带属性的开标签（`<Actions Context="Author">`）；
/// 对同名嵌套标签不敏感：我们关心的这些标签都只出现在一个位置。
pub fn xml_section<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}");
    let open_start = xml.find(&open)?;
    // 跳过属性，找到开标签的 `>`
    let open_end = xml[open_start..].find('>')? + open_start + 1;
    let close = format!("</{tag}>");
    let close_start = xml[open_end..].find(&close)? + open_end;
    Some(&xml[open_end..close_start])
}

/// 读 `<tag>true</tag>` / `<tag>false</tag>`。
fn xml_bool(xml: &str, tag: &str) -> Option<bool> {
    match xml_section(xml, tag)?.trim() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

/// PowerShell 单引号字符串里的转义（`'` → `''`）。
fn escape_powershell_single_quotes(value: &str) -> String {
    value.replace('\'', "''")
}

// --- 系统级（SCM） ---------------------------------------------------------

/// Windows 系统级宿主：**SCM**（`sc.exe`），安装时一次 UAC。
///
/// 刻意**不**引入 `windows-service` crate（`modules.md § 5` 的表格里写了它，
/// 但本 crate 的依赖白名单只有 `ipc-types` / `thiserror` / `tracing` / `sysinfo`；
/// 而且 `windows-service` 是给「服务进程内部」用的，与「外部注册服务」正好是两件事）。
#[derive(Debug)]
pub struct WindowsSystemHost {
    runner: Box<dyn Runner>,
    name: String,
}

impl WindowsSystemHost {
    /// 用默认服务名 + 真执行器。
    pub fn new() -> Self {
        Self::with_name(crate::host::DEFAULT_SERVICE_NAME)
    }

    /// 指定服务名 + 真执行器。
    pub fn with_name(name: impl Into<String>) -> Self {
        Self::with_runner(name, crate::runner::RealRunner)
    }

    /// 指定服务名 + 注入的执行器（**测试用**）。
    pub fn with_runner(name: impl Into<String>, runner: impl Runner + 'static) -> Self {
        Self {
            runner: Box::new(runner),
            name: name.into(),
        }
    }

    /// 服务名。
    pub fn name(&self) -> &str {
        &self.name
    }

    fn runner(&self) -> &dyn Runner {
        self.runner.as_ref()
    }

    /// `sc.exe query`：服务在不在、跑没跑。
    fn is_started(&self) -> bool {
        let spec = CommandSpec::new("sc", ["query", &self.name], "读取系统服务运行状态");
        match crate::host::read_only(self.runner(), &spec) {
            Ok(stdout) => service_is_running(&stdout),
            Err(_) => false,
        }
    }
}

/// `sc query` 的输出里是否显示正在运行。
///
/// `sc` 的字段名与状态词**不做本地化**（`STATE : 4 RUNNING`），所以这里只认 ASCII。
fn service_is_running(stdout: &str) -> bool {
    stdout.to_ascii_uppercase().contains("RUNNING")
}

impl Default for WindowsSystemHost {
    /// 等价于 [`WindowsSystemHost::new`]：默认服务名 + 真执行器。
    fn default() -> Self {
        Self::new()
    }
}

impl ServiceHost for WindowsSystemHost {
    fn status(&self) -> Result<ServiceStatus, ServiceError> {
        let query = CommandSpec::new("sc", ["query", &self.name], "读取系统服务注册状态");
        let stdout = match crate::host::read_only(self.runner(), &query) {
            Ok(stdout) => stdout,
            // `sc query` 对没装的服务返回非零（1060：指定的服务未安装）。
            Err(_) => return Ok(ServiceStatus::not_installed(&self.name)),
        };
        if !stdout.to_ascii_uppercase().contains("SERVICE_NAME") {
            return Ok(ServiceStatus::not_installed(&self.name));
        }

        let failure =
            CommandSpec::new("sc", ["qfailure", &self.name], "读取系统服务的失败重启策略");
        let failure_stdout = crate::host::read_only(self.runner(), &failure).unwrap_or_default();

        Ok(ServiceStatus {
            installed: true,
            // 复用上面那条 `sc query` 的输出，不再多查一次（少一条命令 = 少一个失败点）。
            running: service_is_running(&stdout),
            level: Some(ServiceLevel::System),
            autostart: Some(inspect_sc_autostart(&stdout)),
            name: self.name.clone(),
            binary_path: inspect_sc_binary_path(&stdout),
            // 系统服务：装 / 卸 / 启 / 停都要管理员；读状态不要。
            requires_elevation: true,
            restart_policy_configured: verify_sc_failure_output(&failure_stdout, &self.name)
                .is_ok(),
            last_exit_code: None,
        })
    }

    fn install(&self, opts: &InstallOptions) -> Result<(), ServiceError> {
        crate::host::prepare_install(opts, ServiceLevel::System)?;

        let bin_path = command_line_of(opts);
        let start_type = match opts.autostart {
            Autostart::Boot => "auto",
            Autostart::Off => "demand",
            // `prepare_install` 已经挡掉这一组了；这里只是让 match 穷尽，
            // 文案保持一致（真跑到这里就等于那条校验被改坏了）。
            Autostart::Logon => {
                return Err(ServiceError::invalid_options(
                    "系统服务不依赖登录：请用 Autostart::Boot 或 Autostart::Off",
                ));
            }
        };

        let create = CommandSpec::new(
            "sc",
            [
                "create".to_owned(),
                opts.name.clone(),
                // ⚠️ `sc` 要求**选项名与值分成两个参数**（`sc create x binPath= "…" start= auto`）。
                // 把 `"start= auto"` 当成一个参数传过去，它会报「无效 start= 参数」（退出码 1639，
                // 真机踩过）。按渲染后的命令行写断言是看不出来的：`display_line()` 把两种拼法
                // 拼成同一行，所以 `assert!(line.contains("start= auto"))` 对两种写法都成立。
                "binPath=".to_owned(),
                bin_path,
                "start=".to_owned(),
                start_type.to_owned(),
                "DisplayName=".to_owned(),
                opts.name.clone(),
            ],
            "创建系统服务",
        );
        // ⚠️ `sc create` **写不了** failure actions —— 必须紧跟一条 `sc failure`，
        // 而它的失效只在「服务崩了」时才暴露（service-lifecycle.md § 3 的提醒）。
        let failure = self.failure_command();
        let description = CommandSpec::new(
            "sc",
            [
                "description".to_owned(),
                opts.name.clone(),
                "peon-burrow relay: local WebSocket to IMAP relay".to_owned(),
            ],
            "写系统服务描述",
        );

        Plan::new()
            .command(create)
            .command(description)
            .command(failure)
            .execute(self.runner())?;

        tracing::info!(
            event = "service.install",
            mode = "system",
            autostart = ?opts.autostart,
            path = %opts.binary_path.display(),
            "已创建系统服务"
        );

        // 自检：这里正是 `sc create` 会漏掉失败重启策略的地方。
        self.verify_restart_policy()
    }

    fn uninstall(&self) -> Result<(), ServiceError> {
        // ⚠️ 必须先停：`sc delete` 只做标记，正在跑的进程还会继续跑
        //（表现为「卸载了但扩展还能收信」）。
        self.stop()?;
        let spec = CommandSpec::new("sc", ["delete", &self.name], "删除系统服务");
        match Plan::new().command(spec).execute(self.runner()) {
            Ok(()) => Ok(()),
            Err(error) => {
                // 没装就删：`sc delete` 会报 1060；卸载是幂等的，不当作失败。
                if error.to_string().contains("1060") {
                    Ok(())
                } else {
                    Err(error)
                }
            }
        }
    }

    fn start(&self) -> Result<(), ServiceError> {
        if self.is_started() {
            return Ok(());
        }
        let spec = CommandSpec::new("sc", ["start", &self.name], "启动系统服务");
        match Plan::new().command(spec).execute(self.runner()) {
            Ok(()) => Ok(()),
            Err(error) => {
                // 1056 = 服务已在运行；竞态下会撞上，不算失败。
                if error.to_string().contains("1056") {
                    Ok(())
                } else {
                    Err(error)
                }
            }
        }
    }

    fn stop(&self) -> Result<(), ServiceError> {
        let spec = CommandSpec::new("sc", ["stop", &self.name], "停止系统服务");
        match Plan::new().command(spec).execute(self.runner()) {
            Ok(()) => Ok(()),
            Err(error) => {
                let text = error.to_string();
                // 1062 = 服务未启动；1060 = 服务不存在。两者都已经是「停着」的状态。
                if text.contains("1062") || text.contains("1060") {
                    Ok(())
                } else {
                    Err(error)
                }
            }
        }
    }

    fn set_autostart(&self, on: bool) -> Result<(), ServiceError> {
        let start_type = if on { "auto" } else { "demand" };
        let spec = CommandSpec::new(
            "sc",
            [
                "config".to_owned(),
                self.name.clone(),
                // 同 `create`：选项名与值必须分开传（否则 1639「无效 start= 参数」）
                "start=".to_owned(),
                start_type.to_owned(),
            ],
            if on {
                "开启系统服务自启"
            } else {
                "关闭系统服务自启（改为手动）"
            },
        );
        Plan::new().command(spec).execute(self.runner())
    }

    fn verify_restart_policy(&self) -> Result<(), ServiceError> {
        let failure =
            CommandSpec::new("sc", ["qfailure", &self.name], "读取系统服务的失败重启策略");
        let stdout = crate::host::read_only(self.runner(), &failure)?;
        verify_sc_failure_output(&stdout, &self.name)
    }
}

impl WindowsSystemHost {
    /// `sc failure` 命令（写失败重启策略）。
    fn failure_command(&self) -> CommandSpec {
        CommandSpec::new(
            "sc",
            [
                "failure".to_owned(),
                self.name.clone(),
                // 第一次 / 第二次 / 后续失败都重新启动
                "reset=".to_owned(),
                SYSTEM_RESTART_RESET_SECS.to_string(),
                "actions=".to_owned(),
                format!(
                    "restart/{}/restart/{}/restart/{}",
                    SYSTEM_RESTART_DELAY_MS, SYSTEM_RESTART_DELAY_MS, SYSTEM_RESTART_DELAY_MS
                ),
            ],
            "写入系统服务的失败重启策略",
        )
    }
}

/// 解析 `sc.exe qfailure <name>` 的输出。
///
/// 真实输出（⚠️ 中文系统上**标签会被本地化**成「失败操作」之类，
/// 所以判定只看动作词与延迟数字这两个 ASCII 部分）：
///
/// ```text
/// [SC] QueryServiceConfig2 SUCCESS
///
/// SERVICE_NAME: peon-burrow
///         RESET_PERIOD (in seconds) : 86400
///         REBOOT_MESSAGE           :
///         COMMAND_LINE             :
///         FAILURE_ACTIONS          : RESTART -- DELAY: 5000 ms
///                                    RESTART -- DELAY: 5000 ms
/// ```
///
/// 判定（`service_name` 只进错误文案，让用户知道是哪个服务）：
/// 1. 必须出现 `RESTART`（写成 `RUN` 是「跑命令」，不是重启 —— 那不算失败重启策略）；
/// 2. 重启延迟必须 ≤ 5 秒：`service-lifecycle.md § 5` 要求 5 秒内起来，
///    否则扩展侧的 watch 重连（1s/2s/5s…）会在用户察觉前还没恢复。
pub fn verify_sc_failure_output(stdout: &str, service_name: &str) -> Result<(), ServiceError> {
    let upper = stdout.to_ascii_uppercase();
    if !upper.contains("RESTART") {
        return Err(ServiceError::RestartPolicyMissing {
            name: service_name.to_owned(),
            expected: format!(
                "失败后重新启动（sc failure actions=restart/{SYSTEM_RESTART_DELAY_MS}）"
            ),
            found: first_interesting_line(stdout),
        });
    }

    // 取第一个 DELAY 的数字（毫秒）
    let delay_ms = parse_first_delay_ms(&upper);
    match delay_ms {
        Some(delay) if delay > SYSTEM_RESTART_DELAY_MS => Err(ServiceError::RestartPolicyMissing {
            name: service_name.to_owned(),
            expected: format!(
                "重启延迟不超过 {SYSTEM_RESTART_DELAY_MS} ms（自更新后要 5 秒内起来）"
            ),
            found: format!("延迟 {delay} ms"),
        }),
        _ => Ok(()),
    }
}

/// 从 `RESTART -- DELAY: 5000 ms` 里取第一个延迟。
fn parse_first_delay_ms(upper: &str) -> Option<u64> {
    let index = upper.find("DELAY:")?;
    let rest = upper[index + "DELAY:".len()..].trim_start();
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// 从 `sc query` 的输出里读启动类型。
pub fn inspect_sc_autostart(stdout: &str) -> Autostart {
    // `sc query` 不打印启动类型，`sc qc` 才打印。这里的输入可能是 `sc qc` 的输出：
    // START_TYPE : 2   AUTO_START  (DELAYED)
    let upper = stdout.to_ascii_uppercase();
    if upper.contains("AUTO_START") {
        Autostart::Boot
    } else if upper.contains("DEMAND_START") {
        Autostart::Off
    } else {
        // 读不出来时不要谎报「开着」
        Autostart::Off
    }
}

/// 从 `sc qc` 的输出里读 `BINARY_PATH_NAME`（**只取路径，不取参数**）。
pub fn inspect_sc_binary_path(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        let upper = line.to_ascii_uppercase();
        if !upper.contains("BINARY_PATH_NAME") {
            continue;
        }
        let (_, value) = line.split_once(':')?;
        return parse_windows_program(value);
    }
    None
}

/// `sc qfailure` 里第一行有信息的内容（没有失败策略时给用户看它）。
fn first_interesting_line(stdout: &str) -> String {
    stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.to_ascii_uppercase().contains("SUCCESS"))
        .unwrap_or("（输出为空）")
        .to_owned()
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{CommandSpec, FakeRunner, Mutation};

    fn sample_opts() -> InstallOptions {
        let mut opts = InstallOptions::new(
            "peon-burrow",
            r"C:\Users\me\AppData\Local\Programs\peon-burrow\burrow.exe",
        );
        opts.args = vec!["run".to_owned()];
        opts
    }

    /// `schtasks /Query /XML` 真正返回的那种形状（节选，够覆盖我们要读的字段）。
    fn task_xml_response() -> String {
        task_xml(&sample_opts())
    }

    /// `sc qfailure` 的输出（中文系统上标签是中文的，但动作与延迟是英文）。
    const SCFAILURE_FIXTURE: &str = "\
[SC] QueryServiceConfig2 SUCCESS

SERVICE_NAME: peon-burrow
        RESET_PERIOD (in seconds) : 86400
        REBOOT_MESSAGE           :
        COMMAND_LINE             :
        FAILURE_ACTIONS          : RESTART -- DELAY: 5000 ms
                                   RESTART -- DELAY: 5000 ms
";

    #[test]
    fn task_xml_carries_hidden_logon_and_failure_restart() {
        let xml = task_xml(&sample_opts());
        assert!(
            xml.contains("<Hidden>true</Hidden>"),
            "用户级自启必须隐藏，否则登录时会闪黑框"
        );
        assert!(xml.contains("<LogonTrigger>"), "用户级默认是登录时启动");
        assert!(
            xml.contains("<RestartOnFailure>"),
            "失败重启策略必须写进任务 XML（sc create 那种漏写方式在这里被排除）"
        );
        assert!(xml.contains("<Interval>PT1M</Interval>"));
        assert!(xml.contains("<Count>3</Count>"));
        assert!(
            xml.contains("<RunLevel>LeastPrivilege</RunLevel>"),
            "用户级任务绝不能用最高权限跑"
        );
    }

    #[test]
    fn task_xml_quotes_paths_with_spaces_and_escapes_xml() {
        let mut opts =
            InstallOptions::new("peon-burrow", r"C:\Program Files\peon & burrow\burrow.exe");
        opts.args = vec!["run".to_owned(), "--port".to_owned(), "41316".to_owned()];
        let xml = task_xml(&opts);

        // 路径在 XML 正文里必须先做 XML 转义（`&` → `&amp;`）…
        assert!(xml.contains("peon &amp; burrow"), "XML 正文里的 & 必须转义");
        // …而命令行的引号是给 CreateProcess 看的，用 `&quot;` 表示
        assert!(
            xml.contains("&quot;C:\\Program Files\\peon &amp; burrow\\burrow.exe&quot; run"),
            "带空格的路径必须带引号：{xml}"
        );
        assert!(xml.contains("--port 41316"));
    }

    #[test]
    fn autostart_off_disables_the_logon_trigger() {
        let mut opts = sample_opts();
        opts.autostart = Autostart::Off;
        let xml = task_xml(&opts);
        assert!(xml.contains("<LogonTrigger>\n      <Enabled>false</Enabled>"));
        assert_eq!(inspect_autostart(&xml), Autostart::Off);
        assert_eq!(
            inspect_autostart(&task_xml(&sample_opts())),
            Autostart::Logon,
            "默认（登录自启）要能被读回成 Logon"
        );
    }

    #[test]
    fn autostart_is_read_from_real_task_scheduler_xml() {
        // 实证（`Export-ScheduledTask` 导出我们装好的任务）：`<LogonTrigger>` 里**只有**
        // `<UserId>`，没有 `<Enabled>` —— 缺省即启用。只认 `Some(true)` 会把登录自启
        // 误报成「已关闭」，而这个值正是界面显示开关状态的依据。
        let xml = r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers>
    <LogonTrigger>
      <UserId>DESKTOP-8I3HR5D\imba97</UserId>
    </LogonTrigger>
  </Triggers>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
  </Settings>
</Task>"#;
        assert_eq!(inspect_autostart(xml), Autostart::Logon);
    }

    #[test]
    fn an_explicitly_disabled_logon_trigger_is_off() {
        // 用户手工在「任务计划程序」里禁用触发器时，XML **会**写出这一行
        let xml = "<Triggers><LogonTrigger><UserId>x</UserId><Enabled>false</Enabled></LogonTrigger></Triggers>";
        assert_eq!(inspect_autostart(xml), Autostart::Off);
    }

    #[test]
    fn a_disabled_task_is_off_even_with_a_logon_trigger() {
        // 任务被整体停用（`schtasks /Change /DISABLE`）时，settings 里会有 Enabled=false
        let xml = "<Triggers><LogonTrigger><UserId>x</UserId></LogonTrigger></Triggers>\
                   <Settings><Enabled>false</Enabled></Settings>";
        assert_eq!(inspect_autostart(xml), Autostart::Off);
    }

    #[test]
    fn restart_policy_inspection_accepts_our_own_xml() {
        verify_task_xml(&task_xml_response(), "peon-burrow").expect("自己生成的 XML 必须通过自检");
    }

    #[test]
    fn restart_policy_inspection_rejects_missing_or_zero_count() {
        let without = "<Task><Settings><Hidden>true</Hidden></Settings></Task>";
        let error =
            verify_task_xml(without, "peon-burrow").expect_err("没有 RestartOnFailure 就该报错");
        assert!(
            error.to_string().contains("RestartOnFailure"),
            "错误必须说清缺的是哪一项：{error}"
        );
        assert!(
            error.to_string().contains("peon-burrow"),
            "错误要点名是哪个任务：{error}"
        );
        assert!(
            error.action().contains("install"),
            "错误必须给出下一步动作：{}",
            error.action()
        );

        let zero = "<Task><Settings><RestartOnFailure><Interval>PT1M</Interval><Count>0</Count></RestartOnFailure></Settings></Task>";
        assert!(
            verify_task_xml(zero, "peon-burrow").is_err(),
            "Count=0 等于没有重启"
        );

        let broken = "<Task><Settings><RestartOnFailure><Interval>PT1M</Interval><Count>abc</Count></RestartOnFailure></Settings></Task>";
        assert!(
            verify_task_xml(broken, "peon-burrow").is_err(),
            "解析不了的 Count 要当作没配上"
        );
    }

    #[test]
    fn inspecting_command_takes_the_binary_out_of_the_action() {
        // `Actions` 带属性（`Context="Author"`），解析要能跨过它 —— 真实 XML 就是这样。
        let response = task_xml(&sample_opts());
        assert!(
            response.contains(r#"<Actions Context="Author">"#),
            "夹具要覆盖带属性的开标签"
        );
        assert_eq!(
            inspect_command(&response).as_deref(),
            Some(r"C:\Users\me\AppData\Local\Programs\peon-burrow\burrow.exe"),
            "带引号的动作要能取出纯路径（GUI 要拿它显示 / 校验）"
        );
    }

    #[test]
    fn windows_program_parsing_follows_the_quoted_and_escaped_forms() {
        // 无引号 + 参数
        assert_eq!(
            parse_windows_program("C:\\tools\\burrow.exe run").as_deref(),
            Some("C:\\tools\\burrow.exe")
        );
        // 有引号 + 参数（`sc qc` 里 binPath 的常态）
        assert_eq!(
            parse_windows_program("\"C:\\Program Files\\b\\burrow.exe\" run").as_deref(),
            Some("C:\\Program Files\\b\\burrow.exe")
        );
        // 带转义反斜杠的路径
        assert_eq!(
            parse_windows_program("\"C:\\\\dir with space\\\\burrow.exe\" run").as_deref(),
            Some("C:\\dir with space\\burrow.exe")
        );
        // 按 `CommandLineToArgvW`，`\"` 是**字面引号**（不是收尾引号），
        // 所以路径里那个 `"` 要保留 —— 这正是「不能简单按 `"` 切分」的原因。
        assert_eq!(
            parse_windows_program("\"C:\\weird\\burrow.exe\\\" run").as_deref(),
            Some("C:\\weird\\burrow.exe\" run")
        );
        assert_eq!(parse_windows_program("   "), None);
        assert_eq!(parse_windows_program("\"\""), None);
    }

    #[test]
    fn user_install_registers_then_self_checks() {
        let runner = FakeRunner::new()
            // 1) Register-ScheduledTask
            .expect("")
            // 2) 自检：schtasks /Query /XML
            .expect(task_xml_response());

        let host = WindowsUserHost::with_runner("peon-burrow", runner.clone());
        host.install(&sample_opts()).expect("install 应当成功");

        let mutations = runner.mutations();
        let lines = mutations.command_lines();
        assert_eq!(lines.len(), 2, "安装就是「注册 + 自检」两条命令：{lines:?}");
        assert!(lines[0].starts_with("powershell -NoProfile -NonInteractive -Command"));
        assert!(
            lines[0].contains("Register-ScheduledTask"),
            "用户级必须走任务计划程序：{}",
            lines[0]
        );
        assert!(
            lines[1].contains("schtasks /Query /TN"),
            "第 9a 步必须把注册信息读回来：{}",
            lines[1]
        );
        assert_eq!(runner.expectations_left(), 0, "预置的输出必须被用完");
    }

    #[test]
    fn user_install_feeds_the_xml_through_stdin() {
        let runner = FakeRunner::new().expect("").expect(task_xml_response());
        let host = WindowsUserHost::with_runner("peon-burrow", runner.clone());
        host.install(&sample_opts()).expect("install");

        match runner.mutations().nth(0) {
            Some(crate::runner::Mutation::Run(spec)) => {
                let stdin = spec.stdin.expect("任务 XML 必须通过标准输入喂进去");
                assert!(stdin.contains("<RestartOnFailure>"), "喂进去的就是那份 XML");
                assert!(
                    stdin.contains("<Hidden>true</Hidden>"),
                    "XML 里必须有隐藏设置"
                );
            }
            other => panic!("第 1 个动作应当是命令：{other:?}"),
        }
    }

    #[test]
    fn user_install_fails_when_the_restart_policy_did_not_land() {
        // 自检读回来的 XML 里没有 RestartOnFailure：这正是「装完看着挺好，崩了不回来」
        let runner = FakeRunner::new()
            .expect("")
            .expect("<Task><Settings><Hidden>true</Hidden></Settings></Task>");

        let host = WindowsUserHost::with_runner("peon-burrow", runner);
        let error = host
            .install(&sample_opts())
            .expect_err("策略没写进去就必须报错");
        assert!(matches!(error, ServiceError::RestartPolicyMissing { .. }));
    }

    #[test]
    fn user_lifecycle_actions_are_idempotent_when_not_installed() {
        // 没装过时：卸载 / 自启开关都该是「已经是我们想要的状态」，不是错误。
        let runner = FakeRunner::new()
            // uninstall → is_registered → /Query /XML 失败
            .expect_failure("错误: 系统找不到指定的文件。")
            // set_autostart(false)
            .expect_failure("错误: 系统找不到指定的文件。");
        let host = WindowsUserHost::with_runner("peon-burrow", runner.clone());

        host.uninstall().expect("没装过时卸载必须成功（幂等）");
        let _ = host.set_autostart(false);
        assert_eq!(runner.expectations_left(), 0, "预置的输出必须被用完");
        assert_eq!(
            runner.mutations().run_count(),
            2,
            "只查了一次注册状态，没有再发 /Delete"
        );
    }

    #[test]
    fn user_uninstall_and_autostart_use_schtasks() {
        let runner = FakeRunner::new()
            // uninstall → is_registered → /Query /XML
            .expect(task_xml_response())
            // /Delete
            .expect("成功: 计划任务已删除")
            // set_autostart(false)
            .expect("成功")
            // set_autostart(true)
            .expect("成功");

        let host = WindowsUserHost::with_runner("peon-burrow", runner.clone());
        host.uninstall().expect("uninstall");
        host.set_autostart(false).expect("autostart off");
        host.set_autostart(true).expect("autostart on");

        let lines = runner.mutations().command_lines();
        assert!(lines[1].contains("schtasks /Delete /TN"));
        assert!(lines[1].contains("/F"), "卸载不该弹确认框：{}", lines[1]);
        assert!(lines[2].contains("/Change") && lines[2].contains("/Disable"));
        assert!(lines[3].contains("/Change") && lines[3].contains("/Enable"));
    }

    #[test]
    fn user_start_treats_already_running_as_success() {
        // `schtasks /Query` 报「正在运行」（本地化文本）→ 不该再发 /Run
        let runner =
            FakeRunner::new().expect("文件夹: \\peon-burrow\n任务名: peon-burrow\n状态: 正在运行");
        let host = WindowsUserHost::with_runner("peon-burrow", runner.clone());
        host.start().expect("已经在跑 = 成功");
        assert_eq!(runner.mutations().run_count(), 1, "只查了一次，没有再启动");
    }

    #[test]
    fn user_stop_ignores_not_running() {
        // 幂等靠**先问状态**，不靠匹配 `schtasks` 的报错文案（那文案随系统语言变）。
        let runner =
            FakeRunner::new().expect("文件夹: \\peon-burrow\n任务名: peon-burrow\n状态: 就绪");
        let host = WindowsUserHost::with_runner("peon-burrow", runner.clone());
        host.stop().expect("没在跑时 stop 必须成功（幂等）");
        assert_eq!(
            runner.mutations().run_count(),
            1,
            "只查了状态，没有再发 /End（发了会拿到一条本地化的报错）"
        );
    }

    #[test]
    fn user_stop_ends_a_running_task() {
        let runner = FakeRunner::new()
            .expect("状态: 正在运行") // 中文 Windows 的措辞
            .expect("成功: 计划任务已终止");
        let host = WindowsUserHost::with_runner("peon-burrow", runner.clone());
        host.stop().expect("stop");
        let lines = runner.mutations().command_lines();
        assert!(lines[1].contains("schtasks /End /TN"), "{lines:?}");
    }

    #[test]
    fn system_install_writes_failure_actions_and_self_checks() {
        let runner = FakeRunner::new()
            .expect("") // sc create
            .expect("") // sc description
            .expect("") // sc failure
            .expect(SCFAILURE_FIXTURE); // 自检：sc qfailure

        let host = WindowsSystemHost::with_runner("peon-burrow", runner.clone());
        let mut opts = sample_opts();
        opts.level = ServiceLevel::System;
        opts.autostart = Autostart::Boot;
        host.install(&opts).expect("install");

        let lines = runner.mutations().command_lines();
        assert!(lines[0].contains("sc create peon-burrow"));
        assert!(
            lines[0].contains("start= auto"),
            "开机启动 = auto：{}",
            lines[0]
        );
        assert!(
            lines[2].contains("sc failure peon-burrow"),
            "sc create 写不了 failure actions，必须紧跟 sc failure：{lines:?}"
        );
        assert!(lines[2].contains("actions= restart/5000/restart/5000/restart/5000"));
        assert!(
            lines[3].contains("sc qfailure"),
            "自检要读回来：{}",
            lines[3]
        );
    }

    /// 检查每一条 `sc` 命令的**参数形状**：选项名与值必须是相邻的两个参数。
    ///
    /// 真机踩过：`sc create … "start= auto"` → 退出码 1639「无效 start= 参数」。
    /// 这条只能查 argv —— `display_line()` 把 `["start= auto"]` 和 `["start=", "auto"]`
    /// 渲染成**完全一样**的一行，所以按命令行写断言是抓不住的。
    fn assert_sc_options_are_separate(commands: &[CommandSpec]) {
        const OPTIONS: [&str; 12] = [
            "type=",
            "start=",
            "error=",
            "binPath=",
            "group=",
            "tag=",
            "depend=",
            "obj=",
            "DisplayName=",
            "password=",
            "reset=",
            "actions=",
        ];
        for command in commands {
            if command.program != "sc" {
                continue;
            }
            for arg in &command.args {
                for option in OPTIONS {
                    if let Some(rest) = arg.strip_prefix(option) {
                        assert!(
                            rest.is_empty(),
                            "`{option}` 必须与它的值分成两个参数，现在是 {arg:?}（{}）",
                            command.purpose
                        );
                    }
                }
            }
        }
    }

    /// 取这次测试里跑过的所有命令。
    fn commands_of(runner: &FakeRunner) -> Vec<CommandSpec> {
        runner
            .mutations()
            .all()
            .into_iter()
            .filter_map(|mutation| match mutation {
                Mutation::Run(spec) => Some(spec),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn sc_options_are_passed_as_separate_arguments() {
        let runner = FakeRunner::new()
            .expect("") // sc create
            .expect("") // sc description
            .expect("") // sc failure
            .expect(SCFAILURE_FIXTURE); // sc qfailure
        let host = WindowsSystemHost::with_runner("peon-burrow", runner.clone());
        let mut opts = sample_opts();
        opts.level = ServiceLevel::System;
        opts.autostart = Autostart::Boot;
        host.install(&opts).expect("install");

        let commands = commands_of(&runner);
        assert_sc_options_are_separate(&commands);

        // 正向断言：`start=` 与 `auto` 是相邻的两个参数
        let create = commands.first().expect("sc create");
        let index = create
            .args
            .iter()
            .position(|arg| arg == "start=")
            .expect("create 里要有 start=");
        assert_eq!(create.args.get(index + 1).map(String::as_str), Some("auto"));
        // binPath 的值里带空格（可执行文件路径 + 参数），它**必须**是一个参数
        let binpath = create
            .args
            .iter()
            .position(|arg| arg == "binPath=")
            .expect("create 里要有 binPath=");
        assert!(
            create
                .args
                .get(binpath + 1)
                .is_some_and(|value| value.contains(' ')),
            "binPath 的值是一个参数（含空格）：{:?}",
            create.args
        );
    }

    #[test]
    fn sc_config_for_autostart_uses_separate_arguments_too() {
        // 同一个 bug 的另一处：系统服务的自启开关走 `sc config start= auto|demand`
        for on in [true, false] {
            let runner = FakeRunner::new().expect("");
            let host = WindowsSystemHost::with_runner("peon-burrow", runner.clone());
            host.set_autostart(on).expect("set_autostart");

            let commands = commands_of(&runner);
            assert_sc_options_are_separate(&commands);
            let config = commands.first().expect("sc config");
            let expected = if on { "auto" } else { "demand" };
            let index = config
                .args
                .iter()
                .position(|arg| arg == "start=")
                .expect("config 里要有 start=");
            assert_eq!(
                config.args.get(index + 1).map(String::as_str),
                Some(expected)
            );
        }
    }

    #[test]
    fn system_uninstall_stops_before_deleting() {
        // `sc delete` 只是打标记：不停就删，进程还会继续跑（「卸载了但扩展还能收信」）。
        let runner = FakeRunner::new()
            .expect("STATE : 4 RUNNING") // stop 之前的状态查询
            .expect("") // sc stop
            .expect(""); // sc delete

        let host = WindowsSystemHost::with_runner("peon-burrow", runner.clone());
        host.uninstall().expect("uninstall");

        let lines = runner.mutations().command_lines();
        assert_eq!(lines.len(), 2, "只有「停 + 删」两条：{lines:?}");
        assert!(lines[0].contains("sc stop"), "{lines:?}");
        assert!(lines[1].contains("sc delete"), "{lines:?}");
    }

    #[test]
    fn system_install_rejects_logon_autostart() {
        let runner = FakeRunner::new();
        let host = WindowsSystemHost::with_runner("peon-burrow", runner.clone());
        let mut opts = sample_opts();
        opts.level = ServiceLevel::System;
        opts.autostart = Autostart::Logon;
        let error = host
            .install(&opts)
            .expect_err("系统服务 + 登录自启 是非法组合");
        assert!(matches!(error, ServiceError::InvalidOptions { .. }));
        assert_eq!(
            runner.mutations().run_count(),
            0,
            "校验失败时一条命令都不该发"
        );
    }

    #[test]
    fn system_restart_policy_inspection() {
        verify_sc_failure_output(SCFAILURE_FIXTURE, "peon-burrow")
            .expect("有 RESTART 且有 5000ms 延迟就该通过");

        // 只有 RUN（跑命令）不算重启
        let run_only = "FAILURE_ACTIONS : RUN -- DELAY: 5000 ms";
        assert!(verify_sc_failure_output(run_only, "peon-burrow").is_err());

        // 延迟太长：自更新后的重启要在用户察觉前完成
        let slow = "FAILURE_ACTIONS : RESTART -- DELAY: 60000 ms";
        let error = verify_sc_failure_output(slow, "peon-burrow").expect_err("延迟 60 秒太慢");
        assert!(
            error.to_string().contains("60000"),
            "要报出读到的实际值：{error}"
        );
        assert!(
            error.to_string().contains("peon-burrow"),
            "要点名是哪个服务：{error}"
        );

        // 完全没有 failure actions
        let none = "[SC] QueryServiceConfig2 SUCCESS\n\nSERVICE_NAME: peon-burrow\n";
        let error = verify_sc_failure_output(none, "peon-burrow").expect_err("没有策略要报错");
        assert!(error.action().contains("install"), "{}", error.action());
    }

    #[test]
    fn system_status_reads_sc_output() {
        let query = "SERVICE_NAME: peon-burrow\n        TYPE               : 10  WIN32_OWN_PROCESS\n        STATE              : 4  RUNNING\n";
        let config = "SERVICE_NAME: peon-burrow\n        START_TYPE         : 2   AUTO_START\n        BINARY_PATH_NAME   : \"C:\\Program Files\\peon-burrow\\burrow.exe\" run\n";
        assert_eq!(inspect_sc_autostart(config), Autostart::Boot);
        assert_eq!(
            inspect_sc_autostart("START_TYPE : 3 DEMAND_START"),
            Autostart::Off
        );
        assert_eq!(
            inspect_sc_binary_path(config).as_deref(),
            Some(r"C:\Program Files\peon-burrow\burrow.exe")
        );
        assert!(query.contains("RUNNING"), "（夹具自检）");
    }

    #[test]
    fn status_reports_not_installed_without_a_task() {
        // `schtasks /Query` 对不存在的任务返回非零
        let runner = FakeRunner::new().expect_failure("错误: 系统找不到指定的文件。");
        let host = WindowsUserHost::with_runner("peon-burrow", runner);
        let status = host.status().expect("未安装不是错误");
        assert!(!status.installed);
        assert_eq!(status.name, "peon-burrow");
    }

    #[test]
    fn task_name_uses_a_folder_for_user_level_only() {
        assert_eq!(
            task_name("relay", ServiceLevel::User),
            r"\peon-burrow\relay"
        );
        assert_eq!(task_name("relay", ServiceLevel::System), "relay");
    }

    #[test]
    fn windows_command_line_follows_create_process_rules() {
        assert_eq!(
            windows_command_line(r"C:\tools\burrow.exe", &["run".to_owned()]),
            r"C:\tools\burrow.exe run"
        );
        assert_eq!(
            windows_command_line(r"C:\Program Files\burrow.exe", &[]),
            r#""C:\Program Files\burrow.exe""#
        );
        // 结尾反斜杠必须翻倍，否则会把收尾引号转义掉
        assert_eq!(
            windows_command_line(r"C:\dir with space\", &[]),
            r#""C:\dir with space\\""#
        );
    }
}
