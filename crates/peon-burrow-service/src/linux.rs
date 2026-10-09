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

use std::path::PathBuf;

use crate::host::{
    Autostart, InstallOptions, Plan, RunMode, ServiceError, ServiceHost, ServiceLevel,
    ServiceStatus,
};
use crate::runner::{CommandSpec, FileInstall, Runner};

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

/// 解析 `systemctl show -p Restart -p RestartUSec` 的输出。
///
/// 真实输出（只有被请求的属性，顺序固定）：
///
/// ```text
/// Restart=always
/// RestartUSec=2s
/// ```
///
/// ⚠️ `Restart=` 的取值里 **`always` 与 `on-failure` 才算「失败会回来」**；
/// `no` 是 systemd 的默认值，也是最容易出现的「装完看着挺好、崩了不回来」。
pub fn inspect_restart_properties(stdout: &str) -> Result<(), ServiceError> {
    let mut restart: Option<&str> = None;
    let mut restart_usec: Option<&str> = None;
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("Restart=") {
            restart = Some(value.trim());
        } else if let Some(value) = line.strip_prefix("RestartUSec=") {
            restart_usec = Some(value.trim());
        }
    }

    match restart {
        Some("always") | Some("on-failure") | Some("on-abnormal") => Ok(()),
        Some(other) => Err(ServiceError::RestartPolicyMissing {
            name: "systemd unit".to_owned(),
            expected: "Restart=always（或 on-failure）+ RestartSec=2".to_owned(),
            found: format!(
                "Restart={other}{}",
                restart_usec
                    .map(|usec| format!("，RestartUSec={usec}"))
                    .unwrap_or_default()
            ),
        }),
        None => Err(ServiceError::RestartPolicyMissing {
            name: "systemd unit".to_owned(),
            expected: "Restart=always".to_owned(),
            found: "systemctl show 的输出里没有 Restart= 这一行".to_owned(),
        }),
    }
}

/// 解析 `systemctl cat`（或任何 unit 正文）里的 `Restart=`。
///
/// 作为 [`inspect_restart_properties`] 的退路：`systemctl show` 在某些版本上
/// 对 `--user` 单元返回空属性列表，那时直接看 unit 正文更可靠。
pub fn inspect_restart_line(text: &str) -> Result<(), ServiceError> {
    let found = text
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("Restart="))
        .map(|value| value.trim().to_owned());

    match found.as_deref() {
        Some("always") | Some("on-failure") | Some("on-abnormal") => Ok(()),
        other => Err(ServiceError::RestartPolicyMissing {
            name: "systemd unit".to_owned(),
            expected: "Restart=always".to_owned(),
            found: other
                .map(|value| format!("Restart={value}"))
                .unwrap_or_else(|| "unit 文件里没有 Restart= 这一行".to_owned()),
        }),
    }
}

/// 解析 `systemctl show -p ActiveState -p SubState -p UnitFileState` 的输出。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SystemdState {
    /// `ActiveState`（`active` = 在跑）。
    pub active_state: Option<String>,
    /// `SubState`（`running` / `dead` / `exited` / `failed`）。
    pub sub_state: Option<String>,
    /// `UnitFileState`（`enabled` = 开机自启）。
    pub unit_file_state: Option<String>,
}

impl SystemdState {
    /// 是否在运行。
    pub fn running(&self) -> bool {
        matches!(self.active_state.as_deref(), Some("active"))
    }

    /// 自启是否开着。
    pub fn autostart(&self) -> Autostart {
        match self.unit_file_state.as_deref() {
            Some("enabled") | Some("enabled-runtime") | Some("alias") => Autostart::Boot,
            _ => Autostart::Off,
        }
    }
}

/// 解析 `systemctl show` 的 `key=value` 行。
///
/// ```text
/// ActiveState=active
/// SubState=running
/// UnitFileState=enabled
/// ```
pub fn inspect_systemd_state(stdout: &str) -> SystemdState {
    let mut state = SystemdState::default();
    for line in stdout.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().to_owned();
        if value.is_empty() {
            continue;
        }
        match key {
            "ActiveState" => state.active_state = Some(value),
            "SubState" => state.sub_state = Some(value),
            "UnitFileState" => state.unit_file_state = Some(value),
            _ => {}
        }
    }
    state
}

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

