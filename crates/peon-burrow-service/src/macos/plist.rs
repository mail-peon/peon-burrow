use super::paths::label_of;
use crate::host::{Autostart, InstallOptions, RESTART_RETRY_COUNT, RunMode};

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
