//! **服务形态**：被服务管理器（Windows SCM / systemd / launchd）拉起来的那个进程。
//!
//! 与前台形态（`burrow run`）的差别：
//!
//! | | 前台 | 服务态（`run --service <NAME>`） |
//! | --- | --- | --- |
//! | 停止信号 | Ctrl+C、控制面 `stop` | 服务管理器的控制码、控制面 `stop` |
//! | 日志 | stderr | 落盘（没有终端可看） |
//! | 控制面通道 | 命名管道 | loopback TCP（与桌面端不在同一会话 / 完整性级别） |
//!
//! ⚠️ Windows 上还有第四件事，也正是这个模块存在的**根本原因**：`sc create` 只是「注册」，
//! SCM 启动进程之后要求它在 30 秒内通过 `StartServiceCtrlDispatcher` 报到、把状态转成
//! `SERVICE_RUNNING`。不报到就是 `sc start` 的 **1053**「服务没有及时响应启动或控制请求」
//! —— 服务装得上、永远起不来（真机踩过）。
//!
//! 分工：**外部注册**（`sc.exe` 建/删/启停、写失败重启策略）在 `peon-burrow-service`；
//! 进程内部这一半在这里。`windows-service` crate 只在这个文件里出现。

use tokio::sync::watch;

/// 服务管理器要求停下的信号（中继侧拿着它等）。
///
/// 刻意不做成 `Ctrl+C` 的替身：服务进程没有控制台，SCM 的控制码是**另一个线程**上
/// 同步回调进来的，所以这里用 `watch`（同步可写、异步可等）。
#[derive(Debug, Clone)]
pub(crate) struct ServiceStop {
    rx: watch::Receiver<bool>,
}

impl ServiceStop {
    /// 等到「服务管理器要求停下」为止。
    ///
    /// 信号端整个消失（服务入口已经退出了）也返回：那种情况下进程本来就在往外走。
    pub(crate) async fn requested(&mut self) {
        loop {
            if *self.rx.borrow_and_update() {
                return;
            }
            if self.rx.changed().await.is_err() {
                return;
            }
        }
    }
}

/// 信号端：由服务入口（控制码回调）持有。
///
/// ⚠️ 只在 **Windows** 上被构造（`stop_channel` 的调用点在 `windows_scm` 模块里）：
/// Linux/macOS 的服务形态由服务管理器直接跑 `burrow run`，没有 SCM 那样的控制码通道。
/// 非 Windows 构建里它是「存在但没人用」——`-D warnings` 会把 dead_code 当错误，
/// 所以这里按平台放行（Windows 上仍然严格检查）。
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone)]
pub(crate) struct ServiceStopHandle {
    tx: watch::Sender<bool>,
}

impl ServiceStopHandle {
    /// 请求中继优雅停下。
    ///
    /// ⚠️ 会在 SCM 的**回调线程**上被调用，所以必须立刻返回（`watch` 的写入是同步的，
    /// 这也正是选它的原因）—— 在控制码回调里等中继退出，SCM 会以为服务卡死。
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn request_stop(&self) {
        self.tx.send_replace(true);
    }
}

/// 造一对「信号端 + 等待端」。
///
/// 调用点只有 Windows 的 SCM 那一半（理由见 [`ServiceStopHandle`]）。
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn stop_channel() -> (ServiceStopHandle, ServiceStop) {
    let (tx, rx) = watch::channel(false);
    (ServiceStopHandle { tx }, ServiceStop { rx })
}

#[cfg(windows)]
pub use windows_scm::run_as_service;

/// Windows SCM 那一半（`windows-service` crate 的唯一落点）。
#[cfg(windows)]
mod windows_scm {
    use std::time::Duration;

    use clap::Parser as _;
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::{define_windows_service, service_dispatcher};

    use super::stop_channel;
    use crate::cli::{Cli, Command};
    use crate::exit::ExitCode;

    /// 报到用的时间估计。
    ///
    /// 中继的就绪**不在这里等**（那是安装时的事：`service install` 会读控制面，
    /// 见 `service-lifecycle.md § 3` 第 8 步）：SCM 的握手只要「立刻有人应答」，
    /// 把就绪也塞进来只会让初始化慢的时候被判 1053。
    const START_WAIT_HINT: Duration = Duration::from_secs(30);

    define_windows_service!(ffi_service_main, service_main);

    /// 以服务形态跑：起 SCM dispatcher，**阻塞到服务停止**。
    ///
    /// 必须在**主线程**上调用（dispatcher 要占着它）。
    pub fn run_as_service(name: &str) -> ExitCode {
        match service_dispatcher::start(name, ffi_service_main) {
            // 正常情况下走不到这里：`service_main` 收尾时会自己 `process::exit()`
            //（dispatcher 不会因为服务停了就返回，理由见那里的注释）。
            Ok(()) => ExitCode::Ok,
            Err(error) => {
                // `windows-service` 的 `Display` 对 winapi 错误只说「IO error in winapi call」，
                // 真正有用的是里面那个 `io::Error`（1063 = ERROR_FAILED_SERVICE_CONTROLLER_CONNECT：
                // 进程不是 SCM 拉起来的 —— 用户手敲了 `run --service …`，
                // 或者命令行里的名字与注册的服务名对不上）。
                let detail = match &error {
                    windows_service::Error::Winapi(io) => io.to_string(),
                    other => other.to_string(),
                };
                eprintln!("服务形态启动失败：{detail}");
                eprintln!(
                    "下一步：手敲请改用 `burrow run`；`--service <名字>` 只在由服务管理器拉起时用，\
                     且名字必须与注册进 SCM 的服务名一致（看 `sc qc <名字>`）"
                );
                ExitCode::Runtime
            }
        }
    }

