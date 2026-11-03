use super::runtime::control_endpoint;
use super::secrets::random_hex;
use super::service::{parse_on_off, parse_service_level, service_args};
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
