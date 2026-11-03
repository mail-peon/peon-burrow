use crate::host::{InstallOptions, ServiceLevel};
use std::path::PathBuf;

/// systemd unit 的 `.service` 后缀。
pub const UNIT_SUFFIX: &str = ".service";

/// `RestartSec`（`adr-0003 § 1`：2 秒）。
pub const RESTART_SECS: u32 = 2;

/// 把服务名规范成 systemd 的 unit 名（`relay` → `relay.service`）。
pub fn unit_name(name: &str) -> String {
    if name.ends_with(UNIT_SUFFIX) {
        name.to_owned()
    } else {
        format!("{name}{UNIT_SUFFIX}")
    }
}

/// unit 名是否合法（systemd 只接受 `[A-Za-z0-9:_.\-@]`，且不能带路径分隔符）。
///
/// 这条校验不是洁癖：unit 名会出现在 `/etc/systemd/system/<name>.service` 里，
/// 一个带 `../` 的名字等于让调用方**写到任意路径**。
pub fn is_valid_unit_name(name: &str) -> bool {
    let unit = unit_name(name);
    !unit.is_empty()
        && !unit.contains('/')
        && !unit.contains('\\')
        && unit
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, ':' | '_' | '.' | '-' | '@'))
}

/// unit 文件的落盘路径。
///
/// `$HOME` / `$XDG_CONFIG_HOME` 读不到时返回 `None`（布局铁律 L4：不在库里替调用方猜路径）。
pub fn unit_path(name: &str, level: ServiceLevel) -> Option<PathBuf> {
    match level {
        ServiceLevel::System => Some(
            PathBuf::from("/etc/systemd/system")
                .join(format!("{}.service", name.trim_end_matches(UNIT_SUFFIX))),
        ),
        ServiceLevel::User => {
            let base = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
                })?;
            Some(
                base.join("systemd/user")
                    .join(format!("{}.service", name.trim_end_matches(UNIT_SUFFIX))),
            )
        }
    }
}

/// XDG autostart 的 `.desktop` 路径（退路，见模块文档）。
pub fn xdg_autostart_path(name: &str) -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("autostart").join(format!("{name}.desktop")))
}

/// XDG autostart 的 `.desktop` 正文模板（退路，见模块文档）。
///
/// ⚠️ 它**没有**崩溃恢复能力，`Hidden=false` 只是「登录时启动一次」。
pub fn xdg_autostart_body(opts: &InstallOptions) -> String {
    let command = std::iter::once(opts.binary_path.display().to_string())
        .chain(opts.args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={name}\n\
         Comment=peon-burrow relay（无 systemd 时的退路：登录启动一次，无失败重启）\n\
         Exec={command}\n\
         Terminal=false\n\
         Hidden=false\n\
         X-GNOME-Autostart-enabled=true\n",
        name = opts.name,
    )
}

/// 生成 unit 文件正文（**纯函数**，测试直接断言）。
///
/// 关键字段：
/// - `ExecStart=-<binary> <args>` —— 前导 `-` 表示「非零退出码不视为启动失败」，
///   自更新用 `ExitCode::RestartRequested = 5` 结束进程时不会把 unit 打成 failed；
/// - `Restart=always` + `RestartSec=2` —— **失败重启策略**，`verify_restart_policy` 读的就是它；
/// - `WantedBy=default.target` —— `systemctl enable` 时建立的自启依赖。
pub fn unit_body(opts: &InstallOptions) -> String {
    let exec_start = std::iter::once(format!(
        "-{}",
        quote_systemd_arg(&opts.binary_path.display().to_string())
    ))
    .chain(opts.args.iter().map(|arg| quote_systemd_arg(arg)))
    .collect::<Vec<_>>()
    .join(" ");
    let working_directory = opts
        .binary_path
        .parent()
        .map(|parent| parent.display().to_string())
        .filter(|path| !path.is_empty())
        .map(|path| format!("WorkingDirectory={path}\n"))
        .unwrap_or_default();
    let description = format!("peon-burrow relay ({})", opts.mode.as_str());
    let documentation = match opts.level {
        ServiceLevel::User => "systemd --user 服务（登录时启动；要开机即起请用系统级）",
        ServiceLevel::System => "systemd 系统服务（开机启动）",
    };

    format!(
        "# {documentation}\n\
         # 由 peon-burrow-service 生成；手改后重装会被覆盖。\n\
         [Unit]\n\
         Description={description}\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exec_start}\n\
         {working_directory}\
         Restart=always\n\
         RestartSec={RESTART_SECS}\n\
         # 崩溃循环时不要让 systemd 无限快跑（5 次 / 10 秒内视为启动失败）\n\
         StartLimitIntervalSec=10\n\
         StartLimitBurst=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
    )
}

/// systemd 的参数引用：含空白或引号时整体用双引号包起来（systemd 支持 `"` 引用）。
fn quote_systemd_arg(value: &str) -> String {
    if value.is_empty() || value.contains([' ', '\t', '"', '\'']) {
        format!("\"{}\"", value.replace('"', "\\\""))
    } else {
        value.to_owned()
    }
}