/// Linux 宿主（用户级 = `systemd --user`，系统级 = systemd system unit）。
#[derive(Debug)]
pub struct LinuxHost {
    runner: Box<dyn Runner>,
    name: String,
    level: ServiceLevel,
}

impl LinuxHost {
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

    /// unit 名（`<name>.service`）。
    pub fn unit(&self) -> String {
        unit_name(&self.name)
    }

    /// unit 文件路径。
    pub fn path(&self) -> Option<PathBuf> {
        unit_path(&self.name, self.level)
    }

    /// 注入的执行器（真执行 / 假执行都从这一个口子出去）。
    fn runner(&self) -> &dyn Runner {
        self.runner.as_ref()
    }

    /// systemctl 的公共前缀参数（用户级要 `--user`）。
    fn systemctl(
        &self,
        args: impl IntoIterator<Item = impl Into<String>>,
        purpose: &str,
    ) -> CommandSpec {
        let mut argv = Vec::new();
        if self.level == ServiceLevel::User {
            argv.push("--user".to_owned());
        }
        argv.extend(args.into_iter().map(Into::into));
        CommandSpec::new("systemctl", argv, purpose)
    }

    fn show(&self) -> Result<String, ServiceError> {
        let spec = self.systemctl(
            [
                "show".to_owned(),
                self.unit(),
                "-p".to_owned(),
                "ActiveState".to_owned(),
                "-p".to_owned(),
                "SubState".to_owned(),
                "-p".to_owned(),
                "UnitFileState".to_owned(),
            ],
            "读取 systemd 单元状态",
        );
        crate::host::read_only(self.runner(), &spec)
    }

    fn cat(&self) -> Result<String, ServiceError> {
        let spec = self.systemctl(
            ["cat".to_owned(), self.unit()],
            "读取 systemd 单元正文（自检 Restart=）",
        );
        crate::host::read_only(self.runner(), &spec)
    }
}

impl ServiceHost for LinuxHost {
    fn status(&self) -> Result<ServiceStatus, ServiceError> {
        let stdout = match self.show() {
            Ok(stdout) => stdout,
            // `systemctl show` 对不存在的单元也会以 0 退出并打印一堆空属性；
            // 真的起不来（没装 systemd / 没权限）时才走到这里 → 当作未安装。
            Err(_) => return Ok(ServiceStatus::not_installed(&self.name)),
        };
        let state = inspect_systemd_state(&stdout);

        if state.active_state.is_none() && state.unit_file_state.is_none() {
            return Ok(ServiceStatus::not_installed(&self.name));
        }

        let unit_text = self.cat().unwrap_or_default();
        Ok(ServiceStatus {
            installed: true,
            running: state.running(),
            level: Some(self.level),
            autostart: Some(state.autostart()),
            name: self.name.clone(),
            binary_path: inspect_exec_start(&unit_text),
            requires_elevation: self.level == ServiceLevel::System,
            restart_policy_configured: inspect_restart_line(&unit_text).is_ok(),
            last_exit_code: None,
        })
    }

    fn install(&self, opts: &InstallOptions) -> Result<(), ServiceError> {
        crate::host::prepare_install(opts, self.level)?;
        // unit 名会变成 `/etc/systemd/system/<name>.service` 的一部分：
        // 带路径分隔符的名字等于让调用方写到任意路径。
        if !is_valid_unit_name(&opts.name) {
            return Err(ServiceError::invalid_options(format!(
                "服务名「{}」不是合法的 systemd unit 名（只允许字母数字与 : _ . - @，且不能带路径分隔符）",
                opts.name
            )));
        }

        let path = self.path().ok_or_else(|| {
            ServiceError::invalid_options("读不到 $HOME / $XDG_CONFIG_HOME，无法定位 unit 文件路径")
        })?;
        let file = FileInstall::new(
            path,
            unit_body(opts),
            if self.level == ServiceLevel::User {
                "写 systemd user unit（Restart=always）"
            } else {
                "写 systemd system unit（Restart=always）"
            },
        );

        // ⚠️ 顺序：写 unit → daemon-reload → enable（自启）→ start。
        // 先 enable 后 reload 的话 systemd 会抱怨「unit 文件不存在」。
        let reload = self.systemctl(["daemon-reload"], "让 systemd 重新读取 unit 文件");
        let mut plan = Plan::new().file(file).command(reload);
        if opts.autostart != Autostart::Off {
            plan = plan.command(self.enable_command(true));
        }
        plan = plan.command(self.systemctl(["start".to_owned(), self.unit()], "启动 systemd 服务"));
        plan.execute(self.runner())?;

        tracing::info!(
            event = "service.install",
            mode = ?self.level,
            autostart = ?opts.autostart,
            path = %opts.binary_path.display(),
            "已注册 systemd 服务"
        );

        self.verify_restart_policy()
    }

