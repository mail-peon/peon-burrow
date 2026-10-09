//! `doctor`：检查项注册表。**加一项检查 = 加一个 struct**。
//!
//! ⚠️ 「解释清楚」不是可选项（`ai-docs/design/logging-and-diagnostics.md § 4`）：
//! 每条 FAIL/WARN 都要给出**下一步做什么**，否则用户只会来问「它说失败了，然后呢？」

use peon_burrow_ipc_types::{CheckLevel, CheckResult, DoctorReport};

use crate::config::Config;
use crate::paths::Paths;

/// 检查用的上下文。
pub struct Context<'a> {
    /// 路径（注入）。
    pub paths: &'a Paths,
    /// 合并后的配置。
    pub config: &'a Config,
}

/// 一条检查。
pub struct Check {
    /// 稳定的 id（GUI 与文档按它对照）。
    pub id: &'static str,
    /// 标题。
    pub title: &'static str,
    /// 执行体。
    pub run: fn(&Context<'_>, bool) -> CheckResult,
}

/// 全部检查（顺序就是输出顺序）。
pub fn registry() -> &'static [Check] {
    &[
        Check {
            id: "config",
            title: "配置",
            run: check_config,
        },
        Check {
            id: "data-dir",
            title: "数据目录可写",
            run: check_data_dir,
        },
        Check {
            id: "port",
            title: "监听端口",
            run: check_port,
        },
        Check {
            id: "security.token",
            title: "访问 token",
            run: check_token,
        },
        Check {
            id: "security.tls",
            title: "证书校验",
            run: check_tls,
        },
        Check {
            id: "security.trace",
            title: "明文日志",
            run: check_trace,
        },
        Check {
            id: "update",
            title: "自更新",
            run: check_update,
        },
    ]
}

/// 跑全部检查。
pub fn run_all(context: &Context<'_>, verbose: bool) -> DoctorReport {
    DoctorReport {
        checks: registry()
            .iter()
            .map(|check| (check.run)(context, verbose))
            .collect(),
    }
}

/// 配置本身有没有问题（未知字段、被弃用字段、安全告警）。
fn check_config(context: &Context<'_>, _verbose: bool) -> CheckResult {
    let warnings = context.config.startup_warnings();
    if warnings.is_empty() {
        return ok(
            "config",
            "配置",
            format!("没有发现问题（{}）", context.paths.config_file().display()),
        );
    }
    CheckResult {
        id: "config".to_owned(),
        level: CheckLevel::Warn,
        title: format!("配置有 {} 条提醒", warnings.len()),
        detail: warnings.join("；"),
        action: Some(format!(
            "看一下 {}，改完运行 `burrow service restart`",
            context.paths.config_file().display()
        )),
    }
}

/// 数据目录能不能写。
fn check_data_dir(context: &Context<'_>, _verbose: bool) -> CheckResult {
    let directory = context.paths.data_dir().to_path_buf();
    if let Err(error) = std::fs::create_dir_all(&directory) {
        return CheckResult {
            id: "data-dir".to_owned(),
            level: CheckLevel::Fail,
            title: "数据目录建不出来".to_owned(),
            detail: format!("{}：{error}", directory.display()),
            action: Some("检查这个路径的权限（系统服务模式下它属于服务账号）".to_owned()),
        };
    }

    let probe = directory.join(".write-probe");
    match std::fs::write(&probe, b"ok") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            ok("data-dir", "数据目录可写", directory.display().to_string())
        }
        Err(error) => CheckResult {
            id: "data-dir".to_owned(),
            level: CheckLevel::Fail,
            title: "数据目录不可写".to_owned(),
            detail: format!("{}：{error}", directory.display()),
            action: Some(
                "换个有写权限的用户跑，或者用系统级安装（数据目录会落在 ProgramData）".to_owned(),
            ),
        },
    }
}

