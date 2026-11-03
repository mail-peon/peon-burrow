use super::USER_TASK_FOLDER;
use crate::host::{InstallOptions, RESTART_INTERVAL_SECS, ServiceLevel};

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

/// 任务 XML 的 `<Command>`：**powershell**（它负责把中继的窗口藏起来）。
///
/// ⚠️ 为什么不直接指向 `burrow.exe`：Windows 会给控制台程序配一个控制台窗口，而任务计划
/// 程序的 `<Hidden>` 只隐藏**任务本身**，不隐藏被启动进程的窗口 —— 用户会看到一个带日志的
/// 终端（真机踩过）。`unsafe_code = "forbid"` 又堵住了 `ShowWindow` 那条路。
pub fn exec_command_of(_opts: &InstallOptions) -> String {
    powershell_path()
}

/// 任务 XML 的 `<Arguments>`：在**隐藏窗口**的 PowerShell 里调用中继。
///
/// 用 `&`（调用运算符）而不是 `Start-Process`：这样 PowerShell 会**等**子进程，
/// 任务状态保持 Running（否则任务会立刻「完成」，界面上的运行状态就错了）。
///
/// ⚠️ `<Command>` 与 `<Arguments>` 必须分开：把参数塞进 `<Command>` 会让任务计划程序
/// 拿整串当文件名去「打开」（弹「你要如何打开这个文件?」），而且任务永远起不来。
pub fn exec_arguments_of(opts: &InstallOptions) -> String {
    // 路径与参数都走同一套 PowerShell 引用（`'` 翻倍）
    let mut call = format!(
        "& {}",
        powershell_quote(&opts.binary_path.display().to_string())
    );
    for arg in &opts.args {
        call.push(' ');
        call.push_str(&powershell_quote(arg));
    }
    format!("-NoProfile -NonInteractive -WindowStyle Hidden -Command \"{call}\"")
}

/// PowerShell 单引号字符串（内部 `'` 翻倍）。
///
/// ⚠️ **参数也要引用**：今天用户级只传 `run`，但注册信息里的参数是**会变的**
/// （系统级已经带 `--config` / `--data-dir` 这类可能含空格的值）。不引用的话
/// PowerShell 会把带空格的值当成两个词，任务 XML 静默拼坏 —— 症状是「界面显示已安装、
/// running 一直是 false」（同类问题真机踩过）。
fn powershell_quote(value: &str) -> String {
    format!("'{}'", escape_powershell_single_quotes(value))
}

/// PowerShell 的绝对路径（任务 XML 里给绝对路径最稳妥）。
fn powershell_path() -> String {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned());
    format!(r"{root}\System32\WindowsPowerShell\v1.0\powershell.exe")
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

/// PowerShell 单引号字符串里的转义（`'` → `''`）。
pub(super) fn escape_powershell_single_quotes(value: &str) -> String {
    value.replace('\'', "''")
}

// --- 系统级（SCM） ---------------------------------------------------------
