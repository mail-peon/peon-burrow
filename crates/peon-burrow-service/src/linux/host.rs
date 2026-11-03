use super::inspect::{
    inspect_exec_start, inspect_restart_line, inspect_restart_properties, inspect_systemd_state,
};
use super::unit::{is_valid_unit_name, unit_body, unit_name, unit_path};
use crate::host::{
    Autostart, InstallOptions, Plan, ServiceError, ServiceHost, ServiceLevel, ServiceStatus,
};
use crate::runner::{CommandSpec, FileInstall, Runner};
use std::path::PathBuf;

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
