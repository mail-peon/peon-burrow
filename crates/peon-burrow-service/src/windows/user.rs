use super::USER_TASK_FOLDER;
use super::command::{escape_powershell_single_quotes, task_name};
use super::task::{
    inspect_autostart, inspect_command, inspect_last_result, task_xml, verify_task_xml,
};
use crate::host::{InstallOptions, Plan, ServiceError, ServiceHost, ServiceLevel, ServiceStatus};
use crate::runner::{CommandSpec, Runner};

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
        // ⚠️ 必须先停（`service-lifecycle.md § 4`）：任务删掉不等于进程结束 ——
        // 正在跑的中继会继续活着（「卸载了但扩展还能收信」）。`stop()` 是幂等的，
        // 没在跑时一条命令都不发。
        self.stop()?;
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
