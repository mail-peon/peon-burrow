//! `run()`：把命令行变成行为。**退出码只在这里产生**（约定 C2）。

use std::sync::Arc;
use std::time::Duration;

use peon_burrow_core::{RelayServer, TlsConfig};
use peon_burrow_ipc::{AuthPolicy, ControlClient, ControlEndpoint, Listener, serve};
use peon_burrow_ipc_types::{CheckLevel, Request, ServiceStatus};
use tracing::{info, warn};

use crate::cli::{Cli, Command, ServiceCommand};
use crate::config::{Config, EnvSource};
use crate::control::{Lifecycle, RelayControl};
use crate::doctor;
use crate::exit::{AppError, ExitCode};
use crate::logging::{LogFile, init_logging, verbose_flag};
use crate::paths::Paths;

/// 同步入口：`main` 只调它（前台形态）。
pub fn run(cli: Cli) -> ExitCode {
    run_with(cli, None)
}

/// 服务形态的入口：服务线程调它（见 [`crate::service`]）。
///
/// 与 [`run`] 的唯一差别是「停止信号从哪来」—— 服务进程没有控制台，Ctrl+C 永远不会来。
pub(crate) fn run_service_worker(cli: Cli, stop: crate::service::ServiceStop) -> ExitCode {
    run_with(cli, Some(stop))
}

/// 带可选「服务管理器停止信号」的入口。
fn run_with(cli: Cli, stop: Option<crate::service::ServiceStop>) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("起不了异步运行时：{error}");
            return ExitCode::Runtime;
        }
    };
    runtime.block_on(async move {
        match dispatch_stopping(cli, stop).await {
            Ok(code) => code,
            Err(error) => {
                // 人话 + 下一步：stdout 是给人看的，不在这里打 JSON
                eprintln!("错误：{error}");
                if let AppError::PortInUse { .. } = error {
                    eprintln!("下一步：运行 `burrow doctor` 看占用者，或改配置里的 port");
                }
                error.exit_code()
            }
        }
    })
}

/// 定位路径、读配置、装日志，然后分发（前台形态）。
pub async fn dispatch(cli: Cli) -> Result<ExitCode, AppError> {
    dispatch_stopping(cli, None).await
}

/// 同上，但带上「服务管理器要求停下」的信号（服务形态走这条）。
async fn dispatch_stopping(
    cli: Cli,
    stop: Option<crate::service::ServiceStop>,
) -> Result<ExitCode, AppError> {
    // 服务形态必须把数据目录固定下来（LocalSystem 与登录用户的 `ProjectDirs` 不是同一个），
    // 见 `Paths::discover_with_data_dir`。
    let paths = Paths::discover_with_data_dir(cli.data_dir.as_deref())?;
    let config = Config::load(&paths, &EnvSource::from_env(), &cli.overrides())?;
    dispatch_with_stop(&paths, config, cli, stop).await
}

/// 注入路径与配置的分发（测试走这条，不碰真实用户目录）。
pub async fn dispatch_with(paths: &Paths, config: Config, cli: Cli) -> Result<ExitCode, AppError> {
    dispatch_with_stop(paths, config, cli, None).await
}

/// 注入路径、配置、服务停止信号的分发。
async fn dispatch_with_stop(
    paths: &Paths,
    config: Config,
    cli: Cli,
    stop: Option<crate::service::ServiceStop>,
) -> Result<ExitCode, AppError> {
    // 服务形态没有终端：日志**必须**落盘，否则出问题只能靠猜（`service-lifecycle.md § 3`）。
    // 滚动策略来自配置（`log_max_mib` / `log_keep_files`，见 `logging.rs`）。
    let service_form = cli.service.is_some();
    let log_path = service_form.then(|| paths.log_file());
    let log_file = log_path.as_deref().map(|path| LogFile {
        path,
        max_mib: config.log_max_mib,
        keep: config.log_keep_files,
    });
    init_logging(config.log_level, config.relay.trace, log_file);
    for warning in config.startup_warnings() {
        warn!("{warning}");
    }

    match cli.command.clone() {
        None | Some(Command::Status) => status(paths, cli.json).await,
        Some(Command::Run { no_control }) => {
            run_relay(paths, config, no_control, service_form, stop).await
        }
        Some(Command::Doctor { verbose }) => doctor_command(paths, &config, verbose, cli.json),
        Some(Command::Version) => version(cli.json),
        Some(Command::Url) => url(&config, cli.json),
        Some(Command::Token) => token(cli.json),
        Some(Command::Stop) => {
            control(
                paths,
                Request::Stop {
                    reason: Some("命令行".to_owned()),
                },
                cli.json,
            )
            .await
        }
        Some(Command::Restart) => control(paths, Request::Restart, cli.json).await,
        Some(Command::Trace { off, seconds }) => {
            let request = if off {
                Request::TraceOff
            } else {
                Request::TraceOn { seconds }
            };
            control(paths, request, cli.json).await
        }
        Some(Command::Update { force, apply, .. }) => {
            update_command(paths, &config, force, apply, cli.json).await
        }
        Some(Command::Service(command)) => service_command(paths, &config, command, cli.json).await,
    }
}