/// 监听端口能不能绑上。
fn check_port(context: &Context<'_>, _verbose: bool) -> CheckResult {
    let port = context.config.relay.port;
    if port == 0 {
        return CheckResult {
            id: "port".to_owned(),
            level: CheckLevel::Warn,
            title: "端口是 0".to_owned(),
            detail: "port = 0 表示由系统分配，扩展里的中继地址就没法固定了".to_owned(),
            action: Some("改成 41316 或其它固定端口".to_owned()),
        };
    }

    match std::net::TcpListener::bind((context.config.relay.host.as_str(), port)) {
        Ok(listener) => {
            drop(listener);
            ok(
                "port",
                "监听端口可用",
                format!("{}:{}", context.config.relay.host, port),
            )
        }
        Err(error) => CheckResult {
            id: "port".to_owned(),
            level: CheckLevel::Warn,
            title: format!("端口 {port} 现在绑不上"),
            // 绑不上时问清楚**是谁占着**：用户真正需要的是这个，而不是一个 errno
            detail: describe_port_holder(port).unwrap_or_else(|| error.to_string()),
            action: Some(
                "如果中继已经在跑，这是正常的（用 `burrow status` 确认）；否则用 `burrow doctor -v` 看占用者，或把配置里的 port 改成别的"
                    .to_owned(),
            ),
        },
    }
}

/// token 强度。
fn check_token(context: &Context<'_>, _verbose: bool) -> CheckResult {
    let host = &context.config.relay.host;
    let loopback = host == "127.0.0.1" || host == "::1" || host.eq_ignore_ascii_case("localhost");

    match context.config.token.as_deref() {
        None if loopback => ok(
            "security.token",
            "访问 token",
            "没有设置（只监听本机，可以接受）".to_owned(),
        ),
        None => CheckResult {
            id: "security.token".to_owned(),
            level: CheckLevel::Fail,
            title: "监听了非本机地址却没有 token".to_owned(),
            detail: format!("host = {host}"),
            action: Some(
                "配置里设置 token = \"<32 字节以上的随机串>\"，或者把 host 改回 127.0.0.1"
                    .to_owned(),
            ),
        },
        Some(token) if token.len() < 16 => CheckResult {
            id: "security.token".to_owned(),
            level: CheckLevel::Warn,
            title: "token 太短".to_owned(),
            detail: format!("当前 {} 个字符", token.len()),
            action: Some("换成至少 32 个字符的随机串（`burrow token` 可以生成一个）".to_owned()),
        },
        Some(token) => ok(
            "security.token",
            "访问 token",
            format!("已设置（{} 个字符）", token.len()),
        ),
    }
}

/// 证书校验开关。
fn check_tls(context: &Context<'_>, _verbose: bool) -> CheckResult {
    if context.config.tls_reject_unauthorized {
        return ok("security.tls", "证书校验", "开启（默认）".to_owned());
    }
    CheckResult {
        id: "security.tls".to_owned(),
        level: CheckLevel::Warn,
        title: "证书校验已关闭".to_owned(),
        detail: "tls_reject_unauthorized = false 时，中间人可以读到邮箱授权码".to_owned(),
        action: Some(
            "把 tls_reject_unauthorized 改回 true；自签证书的服务器应当把根证书加进信任库"
                .to_owned(),
        ),
    }
}

/// 明文日志开关。
fn check_trace(context: &Context<'_>, _verbose: bool) -> CheckResult {
    if !context.config.relay.trace {
        return ok("security.trace", "明文日志", "关闭（默认）".to_owned());
    }
    CheckResult {
        id: "security.trace".to_owned(),
        level: CheckLevel::Warn,
        title: "trace 开着".to_owned(),
        detail: "日志里会有 LOGIN 命令，也就是邮箱授权码明文".to_owned(),
        action: Some("排查完把配置里的 trace 改回 false（或用 `burrow trace --off`）".to_owned()),
    }
}

/// 自更新配置。
fn check_update(context: &Context<'_>, _verbose: bool) -> CheckResult {
    let update = &context.config.update;
    if !update.enabled {
        return ok("update", "自更新", "已关闭".to_owned());
    }
    ok(
        "update",
        "自更新",
        format!(
            "通道 {}，每 {} 小时检查一次{}",
            update.channel.as_str(),
            update.check_interval_hours,
            update
                .base_url
                .as_deref()
                .map(|base| format!("，镜像 {base}"))
                .unwrap_or_default()
        ),
    )
}

/// 构造一条 OK。
fn ok(id: &str, title: &str, detail: String) -> CheckResult {
    CheckResult {
        id: id.to_owned(),
        level: CheckLevel::Ok,
        title: title.to_owned(),
        detail,
        action: None,
    }
}

