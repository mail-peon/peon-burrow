use super::paths::{domain, label_of, plist_path};
use super::plist::{
    LaunchdState, inspect_binary_path, inspect_keep_alive, inspect_launchd, inspect_run_at_load,
    plist_body, with_run_at_load,
};
use crate::host::{
    Autostart, InstallOptions, Plan, ServiceError, ServiceHost, ServiceLevel, ServiceStatus,
};
use crate::runner::{CommandSpec, FileInstall, Runner};
use std::path::PathBuf;

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