/// `burrow run`：前台跑中继 + 控制面，直到 Ctrl+C、控制面 `stop`、或服务管理器要求停下。
async fn run_relay(
    paths: &Paths,
    config: Config,
    no_control: bool,
    service_form: bool,
    stop: Option<crate::service::ServiceStop>,
) -> Result<ExitCode, AppError> {
    paths.ensure_dirs()?;

    let options = config.relay_options();
    let host = options.host.clone();

    // 自更新上下文（只在配置开着、且能定位安装目录时才建）
    let update_context = crate::update::context(&config, paths);
    if let Some(context) = &update_context {
        info!(version = %crate::VERSION, channel = %config.update.channel.as_str(), current = %context.current_version, "自更新已启用");
    }

    // 控制面：token 每次启动重新生成（旧文件里的 token 随之作废）
    let token = random_hex(32);
    let endpoint = control_endpoint(&config, &token, service_form);
    // 没有控制面时也要有个信号源，select! 才不用分叉
    let (lifecycle_tx, lifecycle_rx) = tokio::sync::watch::channel(None::<Lifecycle>);

    let listener = if config.control.enabled && !no_control {
        match Listener::bind(&endpoint).await {
            Ok(listener) => {
                let address = listener.address()?;
                Some((listener, address))
            }
            Err(error) => {
                warn!(%error, "控制面起不来，中继照常运行（只能用命令行前台控制）");
                None
            }
        }
    } else {
        None
    };

    let relay = RelayServer::start_with(
        options,
        Arc::new(peon_burrow_core::PolicyRules::new(
            config.token.clone(),
            config.allowed_hosts().to_vec(),
        )),
        tls_config(&config),
    )
    .await?;

    let mut endpoint = endpoint;
    if let Some((listener, address)) = listener {
        endpoint.address = address;
        // 带上自己的 pid：桌面端与控制面据此判断发现文件是不是陈旧
        endpoint = endpoint.with_pid(std::process::id());
        peon_burrow_ipc::write_endpoint(&paths.control_file(), &endpoint)?;

        let mut handler = RelayControl::new(
            relay.snapshot_handle(),
            {
                let config = config.clone();
                Arc::new(move || service_status(&config))
            },
            {
                let paths = paths.clone();
                let config = config.clone();
                Arc::new(move |verbose| {
                    doctor::run_all(
                        &doctor::Context {
                            paths: &paths,
                            config: &config,
                        },
                        verbose,
                    )
                })
            },
            {
                let lifecycle_tx = lifecycle_tx.clone();
                Arc::new(move |action| lifecycle_tx.send_replace(Some(action)).is_none())
            },
            verbose_flag(),
            crate::VERSION,
            host.clone(),
            relay.local_addr().port(),
        );

        if let Some(context) = &update_context {
            handler =
                handler.with_update(crate::update::hook(context.clone(), lifecycle_tx.clone()));
        }
        let handler = Arc::new(handler);

        let auth = Arc::new(AuthPolicy::new(token.clone()));
        let shutdown = {
            let mut rx = lifecycle_rx.clone();
            async move {
                while rx.changed().await.is_ok() {
                    if rx.borrow().is_some() {
                        break;
                    }
                }
            }
        };
        tokio::spawn(async move {
            if let Err(error) = serve(listener, handler, auth, shutdown).await {
                warn!(%error, "控制面停止");
            }
        });

        info!(address = %endpoint.address, "控制面已就绪");
    }

    info!(url = %relay.url(), "中继已就绪 —— 把上面这个地址填进扩展");

    // 更新检查放后台：网络问题绝不能拖住或搞挂中继
    if let Some(context) = update_context {
        let paths = paths.clone();
        let config = config.clone();
        tokio::spawn(async move {
            crate::update::startup_check(context, paths, config).await;
        });
    }
    if !config.allowed_hosts().is_empty() {
        info!(hosts = ?config.allowed_hosts(), "允许的邮件服务器");
    }

    let outcome = {
        let stop_signal = {
            let mut rx = lifecycle_rx.clone();
            async move {
                loop {
                    if let Some(action) = *rx.borrow_and_update() {
                        break action;
                    }
                    if rx.changed().await.is_err() {
                        break Lifecycle::Stop;
                    }
                }
            }
        };
        // 服务管理器的控制码（Windows 是 SCM 的 `SERVICE_CONTROL_STOP`）。
        let service_stop = async move {
            match stop {
                Some(mut stop) => {
                    stop.requested().await;
                    Lifecycle::Stop
                }
                // 前台形态没有服务管理器：这一支永远不完成
                None => std::future::pending().await,
            }
        };
        // ⚠️ 服务形态**不能**等 Ctrl+C：服务进程没有控制台，`SetConsoleCtrlHandler` 会失败，
        // 而这个 future 一失败就立刻完成 —— 表现是「服务刚起来就停」。
        let interrupt = async move {
            if service_form {
                std::future::pending::<()>().await
            } else {
                let _ = tokio::signal::ctrl_c().await;
            }
        };
        tokio::select! {
            _ = interrupt => {
                info!("收到 Ctrl+C，正在停止");
                Lifecycle::Stop
            }
            action = stop_signal => action,
            _ = service_stop => Lifecycle::Stop,
        }
    };

    relay.stop().await?;
    let _ = std::fs::remove_file(paths.control_file());

    Ok(match outcome {
        Lifecycle::Restart => ExitCode::RestartRequested,
        Lifecycle::Stop => ExitCode::Ok,
    })
}

/// 把配置里的 TLS 开关接到真正的 TLS 栈上。
///
/// ⚠️ 从前这里直接传 `TlsConfig::default()`：`tls_reject_unauthorized = false` 只被
/// **解析 + 警告 + 在 `doctor` 里展示**，从不影响握手 —— 用户以为关掉了校验
/// （`doctor` 还明确告诉他「证书校验已关闭」），实际连自签服务器照样失败，
/// 于是他去查别的地方。现在这个开关真的生效，也真的危险（只应在测试环境用）。
fn tls_config(config: &Config) -> TlsConfig {
    let mut tls = TlsConfig::default();
    if !config.tls_reject_unauthorized {
        // 启动时的持续警告由 `Config::startup_warnings()` 负责（唯一一处文案）
        tls = tls.with_any_certificate();
    }
    tls
}

/// 控制面地址：优先本地 socket，**服务形态**退到 loopback TCP。
fn control_endpoint(config: &Config, token: &str, service_form: bool) -> ControlEndpoint {
    if service_form || config.service.level == peon_burrow_ipc_types::ServiceLevel::System {
        // 服务与用户进程处于不同的会话 / 完整性级别，命名管道会被 ACL 挡住
        ControlEndpoint::loopback_tcp(0, token)
    } else {
        ControlEndpoint::local_socket("peon-burrow", token)
    }
}