    fn uninstall(&self) -> Result<(), ServiceError> {
        let Some(path) = self.path() else {
            return Ok(());
        };

        // ⚠️ 先停止 + 关自启，再删 unit，最后 daemon-reload。
        // 顺序反了会得到一个「unit 文件没了但 systemd 还记着」的中间态。
        Plan::new()
            .command_ignoring_failure(self.systemctl(
                ["stop".to_owned(), self.unit()],
                "停止 systemd 服务（卸载前必须先停）",
            ))
            .command_ignoring_failure(self.enable_command(false))
            .remove(path)
            .command(self.systemctl(["daemon-reload"], "让 systemd 忘掉这个 unit"))
            .execute(self.runner())?;
        tracing::info!(event = "service.uninstall", mode = ?self.level, "已删除 systemd 服务");
        Ok(())
    }

    fn start(&self) -> Result<(), ServiceError> {
        let spec = self.systemctl(["start".to_owned(), self.unit()], "启动 systemd 服务");
        Plan::new().command(spec).execute(self.runner())
    }

    fn stop(&self) -> Result<(), ServiceError> {
        let spec = self.systemctl(["stop".to_owned(), self.unit()], "停止 systemd 服务");
        // `systemctl stop` 对「没在跑」的单元是成功的（幂等），不需要特殊处理。
        Plan::new().command(spec).execute(self.runner())
    }

    fn set_autostart(&self, on: bool) -> Result<(), ServiceError> {
        // `enable` / `disable` 不带 `--now`，启动与停止各有显式一步 —— 这样日志里
        // 「是哪一步失败」一眼可见（自启开关与进程状态本来就是两件事）。
        let mut plan = Plan::new().command(self.enable_command(on));
        if on {
            plan = plan
                .command(self.systemctl(["start".to_owned(), self.unit()], "开启自启后确保它在跑"));
        } else {
            plan =
                plan.command(self.systemctl(["stop".to_owned(), self.unit()], "关闭自启并停下来"));
        }
        plan.execute(self.runner())
    }

    fn verify_restart_policy(&self) -> Result<(), ServiceError> {
        // 首选 `systemctl show`（systemd 自己算出来的**有效值**，包含 drop-in 覆盖）。
        let show = self.systemctl(
            [
                "show".to_owned(),
                self.unit(),
                "-p".to_owned(),
                "Restart".to_owned(),
                "-p".to_owned(),
                "RestartUSec".to_owned(),
            ],
            "自检 systemd 的失败重启策略",
        );
        match crate::host::read_only(self.runner(), &show) {
            Ok(stdout) => return inspect_restart_properties(&stdout),
            Err(error) => tracing::debug!(
                event = "service.verify.fallback",
                %error,
                "systemctl show 读不到，退回 unit 正文"
            ),
        }

        // 退路：直接看 unit 正文（`systemctl cat` 会带上我们生成的注释）。
        let unit_text = self.cat()?;
        inspect_restart_line(&unit_text)
    }
}

impl LinuxHost {
    /// `enable` / `disable` 命令。
    ///
    /// `enable` 不带 `--now`：启动交给上一步显式的 `start`，两个动作分开才看得清是哪步失败。
    fn enable_command(&self, on: bool) -> CommandSpec {
        let purpose = if on {
            "开启自启（systemctl enable）"
        } else {
            "关闭自启（systemctl disable）"
        };
        CommandSpec::new(
            "systemctl",
            {
                let mut argv: Vec<String> = Vec::new();
                if self.level == ServiceLevel::User {
                    argv.push("--user".to_owned());
                }
                argv.push(if on { "enable" } else { "disable" }.to_owned());
                argv.push(self.unit());
                argv
            },
            purpose,
        )
    }
}

