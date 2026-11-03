use super::commands::read_endpoint;
use super::output::{print_json, print_service_status, say};
use crate::cli::ServiceCommand;
use crate::config::Config;
use crate::exit::{AppError, ExitCode};
use crate::paths::Paths;
use peon_burrow_ipc::ControlClient;
use peon_burrow_ipc_types::{Request, ServiceStatus};
use std::time::Duration;
use tracing::info;

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
pub(super) fn effective_level(config: &Config) -> peon_burrow_ipc_types::ServiceLevel {
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
pub(super) fn status_at_either_level(
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
pub(super) fn service_args(
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
pub(super) async fn wait_until_ready(paths: &Paths, timeout: Duration) -> Result<(), AppError> {
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

pub(super) async fn service_command(
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
pub(super) fn parse_service_level(
    value: &str,
) -> Result<peon_burrow_ipc_types::ServiceLevel, AppError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "user" => Ok(peon_burrow_ipc_types::ServiceLevel::User),
        "system" => Ok(peon_burrow_ipc_types::ServiceLevel::System),
        other => Err(AppError::Config(format!(
            "--mode 取值不合法：{other}（可选：user | system）"
        ))),
    }
}

/// 解析 `on` / `off`（也接受 true/false、1/0）。
pub(super) fn parse_on_off(value: &str) -> Result<bool, AppError> {
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
pub(super) fn exe_file_name() -> &'static str {
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
pub(super) fn service_error(error: peon_burrow_service::ServiceError) -> AppError {
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