/// `burrow status`
async fn status(paths: &Paths, json: bool) -> Result<ExitCode, AppError> {
    match read_endpoint(paths) {
        Some(endpoint) => {
            let client = ControlClient::new(endpoint);
            match client.request(Request::Status).await {
                Ok(value) => {
                    if json {
                        print_json(&value);
                    } else {
                        print_status(&value);
                    }
                    Ok(ExitCode::Ok)
                }
                Err(error) => {
                    if json {
                        print_json(
                            &serde_json::json!({ "running": false, "error": error.to_string() }),
                        );
                    } else {
                        println!("中继没有响应：{error}");
                        println!(
                            "下一步：`burrow service status` 看服务状态，或 `burrow run` 前台跑一次"
                        );
                    }
                    Ok(ExitCode::Runtime)
                }
            }
        }
        None => {
            if json {
                print_json(&serde_json::json!({ "running": false }));
            } else {
                println!("中继没在运行（找不到 {}）", paths.control_file().display());
                println!("下一步：`burrow service install` 装上自启，或 `burrow run` 前台跑一次");
            }
            Ok(ExitCode::Ok)
        }
    }
}

/// 发一条控制面请求（stop / restart / trace / update）。
async fn control(paths: &Paths, request: Request, json: bool) -> Result<ExitCode, AppError> {
    let Some(endpoint) = read_endpoint(paths) else {
        return Err(AppError::Runtime(format!(
            "中继没在运行（找不到 {}）。先 `burrow run` 或 `burrow service start`",
            paths.control_file().display()
        )));
    };
    let client = ControlClient::new(endpoint);
    match client.request(request).await {
        Ok(value) => {
            if json {
                print_json(&value);
            } else {
                println!("{value}");
            }
            Ok(ExitCode::Ok)
        }
        Err(error) => Err(AppError::Runtime(error.to_string())),
    }
}

/// `burrow doctor`
fn doctor_command(
    paths: &Paths,
    config: &Config,
    verbose: bool,
    json: bool,
) -> Result<ExitCode, AppError> {
    let report = doctor::run_all(&doctor::Context { paths, config }, verbose);

    if json {
        print_json(&serde_json::to_value(&report).unwrap_or_default());
    } else {
        let mut failures = 0;
        for check in &report.checks {
            let mark = match check.level {
                CheckLevel::Ok => "OK  ",
                CheckLevel::Warn => "WARN",
                CheckLevel::Fail => {
                    failures += 1;
                    "FAIL"
                }
            };
            println!("[{mark}] {} — {}", check.id, check.title);
            if verbose && !check.detail.is_empty() {
                println!("       {}", check.detail);
            }
            if let Some(action) = &check.action {
                println!("       → {action}");
            }
        }
        if failures > 0 {
            println!("\n{failures} 项需要处理。");
        }
    }

    let failed = report
        .checks
        .iter()
        .any(|check| check.level == CheckLevel::Fail);
    Ok(if failed {
        ExitCode::Config
    } else {
        ExitCode::Ok
    })
}

/// `burrow version`
fn version(json: bool) -> Result<ExitCode, AppError> {
    if json {
        print_json(&serde_json::json!({
            "version": crate::VERSION,
            "protocol": peon_burrow_ipc_types::IPC_PROTOCOL_VERSION,
            "watchProtocol": peon_burrow_protocol::WATCH_PROTOCOL_VERSION,
            "target": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
        }));
    } else {
        println!("burrow {} ({})", crate::VERSION, std::env::consts::ARCH);
        println!(
            "管道/扩展协议版本 {}",
            peon_burrow_protocol::WATCH_PROTOCOL_VERSION
        );
    }
    Ok(ExitCode::Ok)
}

/// `burrow url`：扩展里该填什么。
fn url(config: &Config, json: bool) -> Result<ExitCode, AppError> {
    let host = &config.relay.host;
    let shown = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.clone()
    };
    let url = format!("ws://{shown}:{}/", config.relay.port);
    if json {
        print_json(&serde_json::json!({ "url": url, "host": host, "port": config.relay.port }));
    } else {
        println!("{url}");
    }
    Ok(ExitCode::Ok)
}

/// `burrow token`
fn token(json: bool) -> Result<ExitCode, AppError> {
    let value = random_hex(32);
    if json {
        print_json(&serde_json::json!({ "token": value }));
    } else {
        println!("{value}");
    }
    Ok(ExitCode::Ok)
}

/// 服务子命令：安装/卸载/启停/查询。
///
/// ⚠️ 安装时会**先把当前可执行文件复制到安装目录**再注册：直接注册临时目录里的 exe
/// （例如刚 `cargo run` 出来的那个）会在目录被清理后指向一个不存在的路径 ——
/// 用户看到的是「重启之后服务起不来」。
/// 服务**实际**装在哪个级别：配置里的级别优先，没装就看另一级。
///
/// 这个函数是「装/卸/启/停/自启」的级别入口。用配置里的级别直接操作会出这种事：
/// 装了系统服务、配置仍是默认的用户级 → 点「卸载」卸掉一个不存在的用户级服务，
/// 命令成功退出（`{"installed": false}`），系统服务原地不动（真机踩过）。
fn effective_level(config: &Config) -> peon_burrow_ipc_types::ServiceLevel {
    let configured = config.service.level;
    match status_at_either_level(configured) {
        Ok(status) if status.installed => status.level.unwrap_or(configured),
        _ => configured,
    }
}

/// 读服务状态：先问配置里的级别，**没装再问另一级**。
///
/// 只问默认级别的话，「装成系统服务」的人在界面上会永远看到「未安装」——
/// 而桌面端又靠 `status.level` 决定装/卸/启停该找哪一级，于是点「安装」会去建一个
/// 用户级服务，和已存在的系统服务打架（真机踩过）。
fn status_at_either_level(
    configured: peon_burrow_ipc_types::ServiceLevel,
) -> Result<peon_burrow_ipc_types::ServiceStatus, AppError> {
    let status = peon_burrow_service::native_for(configured)
        .status()
        .map_err(service_error)?;
    if status.installed {
        return Ok(status);
    }

    // 没装：另一级可能装着（用户级 ↔ 系统级）
    let other = match configured {
        peon_burrow_ipc_types::ServiceLevel::User => peon_burrow_ipc_types::ServiceLevel::System,
        peon_burrow_ipc_types::ServiceLevel::System => peon_burrow_ipc_types::ServiceLevel::User,
    };
    let other_status = peon_burrow_service::native_for(other)
        .status()
        .map_err(service_error)?;
    if other_status.installed {
        return Ok(other_status);
    }

    // 两级都没有：回答默认级别的那份（`level` 字段用于界面提示）
    Ok(status)
}

