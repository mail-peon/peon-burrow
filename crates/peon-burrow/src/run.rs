//! `run()`：把命令行变成行为。**退出码只在这里产生**（约定 C2）。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use peon_burrow_core::{RelayServer, TlsConfig};
use peon_burrow_ipc::{AuthPolicy, ControlClient, ControlEndpoint, Listener, serve};
use peon_burrow_ipc_types::{CheckLevel, Request, ServiceStatus};
use tracing::{info, warn};
use tracing_subscriber::Layer as _;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::cli::{Cli, Command, ServiceCommand};
use crate::config::{Config, EnvSource, LogLevel};
use crate::control::{Lifecycle, RelayControl};
use crate::doctor;
use crate::exit::{AppError, ExitCode};
use crate::paths::Paths;

/// 临时打开明文日志的全局开关（`burrow trace` 会翻它）。
static VERBOSE: std::sync::OnceLock<Arc<AtomicBool>> = std::sync::OnceLock::new();

/// 取（必要时创建）这个开关。
pub fn verbose_flag() -> Arc<AtomicBool> {
    Arc::clone(VERBOSE.get_or_init(|| Arc::new(AtomicBool::new(false))))
}

/// 同步入口：`main` 只调它。
pub fn run(cli: Cli) -> ExitCode {
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
        match dispatch(cli).await {
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

/// 定位路径、读配置、装日志，然后分发。
pub async fn dispatch(cli: Cli) -> Result<ExitCode, AppError> {
    let paths = Paths::discover()?;
    let config = Config::load(&paths, &EnvSource::from_env(), &cli.overrides())?;
    dispatch_with(&paths, config, cli).await
}

/// 注入路径与配置的分发（测试走这条，不碰真实用户目录）。
pub async fn dispatch_with(paths: &Paths, config: Config, cli: Cli) -> Result<ExitCode, AppError> {
    init_logging(config.log_level, config.relay.trace);
    for warning in config.startup_warnings() {
        warn!("{warning}");
    }

    match cli.command.clone() {
        None | Some(Command::Status) => status(paths, cli.json).await,
        Some(Command::Run { no_control }) => run_relay(paths, config, no_control).await,
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

/// 装日志。
///
/// trace 的开关是**运行期可翻**的（`burrow trace` 会翻它），所以过滤器读一个原子量，
/// 而不是在启动时把级别写死。
pub fn init_logging(level: LogLevel, trace: bool) {
    let verbose = verbose_flag();
    verbose.store(trace, Ordering::Relaxed);

    let max = match level {
        LogLevel::Error => tracing::Level::ERROR,
        LogLevel::Warn => tracing::Level::WARN,
        LogLevel::Info => tracing::Level::INFO,
        LogLevel::Debug => tracing::Level::DEBUG,
        LogLevel::Trace => tracing::Level::TRACE,
    };

    let filter = tracing_subscriber::filter::filter_fn(move |metadata| {
        *metadata.level() <= max || verbose.load(Ordering::Relaxed)
    });
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(filter);

    // 已经初始化过（测试里会多次调用）就忽略
    let _ = tracing_subscriber::registry().with(layer).try_init();
}

/// `burrow run`：前台跑中继 + 控制面，直到 Ctrl+C 或收到 stop（控制面请求）。
async fn run_relay(paths: &Paths, config: Config, no_control: bool) -> Result<ExitCode, AppError> {
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
    let endpoint = control_endpoint(&config, &token);
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
        TlsConfig::default(),
    )
    .await?;

    let mut endpoint = endpoint;
    if let Some((listener, address)) = listener {
        endpoint.address = address;
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
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("收到 Ctrl+C，正在停止");
                Lifecycle::Stop
            }
            action = stop_signal => action,
        }
    };

    relay.stop().await?;
    let _ = std::fs::remove_file(paths.control_file());

    Ok(match outcome {
        Lifecycle::Restart => ExitCode::RestartRequested,
        Lifecycle::Stop => ExitCode::Ok,
    })
}

/// 控制面地址：优先本地 socket，系统服务模式下退到 loopback TCP。
fn control_endpoint(config: &Config, token: &str) -> ControlEndpoint {
    if config.service.level == peon_burrow_ipc_types::ServiceLevel::System {
        // 系统服务与用户进程处于不同的完整性级别，命名管道会被 ACL 挡住
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
async fn service_command(
    _paths: &Paths,
    config: &Config,
    command: ServiceCommand,
    json: bool,
) -> Result<ExitCode, AppError> {
    let default_level = config.service.level;

    match command {
        ServiceCommand::Status => {
            let host = peon_burrow_service::native_for(default_level);
            let status = host.status().map_err(service_error)?;
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

            let binary = install_dir.join(exe_file_name());
            let current = std::env::current_exe()?;
            if current != binary {
                std::fs::copy(&current, &binary)?;
                info!(from = %current.display(), to = %binary.display(), "已把可执行文件复制到安装目录");
            }

            let host = peon_burrow_service::native_for(level);
            let options = peon_burrow_service::InstallOptions {
                name: config.service.name.clone(),
                level,
                autostart,
                binary_path: binary.clone(),
                args: vec!["run".to_owned()],
                mode: peon_burrow_service::RunMode::Service,
            };
            host.install(&options).map_err(service_error)?;
            // 装完自检：失败重启策略**真的**写进去了吗（不然服务崩了就回不来）
            host.verify_restart_policy().map_err(service_error)?;
            host.start().map_err(service_error)?;

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
            let host = peon_burrow_service::native_for(default_level);
            host.stop().ok();
            host.uninstall().map_err(service_error)?;
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
    let name = config.service.name.clone();
    peon_burrow_service::native_for(level)
        .status()
        .unwrap_or_else(|_| ServiceStatus::not_installed(name))
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
fn service_error(error: peon_burrow_service::ServiceError) -> AppError {
    if matches!(
        error,
        peon_burrow_service::ServiceError::UnsupportedPlatform { .. }
    ) {
        return AppError::Runtime(format!(
            "{error}\n这个平台上请用前台 `burrow run`，或用系统自带的方式自启"
        ));
    }
    let text = error.to_string();
    // 服务库把「需要提权」写在用途说明里（它是唯一能带上下文的地方）
    if text.contains("提权") || text.contains("管理员") {
        AppError::NeedElevation(text)
    } else {
        AppError::Runtime(text)
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
        let endpoint = control_endpoint(&config, "t");
        assert_eq!(endpoint.kind, peon_burrow_ipc::TransportKind::LocalSocket);

        config.service.level = peon_burrow_ipc_types::ServiceLevel::System;
        let endpoint = control_endpoint(&config, "t");
        assert_eq!(endpoint.kind, peon_burrow_ipc::TransportKind::LoopbackTcp);
        drop(directory);
    }
}
