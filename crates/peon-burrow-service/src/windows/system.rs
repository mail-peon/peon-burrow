use super::command::command_line_of;
use super::scm::{
    inspect_sc_autostart, inspect_sc_binary_path, inspect_sc_exit_code, service_is_running,
    service_is_stopped, verify_failure_actions,
};
use super::{STOP_POLL_INTERVAL, STOP_TIMEOUT, SYSTEM_RESTART_DELAY_MS, SYSTEM_RESTART_RESET_SECS};
use crate::host::{
    Autostart, InstallOptions, Plan, ServiceError, ServiceHost, ServiceLevel, ServiceStatus,
};
use crate::runner::{CommandSpec, Runner};
use std::time::Duration;

/// Windows 系统级宿主：**SCM**（`sc.exe`），安装时一次 UAC。
///
/// 刻意**不**引入 `windows-service` crate（`modules.md § 5` 的表格里写了它，
/// 但本 crate 的依赖白名单只有 `ipc-types` / `thiserror` / `tracing` / `sysinfo`；
/// 而且 `windows-service` 是给「服务进程内部」用的，与「外部注册服务」正好是两件事）。
#[derive(Debug)]
pub struct WindowsSystemHost {
    runner: Box<dyn Runner>,
    name: String,
    /// 等「停下来」的上限（`sc stop` 只是请求）。
    stop_timeout: Duration,
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
            stop_timeout: STOP_TIMEOUT,
        }
    }

    /// 改掉「等它停下来」的上限（**测试用**：超时那一条路不该真等 10 秒）。
    #[must_use]
    pub fn with_stop_timeout(mut self, timeout: Duration) -> Self {
        self.stop_timeout = timeout;
        self
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

    /// 服务现在是不是**停着**（`sc query` 报 `STOPPED`）。
    ///
    /// 查不出来（1060 = 没装、`sc` 起不来）也当「停着」：没有活着的服务要停。
    fn is_stopped(&self) -> bool {
        let spec = CommandSpec::new("sc", ["query", &self.name], "读取系统服务运行状态");
        match crate::host::read_only(self.runner(), &spec) {
            Ok(stdout) => service_is_stopped(&stdout),
            Err(_) => true,
        }
    }

    /// 等它真的报 `STOPPED`（最多 `stop_timeout`）。
    ///
    /// `sc stop` 只是**请求**：SCM 把控制码递出去就返回，进程可能还在退出。
    /// 这一步是卸载前的最后一道闸 —— `sc delete` 只做标记，带着一个活进程删掉就是
    /// 「卸载了但扩展还能收信」（`service-lifecycle.md § 4`）。
    fn wait_until_stopped(&self) -> Result<(), ServiceError> {
        let deadline = std::time::Instant::now() + self.stop_timeout;
        loop {
            if self.is_stopped() {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(ServiceError::StopTimedOut {
                    name: self.name.clone(),
                    seconds: self.stop_timeout.as_secs(),
                });
            }
            std::thread::sleep(STOP_POLL_INTERVAL);
        }
    }
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

        // `sc query` 只有状态（`STATE : 4 RUNNING`）；启动类型与二进制路径**只有 `sc qc` 有**
        //（`sc query` 不打印 `START_TYPE` / `BINARY_PATH_NAME`）。用错那条命令的后果是
        // 「自启永远显示已关闭、路径永远为空」——而它们都是界面上看得见的字段（真机踩过）。
        let config_spec = CommandSpec::new("sc", ["qc", &self.name], "读取系统服务配置");
        let config = crate::host::read_only(self.runner(), &config_spec).unwrap_or_default();

        let failure = failure_actions_query(&self.name);
        let failure_stdout = crate::host::read_only(self.runner(), &failure).unwrap_or_default();

        let running = service_is_running(&stdout);
        Ok(ServiceStatus {
            installed: true,
            running,
            level: Some(ServiceLevel::System),
            autostart: Some(inspect_sc_autostart(&config)),
            name: self.name.clone(),
            binary_path: inspect_sc_binary_path(&config),
            // 系统服务：装 / 卸 / 启 / 停都要管理员；读状态不要。
            requires_elevation: true,
            restart_policy_configured: verify_failure_actions(&failure_stdout, &self.name).is_ok(),
            // 只在**没在跑**时才有意义（在跑时那个码是上一次的残留）。
            // 「装上了但一起来就退」正是最需要看见它的时候。
            last_exit_code: (!running).then(|| inspect_sc_exit_code(&stdout)).flatten(),
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
                bin_path.clone(),
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

        // ⚠️ **幂等**：服务已经存在时 `sc create` 报 1073（真机踩过 —— 上一次失败留下的
        // 半成品会把后面每一次重装都挡住，用户只能自己去 `sc delete`）。
        // 已存在就跳过创建，只把描述/失败策略/启动补齐到期望状态。
        let exists = self.exists()?;
        let mut plan = Plan::new();
        if !exists {
            plan = plan.command(create);
        } else {
            // ⚠️ 已存在时**必须**把命令行刷新成这次要注册的那份：注册信息里的 `binPath`
            // 是我们写的，而它随版本变（例如系统服务从 `"…\burrow.exe" run` 变成
            // `"…\burrow.exe" run --service <名字> --config … --data-dir …`）。
            // 不刷新的话 `service install` 会「成功」，而服务跑的仍是老命令行 —— 老命令行
            // 里没有 `--service`，于是每次启动都不报到、`sc start` 报 1053（真机踩过）。
            plan = plan.command(CommandSpec::new(
                "sc",
                [
                    "config".to_owned(),
                    opts.name.clone(),
                    // 同 `create`：选项名与值必须分开传（否则 1639「无效 start= 参数」）
                    "binPath=".to_owned(),
                    bin_path.clone(),
                    "start=".to_owned(),
                    start_type.to_owned(),
                ],
                "刷新系统服务的启动命令行",
            ));
            tracing::info!(
                event = "service.install",
                mode = "system",
                "服务已存在，跳过创建（命令行会被刷新）"
            );
        }

        plan.command(description)
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
        // `stop()` 自己会**核实**真的停了（不是只看 `sc stop` 的退出码），
        // 停不下来会带着可照做的文案报错 —— 这样「带着活进程去 delete」就不可能发生。
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
        // 幂等：**已经停着不是错误**。
        //
        // 真机踩过：中继自己从控制面停下来（如实报 `SERVICE_STOPPED`）之后再点「停止」或
        // 「卸载」，`sc stop` 报 **1061**「服务无法在此时接受控制信息」——
        // 服务处于 `STOPPED` 时 `dwControlsAccepted` 本来就是 0，SCM 对没有声明
        // `SERVICE_ACCEPT_STOP` 的服务就是回 1061（老实现只容忍了 1062 / 1060）。
        // 于是「**已经停好**」这个正确状态被当成失败，卸载直接死在这一步。
        //
        // 所以先问状态：停着就一条命令都不发（顺带省一次 `sc` 调用）。
        if self.is_stopped() {
            return Ok(());
        }

        let spec = CommandSpec::new("sc", ["stop", &self.name], "停止系统服务");
        match Plan::new().command(spec).execute(self.runner()) {
            Ok(()) => {}
            Err(error) => {
                let text = error.to_string();
                // 1060 = 服务不存在；1061 = 无法在此时接受控制；1062 = 服务未启动。
                // 这三条都**不能**直接当失败：它们可能是「其实已经停了 / 正在停」的竞态。
                // 真的停没停由下面的核实说了算 —— 核实才是这道闸的判据，不是 `sc` 的退出码。
                if !(text.contains("1060") || text.contains("1061") || text.contains("1062")) {
                    return Err(error);
                }
            }
        }

        self.wait_until_stopped()
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
        let failure = failure_actions_query(&self.name);
        let stdout = crate::host::read_only(self.runner(), &failure)?;
        verify_failure_actions(&stdout, &self.name)
    }
}

impl WindowsSystemHost {
    /// 服务注册过没有（`sc query` 对没装的服务返回 1060）。
    fn exists(&self) -> Result<bool, ServiceError> {
        let query = CommandSpec::new("sc", ["query", &self.name], "检查系统服务是否存在");
        match crate::host::read_only(self.runner(), &query) {
            Ok(stdout) => Ok(stdout.to_ascii_uppercase().contains("SERVICE_NAME")),
            // 查询失败（1060 = 没装）不算错误：这正是「不存在」的答案
            Err(_) => Ok(false),
        }
    }

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

/// 读失败重启策略的命令：**查注册表**，不查 `sc qfailure` 的文字输出。
///
/// 注册表里的 `FailureActions` 是 `REG_BINARY`：语言无关，而且就是 SCM 实际执行的那份数据。
fn failure_actions_query(name: &str) -> CommandSpec {
    CommandSpec::new(
        "reg",
        [
            "query".to_owned(),
            format!(r"HKLM\SYSTEM\CurrentControlSet\Services\{name}"),
            "/v".to_owned(),
            "FailureActions".to_owned(),
        ],
        "读取系统服务的失败重启策略",
    )
}