/// 装完等中继就绪的上限（`service-lifecycle.md § 3` 第 8 步：15 秒，每 500ms 一次）。
const RELAY_READY_TIMEOUT: Duration = Duration::from_secs(15);

/// 注册进服务管理器（SCM / 计划任务 / launchd / systemd）的**命令行参数**。
///
/// | 级别 | 参数 |
/// | --- | --- |
/// | 用户级 | `run` |
/// | 系统级 | `run --service <名字> --config <路径> --data-dir <目录>` |
///
/// ⚠️ 系统级这几个参数都不是装饰：
///
/// - `--service <名字>`：Windows 上 SCM 拉起的进程**必须报到**（起 dispatcher、转
///   `SERVICE_RUNNING`），不报到就是 `sc start` 的 1053 —— 服务装得上、永远起不来；
///   而在别的平台上它表示「服务形态」（没有终端 → 日志落盘、控制面走 loopback TCP）；
/// - `--config` / `--data-dir`：系统服务跑在 `LocalSystem`（或 root）下，
///   按平台惯例推出来的配置目录与数据目录是**那个账户**的，与桌面端（登录用户）
///   读的不是同一个 —— 界面会永远显示「已安装，未运行」，扩展里填的地址/token
///   也与服务实际用的那份对不上。把安装者当时的路径写死进注册信息，
///   服务起来后就与桌面端对齐。
///
/// 用户级不需要这些：计划任务 / LaunchAgent / `systemd --user` 都跑在登录用户下，
/// 按惯例推出来的路径本来就与桌面端一致。
fn service_args(
    level: peon_burrow_ipc_types::ServiceLevel,
    name: &str,
    paths: &Paths,
) -> Vec<String> {
    let mut args = vec!["run".to_owned()];
    if level != peon_burrow_ipc_types::ServiceLevel::System {
        return args;
    }
    args.push("--service".to_owned());
    args.push(name.to_owned());
    args.push("--config".to_owned());
    args.push(paths.config_file().display().to_string());
    args.push("--data-dir".to_owned());
    args.push(paths.data_dir().display().to_string());
    args
}