/// 端口占用者诊断（`netstat`/`lsof` + 进程名）。
fn describe_port_holder(port: u16) -> Option<String> {
    let status = peon_burrow_service::probe_port(port).ok()?;
    if status.free {
        return Some("内核说端口是空的（可能是权限或地址绑定问题，而不是占用）".to_owned());
    }
    let who = match (status.owner_name.as_deref(), status.owner_pid) {
        (Some(name), Some(pid)) => format!("{name}（PID {pid}）"),
        (Some(name), None) => name.to_owned(),
        (None, Some(pid)) => format!("PID {pid}"),
        (None, None) => "查不到占用者（可能属于其它用户）".to_owned(),
    };
    Some(format!("被 {who} 占着"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CliOverrides, Config, EnvSource};

    struct Fixture {
        _directory: tempfile::TempDir,
        paths: Paths,
        config: Config,
    }

    impl Fixture {
        fn new(extra_toml: &str) -> Self {
            let directory = tempfile::tempdir().expect("tempdir");
            let paths = Paths::for_test(directory.path().join("data"));
            let file = {
                use crate::config::test_support::file_config;
                file_config(extra_toml)
            };
            let config = Config::merge(
                file,
                &EnvSource::default(),
                &CliOverrides::default(),
                Vec::new(),
            )
            .expect("merge");
            Self {
                _directory: directory,
                paths,
                config,
            }
        }

        fn context(&self) -> Context<'_> {
            Context {
                paths: &self.paths,
                config: &self.config,
            }
        }
    }

    #[test]
    fn the_registry_has_stable_ids() {
        let ids: Vec<&str> = registry().iter().map(|check| check.id).collect();
        assert_eq!(
            ids,
            vec![
                "config",
                "data-dir",
                "port",
                "security.token",
                "security.tls",
                "security.trace",
                "update"
            ]
        );
    }

    #[test]
    fn a_clean_setup_passes() {
        let fixture = Fixture::new("");
        let report = run_all(&fixture.context(), false);
        assert_eq!(report.checks.len(), registry().len());
        let failures: Vec<&str> = report
            .checks
            .iter()
            .filter(|check| check.level == CheckLevel::Fail)
            .map(|check| check.id.as_str())
            .collect();
        assert!(
            failures.is_empty(),
            "干净的临时目录不该有 FAIL：{failures:?}"
        );
    }

    #[test]
    fn a_non_loopback_host_without_a_token_is_a_failure() {
        let fixture = Fixture::new("host = \"0.0.0.0\"\nallowed_hosts = [\"imap.qq.com\"]\n");
        let result = check_token(&fixture.context(), false);
        assert_eq!(result.level, CheckLevel::Fail);
        assert!(result.action.unwrap().contains("token"));
    }

    #[test]
    fn a_short_token_warns() {
        let fixture = Fixture::new("token = \"short\"\n");
        assert_eq!(
            check_token(&fixture.context(), false).level,
            CheckLevel::Warn
        );
    }

    #[test]
    fn disabling_certificate_verification_warns_with_the_reason() {
        let fixture = Fixture::new("tls_reject_unauthorized = false\n");
        let result = check_tls(&fixture.context(), false);
        assert_eq!(result.level, CheckLevel::Warn);
        assert!(result.detail.contains("中间人"));
    }

    #[test]
    fn trace_and_config_warnings_are_surfaced() {
        let fixture = Fixture::new("trace = true\nunknown_key = 1\n");
        assert_eq!(
            check_trace(&fixture.context(), false).level,
            CheckLevel::Warn
        );
        assert_eq!(
            check_config(&fixture.context(), false).level,
            CheckLevel::Warn
        );
    }

    #[test]
    fn port_zero_is_a_warning_not_a_failure() {
        let fixture = Fixture::new("port = 0\n");
        assert_eq!(
            check_port(&fixture.context(), false).level,
            CheckLevel::Warn
        );
    }

    #[test]
    fn an_occupied_port_warns_with_the_next_step() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let fixture = Fixture::new(&format!("port = {port}\n"));
        let result = check_port(&fixture.context(), false);
        assert_eq!(result.level, CheckLevel::Warn);
        let action = result.action.unwrap();
        assert!(
            action.contains("burrow status") || action.contains("port"),
            "{action}"
        );
    }

    #[test]
    fn update_state_is_reported() {
        let fixture = Fixture::new("[update]\nchannel = \"beta\"\ncheck_interval_hours = 6\n");
        let result = check_update(&fixture.context(), false);
        assert_eq!(result.level, CheckLevel::Ok);
        assert!(result.detail.contains("beta"), "{}", result.detail);
        assert!(result.detail.contains('6'));
    }
}
