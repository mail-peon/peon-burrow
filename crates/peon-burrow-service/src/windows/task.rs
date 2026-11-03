use super::USER_TASK_FOLDER;
use super::command::{
    current_user_id, exec_arguments_of, exec_command_of, restart_interval_iso8601, xml_escape,
};
use crate::host::{Autostart, InstallOptions, RESTART_RETRY_COUNT, ServiceError};

/// 生成用户级任务的注册 XML（**纯函数**，测试直接断言正文）。
///
/// 关键字段：
/// - `<LogonTrigger><UserId>…` —— 登录时启动（`ONLOGON`）；
/// - `<Hidden>true</Hidden>` —— 不在任务计划程序界面里显示（见模块文档的说明）；
/// - `<RestartOnFailure><Interval>PT1M</Interval><Count>3</Count>` —— **失败重启策略**，
///   也就是 `verify_restart_policy` 要读回来的那一项。
pub fn task_xml(opts: &InstallOptions) -> String {
    // ⚠️ `<Command>` 只放**可执行文件**，参数必须放 `<Arguments>`：塞在一起的话
    // 任务计划程序会拿整串当文件名去「打开」，于是弹出 Windows 的「你要如何打开这个文件?」，
    // 而且任务永远起不来（真机踩过：界面显示「已安装」但 running 一直 false）。
    let command = xml_escape(&exec_command_of(opts));
    let arguments = xml_escape(&exec_arguments_of(opts));
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
      <Arguments>{arguments}</Arguments>
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

    // 正常情况下 `<Command>` 就是可执行文件；但我们的任务是用 powershell 的隐藏窗口
    // 拉起中继的，真正的中继路径在 `<Arguments>` 里（`& '<path>' run`）。
    if let Some(path) = xml_section(exec, "Arguments").and_then(quoted_path) {
        return Some(path);
    }
    parse_windows_program(command)
}

/// 从 `-Command "& '<path>' run"` 里取出被单引号包住的路径。
fn quoted_path(arguments: &str) -> Option<String> {
    let start = arguments.find('\'')?;
    let rest = &arguments[start + 1..];
    let end = rest.find('\'')?;
    let path = &rest[..end];
    if path.is_empty() {
        None
    } else {
        // XML 正文里 `&` 是 `&amp;`：读回来的必须是**真路径**（界面上要显示它）
        Some(
            path.replace("''", "'")
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&quot;", "\"")
                .replace("&#39;", "'")
                .replace("&amp;", "&"),
        )
    }
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