    /// SCM 的 `service_main`：报到 → 跑中继 → 按退出码决定要不要「不报 STOPPED」。
    fn service_main(_arguments: Vec<std::ffi::OsString>) {
        // 与服务入口用同一个解析器：SCM 注册的命令行就是 `run --service <NAME> …`
        //（SCM 传给 service_main 的 argv 只有 `sc start` 带的参数，不是 binPath 里的参数）。
        let cli = Cli::parse();
        let name = cli.service.clone().unwrap_or_default();

        if !matches!(cli.command, Some(Command::Run { .. })) {
            // 参数是我们自己写进注册信息的，走到这里说明注册信息被人改坏了。
            // 这里也**不能只 return**（dispatcher 会把进程留成僵尸），必须退出去。
            eprintln!("`--service` 只能与 `burrow run` 一起用");
            std::process::exit(i32::from(ExitCode::Config.as_u8()));
        }

        run_service(cli, &name);
    }

    /// 注册控制码处理、报到、把中继放到工作线程上跑。
    ///
    /// **不会返回**：收尾时按退出码决定要不要报 STOPPED，然后自己 `process::exit()`。
    fn run_service(cli: Cli, name: &str) -> ! {
        let (handle, stop) = stop_channel();

        // 控制码：停止 / 关机 → 请中继优雅退出；Interrogate 是**必须**应答的空操作。
        let event_handler = move |control| match control {
            ServiceControl::Stop | ServiceControl::Shutdown | ServiceControl::Preshutdown => {
                handle.request_stop();
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        };

        let status = match service_control_handler::register(name, event_handler) {
            Ok(status) => status,
            Err(error) => {
                eprintln!(
                    "注册服务控制码处理失败：{error}（注册名 {name:?} 与 `sc qc` 里的名字对得上吗）"
                );
                std::process::exit(i32::from(ExitCode::Runtime.as_u8()));
            }
        };

        // 先报「正在启动」：初始化再快也先把估计时间告诉 SCM，别让它按默认的 30 秒判死。
        let _ = status.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::StartPending,
            // pending 期间不接受控制码
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::NO_ERROR,
            checkpoint: 1,
            wait_hint: START_WAIT_HINT,
            process_id: None,
        });

        // 中继跑在**另一个线程**上：dispatcher 占着主线程，而它必须能随时收控制码。
        let worker = std::thread::spawn(move || crate::run::run_service_worker(cli, stop));

        let _ = status.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            exit_code: ServiceExitCode::NO_ERROR,
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        });

        let code = worker.join().unwrap_or(ExitCode::Runtime);

        if code == ExitCode::Ok {
            // 干净停下（控制面 `stop` / `sc stop`）：**必须**明确报 STOPPED，
            // 否则 SCM 会把这次退出当成崩溃。
            let _ = status.set_service_status(ServiceStatus {
                service_type: ServiceType::OWN_PROCESS,
                current_state: ServiceState::Stopped,
                controls_accepted: ServiceControlAccept::empty(),
                exit_code: ServiceExitCode::Win32(0),
                checkpoint: 0,
                wait_hint: Duration::default(),
                process_id: None,
            });
        } else {
            // 非零退出（出错 / 自更新后请求重启）：**故意不报 STOPPED**。
            // `SERVICE_FAILURE_ACTIONS_FLAG` 默认 FALSE 时，`sc failure` 写进去的重启动作
            // 只在「进程没报 STOPPED 就退出」时执行 —— 这正是自更新后能自己回来的原因
            //（见 `service-lifecycle.md § 5` 与 `adr-0005`）。
            tracing::warn!(
                code = code.as_u8(),
                "中继非正常退出：不向 SCM 报 STOPPED，交给失败重启策略"
            );
        }

        // ⚠️ 必须**自己退出进程**，不能只 `return`：
        // `StartServiceCtrlDispatcherW` 要等到「进程里所有服务都停了」才返回，而它的判据是
        // 服务**报了 STOPPED**。非零退出那条路（上面）故意不报，于是 dispatcher 永远不返回
        // —— 进程会以「SCM 认为在跑、中继其实早停了」的僵尸形态挂着（真机踩过：
        // `sc query` 显示 RUNNING、端口没人监听、`control.json` 也没了，而且它还锁着
        // 安装目录里的 exe，下一次安装复制不进去）。
        //
        // 这是「退出码只有一个来源（约定 C2）」的**唯一例外**，也只能是例外：
        // 退出码必须在这一刻交给操作系统，否则会留下一个永远不退出、又不干活的进程。
        std::process::exit(i32::from(code.as_u8()));
    }
}
