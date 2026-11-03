//! `run()`：把命令行变成行为。**退出码只在这里产生**（约定 C2）。

mod runtime;
mod service;
pub use service::service_status;
mod commands;
mod output;
mod secrets;
pub use secrets::random_hex;

use crate::cli::{Cli, Command};
use crate::config::{Config, EnvSource};
use crate::exit::{AppError, ExitCode};
use crate::logging::{LogFile, init_logging};
use crate::paths::Paths;
use commands::{control, doctor_command, status, token, update_command, url, version};
use peon_burrow_ipc_types::Request;
use runtime::run_relay;
use service::service_command;
use tracing::warn;

/// 同步入口：`main` 只调它（前台形态）。
pub fn run(cli: Cli) -> ExitCode {
    run_with(cli, None)
}

/// 服务形态的入口：服务线程调它（见 [`crate::service`]）。
///
/// 与 [`run`] 的唯一差别是「停止信号从哪来」—— 服务进程没有控制台，Ctrl+C 永远不会来。
///
/// ⚠️ 调用点在 Windows 的 SCM 那一半（`service.rs` 的 `windows_scm`）：Linux/macOS 的
/// 服务形态由服务管理器直接跑 `burrow run`。非 Windows 构建里没人调它，而
/// `-D warnings` 会把 dead_code 当错误 —— 按平台放行（Windows 上仍然严格检查）。
#[cfg_attr(not(windows), allow(dead_code))]
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

#[cfg(test)]
mod tests;