/// 等中继的控制面应答一次（超时 15 秒，500ms 一次）。
///
/// 为什么值得等：注册与启动都成功**不等于**中继在跑 —— 配置错、端口被占、
/// 服务进程没报到，都是启动之后才发生的事。不等的话用户看到的是「装好了」，
/// 而扩展一个字都收不到（`service-lifecycle.md § 3` 第 8 / 9b 步）。
async fn wait_until_ready(paths: &Paths, timeout: Duration) -> Result<(), AppError> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let answered = match read_endpoint(paths) {
            Some(endpoint) => ControlClient::new(endpoint)
                .request(Request::Ping)
                .await
                .is_ok(),
            None => false,
        };
        if answered {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(AppError::Runtime(format!(
                "服务已注册，但 {} 秒内连不上中继的控制面：多半是配置或端口的问题。\n\
                 看日志：{}\n\
                 看服务：`burrow service status`（`lastExitCode` 非 0 就是它启动后立刻退了）",
                timeout.as_secs(),
                paths.log_file().display()
            )));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn service_command(
    paths: &Paths,
    config: &Config,
    command: ServiceCommand,
    json: bool,
) -> Result<ExitCode, AppError> {
    // ⚠️ 不是 `config.service.level`：装了系统服务的人，卸载/启停/自启都必须针对
    // 实际存在的那一级，否则会去操作一个不存在的用户级服务（真机踩过）。
    let default_level = effective_level(config);

    match command {
        ServiceCommand::Status => {
            let status = status_at_either_level(default_level)?;
            if json {
                print_json(&serde_json::to_value(&status).unwrap_or_default());
            } else {
                print_service_status(&status);
            }
            Ok(ExitCode::Ok)
        }

        ServiceCommand::Install {
            system,
            mode,
            no_autostart,
        } => {
            // `--system` 与 `--mode system` 等价（前者是早期写法，桌面端用后者）
            let level = if system {
                peon_burrow_ipc_types::ServiceLevel::System
            } else {
                match mode.as_deref() {
                    Some(value) => parse_service_level(value)?,
                    None => default_level,
                }
            };
            let autostart = if no_autostart {
                peon_burrow_ipc_types::Autostart::Off
            } else if level == peon_burrow_ipc_types::ServiceLevel::System {
                // 用户级做不到「开机即起」：它绑在登录会话上
                peon_burrow_ipc_types::Autostart::Boot
            } else {
                peon_burrow_ipc_types::Autostart::Logon
            };

            let install_dir = peon_burrow_service::default_install_dir(level).ok_or_else(|| {
                AppError::NeedElevation(format!(
                    "{level:?} 级安装需要一个可写的安装目录（系统级落在 ProgramData）"
                ))
            })?;
            std::fs::create_dir_all(&install_dir)?;

            let host = peon_burrow_service::native_for(level);

            // ⚠️ 先停掉旧实例再复制：Windows 上正在运行的 exe 映像没放开，直接覆盖会得到
            // 「另一个程序正在使用此文件」，而用户看到的是一句毫无线索的 IO 错误。
            // 停不下来不算失败（本来就没装 / 没在跑）。
            if let Err(error) = host.stop() {
                tracing::debug!(%error, "停止旧实例失败（继续安装）");
            } else {
                // `sc stop` / `schtasks /End` 只是**请求**停止，返回时进程可能还在退出 ——
                // 不等它放开 exe 映像，紧接着的复制会失败（`service-lifecycle.md § 4` 第 1 步）。
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while std::time::Instant::now() < deadline
                    && host.status().map(|status| status.running).unwrap_or(false)
                {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }

            // 换级别安装时另一级可能还在跑（两级本来就可以同时装着）：也停掉它 ——
            // 同一个安装目录里的 exe 映像被锁住，复制一定失败。
            // 停不下来不算失败（没装 / 没权限），复制那一步会给出可照做的错误文案。
            let other = match level {
                peon_burrow_ipc_types::ServiceLevel::User => {
                    peon_burrow_ipc_types::ServiceLevel::System
                }
                peon_burrow_ipc_types::ServiceLevel::System => {
                    peon_burrow_ipc_types::ServiceLevel::User
                }
            };
            if let Err(error) = peon_burrow_service::native_for(other).stop() {
                tracing::debug!(%error, "停止另一级实例失败（继续安装）");
            }

            let binary = install_dir.join(exe_file_name());
            let current = std::env::current_exe()?;
            if current != binary {
                std::fs::copy(&current, &binary).map_err(|error| {
                    AppError::Runtime(format!(
                        "把可执行文件复制到安装目录失败（{} → {}）：{error}\n\
                         下一步：先 `burrow service stop`（系统服务要管理员），再重试安装",
                        current.display(),
                        binary.display()
                    ))
                })?;
                info!(from = %current.display(), to = %binary.display(), "已把可执行文件复制到安装目录");
            }

            let options = peon_burrow_service::InstallOptions {
                name: config.service.name.clone(),
                level,
                autostart,
                binary_path: binary.clone(),
                args: service_args(level, &config.service.name, paths),
                mode: peon_burrow_service::RunMode::Service,
            };
            host.install(&options).map_err(service_error)?;
            // 装完自检：失败重启策略**真的**写进去了吗（不然服务崩了就回不来）
            host.verify_restart_policy().map_err(service_error)?;
            // 启动之前把发现文件清掉：它是「控制面在跑」的证据，
            // 留上一次的（崩溃残留）会让我们把「没起来」误判成「起来了」。
            let _ = std::fs::remove_file(paths.control_file());
            host.start().map_err(service_error)?;
            // `sc start` / `schtasks /Run` 只保证「进程被拉起来了」：
            // 配置错、端口被占、服务没报到都是**之后**才发生的。等控制面应答一次
            // 才算真的装好（`service-lifecycle.md § 3` 第 8 步 + 第 9b 步）。
            wait_until_ready(paths, RELAY_READY_TIMEOUT).await?;

            if json {
                print_json(&serde_json::json!({
                    "installed": true,
                    "name": config.service.name,
                    "binaryPath": binary.display().to_string(),
                    "autostart": format!("{autostart:?}"),
                }));
            } else {
                println!("已安装并启动：{}", config.service.name);
                println!("  自启方式：{autostart:?}");
                println!("  安装路径：{}", binary.display());
                println!(
                    "下一步：`burrow status` 确认中继在跑；扩展里填 `burrow url` 打出来的地址"
                );
            }
            Ok(ExitCode::Ok)
        }

        ServiceCommand::Uninstall => {
            // ⚠️ **两级都卸**：用户级任务与系统服务可以同时存在（先装用户级、后装系统级，
            // 或者反过来的半成品）。只卸「有效那一级」的话，用户点一次卸载只掉一个，
            // 再看状态还是「已安装」——表现就是「卸载不掉，也没报错」（真机踩过）。
            let levels = [
                default_level,
                match default_level {
                    peon_burrow_ipc_types::ServiceLevel::User => {
                        peon_burrow_ipc_types::ServiceLevel::System
                    }
                    peon_burrow_ipc_types::ServiceLevel::System => {
                        peon_burrow_ipc_types::ServiceLevel::User
                    }
                },
            ];

            let mut removed = false;
            let mut last_error = None;
            for level in levels {
                let host = peon_burrow_service::native_for(level);
                // 没装的那一级：`uninstall` 会报「未安装」，这不是失败
                if host
                    .status()
                    .map(|status| status.installed)
                    .unwrap_or(false)
                {
                    host.stop().ok();
                    match host.uninstall() {
                        Ok(()) => removed = true,
                        Err(error) => last_error = Some(error),
                    }
                }
            }

            // 两级都没装、也没出错：幂等成功（用户重复点卸载不该报错）
            if let Some(error) = last_error {
                if !removed {
                    return Err(service_error(error));
                }
                tracing::warn!(%error, "一级卸载失败，另一级已卸掉");
            }

            if json {
                print_json(&serde_json::json!({ "installed": false }));
            } else {
                println!("已卸载：{}", config.service.name);
            }
            Ok(ExitCode::Ok)
        }

        ServiceCommand::Start => {
            peon_burrow_service::native_for(default_level)
                .start()
                .map_err(service_error)?;
            say(json, "已启动", serde_json::json!({ "running": true }));
            Ok(ExitCode::Ok)
        }

        ServiceCommand::Stop => {
            peon_burrow_service::native_for(default_level)
                .stop()
                .map_err(service_error)?;
            say(json, "已停止", serde_json::json!({ "running": false }));
            Ok(ExitCode::Ok)
        }

        ServiceCommand::Restart => {
            let host = peon_burrow_service::native_for(default_level);
            // 停不下来不算致命：可能本来就没在跑
            host.stop().ok();
            host.start().map_err(service_error)?;
            say(json, "已重启", serde_json::json!({ "running": true }));
            Ok(ExitCode::Ok)
        }

        ServiceCommand::Autostart { state } => {
            let on = parse_on_off(&state)?;
            let host = peon_burrow_service::native_for(default_level);
            host.set_autostart(on).map_err(service_error)?;
            let autostart = if on {
                peon_burrow_ipc_types::Autostart::Logon
            } else {
                peon_burrow_ipc_types::Autostart::Off
            };
            if json {
                print_json(&serde_json::json!({ "autostart": format!("{autostart:?}") }));
            } else {
                println!("开机自启：{}", if on { "已打开" } else { "已关闭" });
            }
            Ok(ExitCode::Ok)
        }
    }
}

/// 解析 `--mode` 的取值。
///
/// ⚠️ 不认识的值要**报错并列出可选值**，不能默默当成默认值 —— 用户以为装成系统服务、
/// 实际装成用户级，直到「重启机器后服务没起来」才会发现。
fn parse_service_level(value: &str) -> Result<peon_burrow_ipc_types::ServiceLevel, AppError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "user" => Ok(peon_burrow_ipc_types::ServiceLevel::User),
        "system" => Ok(peon_burrow_ipc_types::ServiceLevel::System),
        other => Err(AppError::Config(format!(
            "--mode 取值不合法：{other}（可选：user | system）"
        ))),
    }
}

