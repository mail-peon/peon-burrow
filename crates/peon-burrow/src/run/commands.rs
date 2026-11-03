use super::output::{print_json, print_status};
use super::secrets::random_hex;
use crate::config::Config;
use crate::doctor;
use crate::exit::{AppError, ExitCode};
use crate::paths::Paths;
use peon_burrow_ipc::{ControlClient, ControlEndpoint};
use peon_burrow_ipc_types::{CheckLevel, Request};

/// `burrow status`
pub(super) async fn status(paths: &Paths, json: bool) -> Result<ExitCode, AppError> {
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
pub(super) async fn control(
    paths: &Paths,
    request: Request,
    json: bool,
) -> Result<ExitCode, AppError> {
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
pub(super) fn doctor_command(
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
pub(super) fn version(json: bool) -> Result<ExitCode, AppError> {
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
pub(super) fn url(config: &Config, json: bool) -> Result<ExitCode, AppError> {
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
pub(super) fn token(json: bool) -> Result<ExitCode, AppError> {
    let value = random_hex(32);
    if json {
        print_json(&serde_json::json!({ "token": value }));
    } else {
        println!("{value}");
    }
    Ok(ExitCode::Ok)
}

/// 读控制面地址；文件不存在或读不动都当「没在运行」。
pub(super) fn read_endpoint(paths: &Paths) -> Option<ControlEndpoint> {
    peon_burrow_ipc::read_endpoint(&paths.control_file()).ok()
}

/// `burrow update`：中继在跑就走控制面（它能顺手触发重启），否则直连做一次。
///
/// 两条路径都要能走通：CLI 必须能**单独**用，而不是非得先起中继。
pub(super) async fn update_command(
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
