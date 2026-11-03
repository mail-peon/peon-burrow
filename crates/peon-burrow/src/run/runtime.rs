use super::secrets::random_hex;
use super::service::service_status;
use crate::config::Config;
use crate::control::{Lifecycle, RelayControl};
use crate::doctor;
use crate::exit::{AppError, ExitCode};
use crate::logging::verbose_flag;
use crate::paths::Paths;
use peon_burrow_core::{RelayServer, TlsConfig};
use peon_burrow_ipc::{AuthPolicy, ControlEndpoint, Listener, serve};
use std::sync::Arc;
use tracing::{info, warn};

/// `burrow run`：前台跑中继 + 控制面，直到 Ctrl+C、控制面 `stop`、或服务管理器要求停下。
pub(super) async fn run_relay(
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
pub(super) fn tls_config(config: &Config) -> TlsConfig {
    let mut tls = TlsConfig::default();
    if !config.tls_reject_unauthorized {
        // 启动时的持续警告由 `Config::startup_warnings()` 负责（唯一一处文案）
        tls = tls.with_any_certificate();
    }
    tls
}

/// 控制面地址：优先本地 socket，**服务形态**退到 loopback TCP。
pub(super) fn control_endpoint(
    config: &Config,
    token: &str,
    service_form: bool,
) -> ControlEndpoint {
    if service_form || config.service.level == peon_burrow_ipc_types::ServiceLevel::System {
        // 服务与用户进程处于不同的会话 / 完整性级别，命名管道会被 ACL 挡住
        ControlEndpoint::loopback_tcp(0, token)
    } else {
        ControlEndpoint::local_socket("peon-burrow", token)
    }
}