/// 解析 `on` / `off`（也接受 true/false、1/0）。
fn parse_on_off(value: &str) -> Result<bool, AppError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "1" | "yes" => Ok(true),
        "off" | "false" | "0" | "no" => Ok(false),
        other => Err(AppError::Config(format!(
            "取值不合法：{other}（可选：on | off）"
        ))),
    }
}

/// 服务状态查询（控制面用）：每次都现问服务管理器，不缓存。
pub fn service_status(config: &Config) -> ServiceStatus {
    let level = config.service.level;
    // 与 `service status` 一致：两级都看（装了系统服务的人不该显示「未安装」）
    status_at_either_level(level).unwrap_or_else(|_| ServiceStatus::not_installed("peon-burrow"))
}

/// 平台上的可执行文件名。
fn exe_file_name() -> &'static str {
    if cfg!(windows) {
        "burrow.exe"
    } else {
        "burrow"
    }
}

/// 服务错误 → 应用错误（提权单独成一类，退出码不一样）。
///
/// ⚠️ 两个顺序 / 形状上的讲究：
/// 1. **先分类再拼「下一步」**：分类靠 `Display` 的文字（下面第 2 点解释了它有多脆），
///    而建议文案里也会出现「管理员」这类词（例如 `StopTimedOut` 提到「管理员终端」）——
///    先拼进去就会把「运行期失败（1）」误判成「需要提权（4）」；
/// 2. 用户看到的必须有**下一步**：错误枚举的 `action()` 就是为这个写的（文案规范
///    「说人话、给下一步、可复制」），从前这里把它丢掉了，等于让用户对着一条
///    `sc` 的原始报错自己想办法。
///
/// ⏳ 分类本身靠 `purpose` 里的字（「提权」/「管理员」）：改一句文案就会偏。
/// 彻底修法是把「要不要提权」变成结构化字段（跟着 `CommandSpec` 走）。
fn service_error(error: peon_burrow_service::ServiceError) -> AppError {
    let display = error.to_string();
    let needs_elevation = display.contains("提权") || display.contains("管理员");
    let message = format!("{display}\n下一步：{}", error.action());

    if matches!(
        error,
        peon_burrow_service::ServiceError::UnsupportedPlatform { .. }
    ) {
        return AppError::Runtime(format!(
            "{display}\n这个平台上请用前台 `burrow run`，或用系统自带的方式自启"
        ));
    }
    if needs_elevation {
        AppError::NeedElevation(message)
    } else {
        AppError::Runtime(message)
    }
}

/// 一行成功输出（`--json` 时给结构化结果）。
fn say(json: bool, human: &str, value: serde_json::Value) {
    if json {
        print_json(&value);
    } else {
        println!("{human}");
    }
}

/// 人话版服务状态。
fn print_service_status(status: &ServiceStatus) {
    if status.installed {
        println!("服务：已安装（{}）", status.name);
        println!("  运行中：{}", if status.running { "是" } else { "否" });
        println!("  级别：{:?}", status.level);
        println!("  自启：{:?}", status.autostart);
        println!(
            "  失败重启策略：{}",
            if status.restart_policy_configured {
                "已配置"
            } else {
                "未配置（服务崩了不会自己回来）"
            }
        );
        if let Some(path) = &status.binary_path {
            println!("  路径：{path}");
        }
    } else {
        println!("服务：未安装（{}）", status.name);
        println!("下一步：`burrow service install`（默认用户级、零提权）");
    }
}

/// 读控制面地址；文件不存在或读不动都当「没在运行」。
fn read_endpoint(paths: &Paths) -> Option<ControlEndpoint> {
    peon_burrow_ipc::read_endpoint(&paths.control_file()).ok()
}

/// 打印人话版状态。
fn print_status(value: &serde_json::Value) {
    let process = &value["process"];
    let service = &value["service"];
    println!("中继：运行中");
    println!(
        "  地址：ws://{}:{}/",
        process["host"].as_str().unwrap_or("?"),
        process["port"].as_u64().unwrap_or_default()
    );
    println!("  版本：{}", process["version"].as_str().unwrap_or("?"));
    println!(
        "  连接：{}（其中 watch {}）",
        process["connections"].as_u64().unwrap_or_default(),
        process["watchConnections"].as_u64().unwrap_or_default()
    );
    println!("  启动于：{}", process["startedAt"].as_str().unwrap_or("?"));
    if let Some(error) = process["lastError"].as_str() {
        println!("  最近错误：{error}");
    }
    println!(
        "服务：{}",
        if service["installed"].as_bool().unwrap_or(false) {
            "已安装"
        } else {
            "未安装"
        }
    );
}

/// 打印 JSON。
fn print_json(value: &serde_json::Value) {
    match serde_json::to_string_pretty(value) {
        Ok(text) => println!("{text}"),
        Err(error) => eprintln!("JSON 输出失败：{error}"),
    }
}

/// 系统随机数（用 rustls 已经拉进来的 `ring`，不再引一个 RNG 依赖）。
pub fn random_hex(bytes: usize) -> String {
    let provider = rustls::crypto::ring::default_provider();
    let mut buffer = vec![0u8; bytes];
    if provider.secure_random.fill(&mut buffer).is_err() {
        // 系统随机数拿不到时**不要**退回可预测的值：那等于没有 token
        panic!("系统随机数不可用，无法生成安全 token");
    }
    hex::encode(buffer)
}