/// 从 unit 正文里读 `ExecStart=` 的二进制路径（去掉前导 `-`）。
pub fn inspect_exec_start(unit_text: &str) -> Option<String> {
    let value = unit_text.lines().map(str::trim).find_map(|line| {
        line.strip_prefix("ExecStart=")
            .or_else(|| {
                line.strip_prefix('#')
                    .map(str::trim)
                    .and_then(|line| line.strip_prefix("ExecStart="))
            })
            .map(|value| value.trim())
    })?;
    let value = value.strip_prefix('-').unwrap_or(value);
    let token = if let Some(rest) = value.strip_prefix('"') {
        rest.split('"').next().unwrap_or("")
    } else {
        value.split_whitespace().next().unwrap_or("")
    };
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::FakeRunner;

    fn sample_opts() -> InstallOptions {
        let mut opts = InstallOptions::new(
            "peon-burrow",
            "/home/me/.local/share/peon-burrow/bin/burrow",
        );
        opts.args = vec!["run".to_owned()];
        opts
    }

    /// `systemctl show` 的输出（只有被请求的属性）。
    const SHOW_FIXTURE: &str = "\
ActiveState=active
SubState=running
UnitFileState=enabled
";

    /// `systemctl show -p Restart -p RestartUSec` 的输出。
    const SHOW_RESTART_FIXTURE: &str = "Restart=always\nRestartUSec=2s\n";

    #[test]
    fn unit_carries_restart_and_install_section() {
        let unit = unit_body(&sample_opts());
        assert!(unit.contains("Restart=always"), "systemd 默认是 Restart=no");
        assert!(
            unit.contains("RestartSec=2"),
            "重启间隔要够短（adr-0003 § 1）"
        );
        assert!(unit.contains("[Install]") && unit.contains("WantedBy=default.target"));
        assert!(
            unit.contains("ExecStart=-/home/me/.local/share/peon-burrow/bin/burrow run"),
            "前导 - 让自更新的非零退出码不被当成启动失败：{unit}"
        );
        assert!(unit.contains("WorkingDirectory=/home/me/.local/share/peon-burrow/bin"));
    }

    #[test]
    fn unit_quotes_paths_with_spaces() {
        let opts = InstallOptions::new("relay", "/opt/my relay/burrow");
        let unit = unit_body(&opts);
        assert!(
            unit.contains(r#"ExecStart=-"/opt/my relay/burrow""#),
            "带空格的路径要引用：{unit}"
        );
    }

    #[test]
    fn unit_name_is_normalized() {
        assert_eq!(unit_name("relay"), "relay.service");
        assert_eq!(unit_name("relay.service"), "relay.service");
        assert!(is_valid_unit_name("peon-burrow"));
        assert!(is_valid_unit_name("my.app@1"));
        assert!(
            !is_valid_unit_name("../etc/cron.d/x"),
            "不能借 unit 名写到任意路径"
        );
        assert!(!is_valid_unit_name("a/b"));
    }

    #[test]
    fn restart_properties_are_read_from_systemctl_show() {
        inspect_restart_properties(SHOW_RESTART_FIXTURE).expect("Restart=always 应当通过");
        // on-failure 也算「失败会回来」
        inspect_restart_properties("Restart=on-failure\nRestartUSec=2s\n")
            .expect("on-failure 通过");

        let error = inspect_restart_properties("Restart=no\nRestartUSec=100ms\n")
            .expect_err("Restart=no 就必须报错");
        assert!(
            error.to_string().contains("Restart=no"),
            "要报出读到的实际值：{error}"
        );
        assert!(error.to_string().contains("RestartSec"));
        assert!(
            error.action().contains("install"),
            "要给下一步：{}",
            error.action()
        );

        assert!(
            inspect_restart_properties("").is_err(),
            "读不到 Restart= 时绝不能当作通过"
        );
    }

    #[test]
    fn restart_line_fallback_reads_the_unit_text() {
        let unit = unit_body(&sample_opts());
        inspect_restart_line(&unit).expect("自己生成的 unit 必须通过");
        assert!(inspect_restart_line("[Service]\nType=simple\n").is_err());
        // `systemctl cat` 会在正文前加 `# /path/to/unit` 注释行，不能干扰解析
        let catted = format!("# /home/me/.config/systemd/user/peon-burrow.service\n{unit}");
        inspect_restart_line(&catted).expect("带 `systemctl cat` 头部的输出也要能解析");
    }

    #[test]
    fn systemd_state_parsing() {
        let state = inspect_systemd_state(SHOW_FIXTURE);
        assert_eq!(state.active_state.as_deref(), Some("active"));
        assert_eq!(state.sub_state.as_deref(), Some("running"));
        assert!(state.running());
        assert_eq!(state.autostart(), Autostart::Boot);

        let idle =
            inspect_systemd_state("ActiveState=inactive\nSubState=dead\nUnitFileState=disabled\n");
        assert!(!idle.running(), "「已安装·未运行」是正常状态");
        assert_eq!(idle.autostart(), Autostart::Off);

        let empty = inspect_systemd_state("ActiveState=\nSubState=\nUnitFileState=\n");
        assert_eq!(empty, SystemdState::default(), "空值不该被当成有效状态");
    }

    #[test]
    fn exec_start_is_read_back_without_the_dash_or_quotes() {
        let unit = unit_body(&sample_opts());
        assert_eq!(
            inspect_exec_start(&unit).as_deref(),
            Some("/home/me/.local/share/peon-burrow/bin/burrow")
        );
        let quoted = unit_body(&InstallOptions::new("relay", "/opt/my relay/burrow"));
        assert_eq!(
            inspect_exec_start(&quoted).as_deref(),
            Some("/opt/my relay/burrow")
        );
    }

    #[test]
    fn install_rejects_a_name_that_could_write_outside_the_unit_directory() {
        // unit 名会拼进 `/etc/systemd/system/<name>.service`：带 `../` 的名字等于任意写。
        let runner = FakeRunner::new();
        let host = LinuxHost::with_runner("peon-burrow", ServiceLevel::User, runner.clone());
        let mut opts = sample_opts();
        opts.name = "../../etc/cron.d/evil".to_owned();

        let error = host.install(&opts).expect_err("非法 unit 名必须被拒");
        assert!(matches!(error, ServiceError::InvalidOptions { .. }));
        assert!(
            runner.mutations().run_count() == 0,
            "校验失败时一条命令都不该发"
        );
    }

    #[test]
    fn user_install_writes_unit_reloads_enables_starts_then_self_checks() {
        let runner = FakeRunner::new()
            .expect("") // daemon-reload
            .expect("") // enable
            .expect("") // start
            .expect(SHOW_RESTART_FIXTURE); // 自检

        let host = LinuxHost::with_runner("peon-burrow", ServiceLevel::User, runner.clone());
        host.install(&sample_opts()).expect("install");

        let mutations = runner.mutations();
        let lines = mutations.command_lines();
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert!(
            lines[0].contains("systemctl --user daemon-reload"),
            "{}",
            lines[0]
        );
        assert!(lines[1].contains("systemctl --user enable peon-burrow.service"));
        assert!(lines[2].contains("systemctl --user start peon-burrow.service"));
        assert!(
            lines[3].contains("systemctl --user show"),
            "自检要读回来：{}",
            lines[3]
        );
        assert_eq!(mutations.files().len(), 1, "unit 必须先落盘");
        assert!(mutations.file_content(0).contains("Restart=always"));
    }

    #[test]
    fn install_skips_enable_when_autostart_is_off() {
        let runner = FakeRunner::new()
            .expect("") // daemon-reload
            .expect("") // start
            .expect(SHOW_RESTART_FIXTURE);
        let host = LinuxHost::with_runner("peon-burrow", ServiceLevel::User, runner.clone());
        let mut opts = sample_opts();
        opts.autostart = Autostart::Off;
        host.install(&opts).expect("install");

        let lines = runner.mutations().command_lines();
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(
            !lines.iter().any(|line| line.contains("enable")),
            "Autostart::Off 不该发 enable：{lines:?}"
        );
    }

    #[test]
    fn install_fails_when_restart_is_not_always() {
        let runner = FakeRunner::new()
            .expect("")
            .expect("")
            .expect("")
            // 自检：Restart=no —— 这正是「装完看着挺好、崩了不回来」
            .expect("Restart=no\nRestartUSec=100ms\n");
        let host = LinuxHost::with_runner("peon-burrow", ServiceLevel::User, runner);
        let error = host
            .install(&sample_opts())
            .expect_err("Restart=no 必须报错");
        assert!(matches!(error, ServiceError::RestartPolicyMissing { .. }));
    }

    #[test]
    fn verify_falls_back_to_the_unit_text() {
        let runner = FakeRunner::new()
            // systemctl show -p Restart 失败（老版本 / 权限问题）
            .expect_failure("Failed to get properties: Access denied")
            // 退回 systemctl cat
            .expect(unit_body(&sample_opts()));
        let host = LinuxHost::with_runner("peon-burrow", ServiceLevel::User, runner.clone());
        host.verify_restart_policy().expect("退路也要能自检通过");

        let lines = runner.mutations().command_lines();
        assert!(lines[1].contains("systemctl --user cat peon-burrow.service"));
    }

    #[test]
    fn system_level_uses_no_user_flag() {
        let runner = FakeRunner::new()
            .expect("") // daemon-reload
            .expect("") // enable
            .expect("") // start
            .expect(SHOW_RESTART_FIXTURE);
        let host = LinuxHost::with_runner("peon-burrow", ServiceLevel::System, runner.clone());
        let mut opts = sample_opts();
        opts.level = ServiceLevel::System;
        opts.autostart = Autostart::Boot;
        host.install(&opts).expect("install");

        let lines = runner.mutations().command_lines();
        assert!(
            lines[0].starts_with("systemctl daemon-reload"),
            "{}",
            lines[0]
        );
        assert!(
            !lines.iter().any(|line| line.contains("--user")),
            "系统级不能带 --user：{lines:?}"
        );
        match runner.mutations().files().first() {
            Some(crate::runner::Mutation::Install(file)) => assert_eq!(
                file.path.display().to_string(),
                "/etc/systemd/system/peon-burrow.service"
            ),
            other => panic!("第 1 个动作应当是写 unit：{other:?}"),
        }
    }

    #[test]
    fn uninstall_stops_disables_removes_then_reloads() {
        let mut runner = FakeRunner::new();
        // stop / disable 用 best-effort：即使失败也要继续
        runner = runner.expect_failure("Unit peon-burrow.service not loaded.");
        runner = runner.expect_failure("Unit file peon-burrow.service does not exist.");
        runner = runner.expect(""); // daemon-reload
        let host = LinuxHost::with_runner("peon-burrow", ServiceLevel::User, runner.clone());
        host.uninstall()
            .expect("卸载要幂等：停不掉的服务也要能删掉");

        let mutations = runner.mutations();
        let lines = mutations.command_lines();
        assert!(lines[0].contains("stop"));
        assert!(lines[1].contains("disable"));
        assert!(lines[2].contains("daemon-reload"));
        assert!(
            matches!(mutations.nth(3), Some(crate::runner::Mutation::Remove(_))),
            "unit 文件要在停止/关自启之后删：{:?}",
            mutations.all()
        );
    }

    #[test]
    fn status_reports_not_installed_when_systemctl_is_absent() {
        let runner = FakeRunner::new().expect_failure("systemctl: command not found");
        let host = LinuxHost::with_runner("peon-burrow", ServiceLevel::User, runner);
        let status = host.status().expect("探测不到 systemd 不算致命错误");
        assert!(!status.installed);
    }

    #[test]
    fn status_reads_show_and_cat() {
        let runner = FakeRunner::new()
            .expect(SHOW_FIXTURE)
            .expect(unit_body(&sample_opts()));
        let host = LinuxHost::with_runner("peon-burrow", ServiceLevel::User, runner);
        let status = host.status().expect("status");
        assert!(status.installed);
        assert!(status.running);
        assert_eq!(status.autostart, Some(Autostart::Boot));
        assert!(
            status.restart_policy_configured,
            "Restart=always 在，就该报 true"
        );
        assert_eq!(
            status.binary_path.as_deref(),
            Some("/home/me/.local/share/peon-burrow/bin/burrow")
        );
        assert!(!status.requires_elevation, "用户级零提权");
    }

    #[test]
    fn xdg_fallback_is_documented_not_used() {
        let body = xdg_autostart_body(&sample_opts());
        assert!(body.contains("Type=Application"));
        assert!(body.contains("Exec=/home/me/.local/share/peon-burrow/bin/burrow run"));
        assert!(
            !body.contains("Restart="),
            ".desktop 没有失败重启能力 —— 这条退路必须让用户知道"
        );
        assert!(body.contains("无失败重启"));
    }
}