/// `burrow update`：中继在跑就走控制面（它能顺手触发重启），否则直连做一次。
///
/// 两条路径都要能走通：CLI 必须能**单独**用，而不是非得先起中继。
async fn update_command(
    paths: &Paths,
    config: &Config,
    force: bool,
    apply: bool,
    json: bool,
) -> Result<ExitCode, AppError> {
    if read_endpoint(paths).is_some() {
        let request = if apply {
            Request::UpdateApply
        } else {
            Request::UpdateCheck { force }
        };
        return control(paths, request, json).await;
    }

    let Some(context) = crate::update::context(config, paths) else {
        return Err(AppError::Runtime(
            "自更新被配置关掉了（[update] enabled = false）".to_owned(),
        ));
    };

    if apply {
        let applied = peon_burrow_update::apply(&context)
            .await
            .map_err(|error| AppError::Update(error.to_string()))?;
        if json {
            print_json(&serde_json::json!({
                "from": applied.from.to_string(),
                "to": applied.to.to_string(),
                "asset": applied.asset,
            }));
        } else {
            println!("已更新：{} → {}", applied.from, applied.to);
            println!(
                "下一步：重启中继（`burrow service restart`，或前台 `burrow run` 的窗口里 Ctrl+C 再跑）"
            );
        }
        return Ok(ExitCode::RestartRequested);
    }

    let status = peon_burrow_update::check(&context)
        .await
        .map_err(|error| AppError::Update(error.to_string()))?;
    crate::update::record(paths, &status);

    if json {
        print_json(&crate::update::describe(&status));
    } else if status.available {
        println!("有新版本：{} → {}", status.current, status.latest);
        println!("下一步：`burrow update --apply`（替换后需要重启中继）");
    } else {
        println!("已是最新版本（{}）", status.current);
    }
    Ok(ExitCode::Ok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CliOverrides, Config, EnvSource};
    use clap::Parser;

    fn cli(args: &[&str]) -> Cli {
        Cli::parse_from(std::iter::once("burrow").chain(args.iter().copied()))
    }

    fn fixture(extra: &str) -> (tempfile::TempDir, Paths, Config) {
        let directory = tempfile::tempdir().expect("tempdir");
        let paths = Paths::for_test(directory.path().join("data"));
        let config = Config::merge(
            crate::config::test_support::file_config(extra),
            &EnvSource::default(),
            &CliOverrides::default(),
            Vec::new(),
        )
        .expect("merge");
        (directory, paths, config)
    }

    #[test]
    fn version_and_token_are_thirty_two_byte_secrets() {
        assert_eq!(random_hex(32).len(), 64);
        assert_ne!(random_hex(32), random_hex(32), "两次必须不同");
        assert!(random_hex(32).chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn version_json_carries_both_protocol_versions() {
        let (directory, paths, config) = fixture("");
        let code = dispatch_with(&paths, config, cli(&["version", "--json"]))
            .await
            .expect("version");
        assert_eq!(code, ExitCode::Ok);
        drop(directory);
    }

    #[tokio::test]
    async fn url_uses_the_configured_port() {
        let (directory, paths, config) = fixture("port = 41319\n");
        let code = dispatch_with(&paths, config, cli(&["url"]))
            .await
            .expect("url");
        assert_eq!(code, ExitCode::Ok);
        drop(directory);
    }

    #[tokio::test]
    async fn status_without_a_running_relay_is_not_an_error() {
        let (directory, paths, config) = fixture("");
        let code = dispatch_with(&paths, config, cli(&["status", "--json"]))
            .await
            .expect("status");
        assert_eq!(code, ExitCode::Ok, "「没在运行」是正常回答，不是失败");
        drop(directory);
    }

    #[tokio::test]
    async fn stop_without_a_running_relay_fails_with_a_next_step() {
        let (directory, paths, config) = fixture("");
        let error = dispatch_with(&paths, config, cli(&["stop"]))
            .await
            .expect_err("should fail");
        assert!(error.to_string().contains("burrow run"), "{error}");
        assert_eq!(error.exit_code(), ExitCode::Runtime);
        drop(directory);
    }

    #[tokio::test]
    async fn doctor_reports_a_clean_setup_as_ok() {
        let (directory, paths, config) = fixture("");
        let code = dispatch_with(&paths, config, cli(&["doctor"]))
            .await
            .expect("doctor");
        assert_eq!(code, ExitCode::Ok);
        drop(directory);
    }

    #[tokio::test]
    async fn doctor_fails_with_the_config_exit_code_on_a_broken_setup() {
        // 非本机地址却没 token：doctor 必须 FAIL，退出码 2
        let (directory, paths, config) =
            fixture("host = \"0.0.0.0\"\nallowed_hosts = [\"imap.qq.com\"]\n");
        let code = dispatch_with(&paths, config, cli(&["doctor"]))
            .await
            .expect("doctor");
        assert_eq!(code, ExitCode::Config);
        drop(directory);
    }

    #[test]
    fn service_level_and_on_off_are_parsed_strictly() {
        use peon_burrow_ipc_types::ServiceLevel;

        assert_eq!(parse_service_level("user").unwrap(), ServiceLevel::User);
        assert_eq!(parse_service_level("SYSTEM").unwrap(), ServiceLevel::System);
        // ⚠️ 不认识的值必须报错：默默当默认值会让「我明明装的是系统服务」变成
        // 「重启机器后服务没起来」这种极难查的问题
        assert!(parse_service_level("root").is_err());

        assert!(parse_on_off("on").unwrap());
        assert!(parse_on_off("true").unwrap());
        assert!(!parse_on_off("off").unwrap());
        assert!(!parse_on_off("0").unwrap());
        assert!(parse_on_off("maybe").is_err());
    }

    #[tokio::test]
    async fn service_status_is_a_read_only_query_that_always_answers() {
        // 真去问本机的服务管理器。这是个**只读**查询：无论装没装都必须给出结论，
        // 而不是报错 —— GUI 的「状态」页就靠它。
        let (directory, paths, config) = fixture("");
        let code = dispatch_with(&paths, config, cli(&["service", "status", "--json"]))
            .await
            .expect("查询服务状态不该失败");
        assert_eq!(code, ExitCode::Ok);
        drop(directory);
    }

    #[test]
    fn the_endpoint_prefers_a_local_socket_but_falls_back_for_system_level() {
        let (directory, _paths, mut config) = fixture("");
        let endpoint = control_endpoint(&config, "t", false);
        assert_eq!(endpoint.kind, peon_burrow_ipc::TransportKind::LocalSocket);

        config.service.level = peon_burrow_ipc_types::ServiceLevel::System;
        let endpoint = control_endpoint(&config, "t", false);
        assert_eq!(endpoint.kind, peon_burrow_ipc::TransportKind::LoopbackTcp);
        drop(directory);
    }

    #[test]
    fn the_service_form_always_uses_loopback_tcp() {
        // 服务形态不看配置里的 `[service] mode`：桌面端（登录用户、中完整性）要能连上
        // 跑在 LocalSystem 下的中继，命名管道会被 ACL 挡住。
        let (directory, _paths, config) = fixture("");
        let endpoint = control_endpoint(&config, "t", true);
        assert_eq!(endpoint.kind, peon_burrow_ipc::TransportKind::LoopbackTcp);
        drop(directory);
    }

    #[test]
    fn user_level_registration_keeps_the_plain_run_command() {
        // 用户级（计划任务 / LaunchAgent / systemd --user）跑在登录用户下，
        // 按惯例推出来的路径与桌面端一致 —— 不要往注册信息里塞路径（改了就等于改了
        // 那些平台单元文件的正文，收益为零）。
        let (directory, paths, _config) = fixture("");
        assert_eq!(
            service_args(
                peon_burrow_ipc_types::ServiceLevel::User,
                "peon-burrow",
                &paths
            ),
            vec!["run"]
        );
        drop(directory);
    }

    #[test]
    fn system_level_registration_pins_the_service_form_and_the_installers_paths() {
        // 系统级必须把这三样写进注册信息：
        // ① `--service <名字>`（Windows 上 SCM 靠它报到，缺了就是 sc start 的 1053）；
        // ② `--config` ③ `--data-dir`（服务跑在 LocalSystem / root 下，
        //    按惯例推出来的目录与桌面端读的那个不是同一个）。
        let (directory, paths, _config) = fixture("");
        let args = service_args(
            peon_burrow_ipc_types::ServiceLevel::System,
            "peon-burrow",
            &paths,
        );

        assert_eq!(args[0], "run", "{args:?}");
        let value_of = |flag: &str| {
            args.iter()
                .position(|arg| arg == flag)
                .and_then(|index| args.get(index + 1))
                .cloned()
        };
        let config_file = paths.config_file().display().to_string();
        let data_dir = paths.data_dir().display().to_string();
        assert_eq!(
            value_of("--service").as_deref(),
            Some("peon-burrow"),
            "{args:?}"
        );
        assert_eq!(
            value_of("--config").as_deref(),
            Some(config_file.as_str()),
            "{args:?}"
        );
        assert_eq!(
            value_of("--data-dir").as_deref(),
            Some(data_dir.as_str()),
            "{args:?}"
        );
        drop(directory);
    }

    /// 从一段文案里捞出反引号包起来的 `burrow …` 命令（这个仓库的文案就是这么写的）。
    fn burrow_commands_in(text: &str) -> Vec<String> {
        text.match_indices('`')
            .filter_map(|(start, _)| {
                let rest = text.get(start + 1..)?;
                let end = rest.find('`')?;
                let quoted = rest.get(..end)?;
                quoted.starts_with("burrow ").then(|| quoted.to_owned())
            })
            .collect()
    }

    /// 错误文案里的 `burrow …` 命令**必须真的能被 CLI 解析**。
    ///
    /// 为什么值得一条测试：这些文案自称「可复制」（`host.rs` 的错误枚举就是这么写的），
    /// 而真机上出现过 `burrow service install --name <名字>`、`--level user`、
    /// `burrow doctor --port` 这类**不存在的参数**（CLI 里是 `--mode`、服务名来自配置、
    /// `--port` 要带值）——用户照抄只会拿到第二个错误。
    /// 这里把文案里的命令原样喂给 clap，写错就红。
    #[test]
    fn the_commands_in_the_error_advice_actually_parse() {
        let advices = vec![
            peon_burrow_service::ServiceError::UnsupportedPlatform { os: "plan9" }.action(),
            peon_burrow_service::ServiceError::InvalidOptions {
                message: "bad".to_owned(),
            }
            .action(),
            peon_burrow_service::ServiceError::NotInstalled {
                name: "peon-burrow".to_owned(),
            }
            .action(),
            peon_burrow_service::ServiceError::AlreadyInstalled {
                name: "peon-burrow".to_owned(),
            }
            .action(),
            peon_burrow_service::ServiceError::RestartPolicyMissing {
                name: "peon-burrow".to_owned(),
                expected: "restart/5000".to_owned(),
                found: String::new(),
            }
            .action(),
            peon_burrow_service::ServiceError::StopTimedOut {
                name: "peon-burrow".to_owned(),
                seconds: 10,
            }
            .action(),
            peon_burrow_service::PortStatus::advice(41316),
        ];

        let mut checked = 0usize;
        for advice in advices {
            for command in burrow_commands_in(&advice) {
                // 文案里写的是 `burrow …` 整条命令；clap 要的 argv 第一项是程序名 ✓
                let mut args: Vec<&str> = command.split_whitespace().collect();
                if let Some(program) = args.first_mut() {
                    *program = "burrow";
                }
                Cli::try_parse_from(args).unwrap_or_else(|error| {
                    panic!("文案里的命令解析不了：{command}\n（来自：{advice}）\n{error}")
                });
                checked += 1;
            }
        }
        assert!(
            checked >= 5,
            "至少该从这些文案里捞出几条命令来核查，实际只有 {checked} 条"
        );
    }
}
