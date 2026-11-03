use super::*;
use peon_burrow_core::RelayOptions;
use peon_burrow_ipc_types::{Autostart, ServiceLevel};
use std::path::Path;
use std::time::Duration;

fn file_config(text: &str) -> FileConfig {
    let table: toml::Table = toml::from_str(text).expect("toml");
    table.try_into().expect("config")
}

fn merged(text: &str, env: &EnvSource, cli: &CliOverrides) -> Config {
    let mut warnings = Vec::new();
    Config::merge(file_config(text), env, cli, Vec::new())
        .map(|mut config| {
            config.warnings.append(&mut warnings);
            config
        })
        .expect("merge")
}

#[test]
fn defaults_come_from_core() {
    let config = merged("", &EnvSource::default(), &CliOverrides::default());
    let core_default = RelayOptions::default();
    assert_eq!(config.relay.port, core_default.port);
    assert_eq!(config.relay.host, core_default.host);
    assert_eq!(config.relay.max_connections, core_default.max_connections);
    assert_eq!(
        config.relay.watch_retry_delays,
        core_default.watch_retry_delays
    );
    assert_eq!(config.log_level, LogLevel::Info);
    assert_eq!(config.service.level, ServiceLevel::User);
    assert_eq!(config.service.autostart, Autostart::Logon);
    assert_eq!(config.update.channel, UpdateChannel::Stable);
}

#[test]
fn cli_beats_env_beats_file() {
    let env = EnvSource::from_pairs([("PORT", "2000"), ("HOST", "env-host")]);
    let cli = CliOverrides {
        port: Some(3000),
        ..CliOverrides::default()
    };

    let config = merged("port = 1000\nhost = \"file-host\"\n", &env, &cli);
    assert_eq!(config.relay.port, 3000, "CLI 最高");
    assert_eq!(config.relay.host, "env-host", "ENV 压过文件");

    let env_only = EnvSource::from_pairs([("PORT", "2000")]);
    let config = merged("port = 1000\n", &env_only, &CliOverrides::default());
    assert_eq!(config.relay.port, 2000, "ENV 压过文件");

    let config = merged(
        "port = 1000\n",
        &EnvSource::default(),
        &CliOverrides::default(),
    );
    assert_eq!(config.relay.port, 1000, "文件压过默认值");
}

#[test]
fn the_legacy_environment_variables_still_work() {
    let env = EnvSource::from_pairs([
        ("HOST", "127.0.0.1"),
        ("PORT", "41317"),
        ("RELAY_TOKEN", "  s3cret  "),
        ("ALLOWED_HOSTS", "imap.qq.com, *.163.com ,, imap.gmail.com"),
        ("TLS_REJECT_UNAUTHORIZED", "0"),
        ("RELAY_TRACE", "1"),
    ]);
    let config = merged("", &env, &CliOverrides::default());

    assert_eq!(config.relay.port, 41317);
    assert_eq!(config.token.as_deref(), Some("s3cret"), "应当去掉首尾空白");
    assert_eq!(
        config.allowed_hosts,
        vec!["imap.qq.com", "*.163.com", "imap.gmail.com"],
        "逗号分隔、忽略空项"
    );
    assert!(!config.tls_reject_unauthorized);
    assert!(config.relay.trace);
}

#[test]
fn only_a_literal_zero_disables_certificate_verification() {
    for (value, expected) in [
        ("0", false),
        ("false", true),
        ("no", true),
        ("off", true),
        ("1", true),
    ] {
        let env = EnvSource::from_pairs([("TLS_REJECT_UNAUTHORIZED", value)]);
        let config = merged("", &env, &CliOverrides::default());
        assert_eq!(config.tls_reject_unauthorized, expected, "value = {value}");
    }
}

#[test]
fn an_invalid_enum_lists_the_allowed_values() {
    let env = EnvSource::from_pairs([("RELAY_LOG_LEVEL", "verbose")]);
    let error = Config::merge(
        FileConfig::default(),
        &env,
        &CliOverrides::default(),
        Vec::new(),
    )
    .expect_err("should fail");
    let message = error.to_string();
    assert!(message.contains("verbose"), "{message}");
    for allowed in LogLevel::ALL {
        assert!(message.contains(allowed), "缺少可选值 {allowed}：{message}");
    }

    let env = EnvSource::from_pairs([("RELAY_TRACE", "maybe")]);
    let error = Config::merge(
        FileConfig::default(),
        &env,
        &CliOverrides::default(),
        Vec::new(),
    )
    .expect_err("should fail");
    assert!(error.to_string().contains("1/true/yes/on"), "{}", error);
}

#[test]
fn an_invalid_toml_enum_reports_the_options() {
    let mut warnings = Vec::new();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("relay.toml");
    std::fs::write(&path, "[service]\nmode = \"root\"\n").expect("write");
    let error = parse_file(&path, &mut warnings).expect_err("should fail");
    let message = error.to_string();
    assert!(message.contains("user | system"), "{message}");
}

#[test]
fn unknown_fields_warn_instead_of_failing() {
    let mut warnings = Vec::new();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("relay.toml");
    std::fs::write(
        &path,
        "port = 41316\nmy_note = \"hi\"\n\n[service]\nmode = \"user\"\nnope = 1\n",
    )
    .expect("write");

    let config = parse_file(&path, &mut warnings).expect("should parse");
    assert_eq!(config.port, Some(41316));
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(warnings.iter().any(|warning| warning.contains("my_note")));
    assert!(warnings.iter().any(|warning| warning.contains("nope")));
}

#[test]
fn a_missing_file_is_not_an_error() {
    let paths = Paths::for_test("C:\\definitely\\not\\there");
    let config = Config::load(&paths, &EnvSource::default(), &CliOverrides::default())
        .expect("missing file should fall back to defaults");
    assert_eq!(config.relay.port, RelayOptions::default().port);
}

#[test]
fn a_non_loopback_host_requires_a_token_and_an_allow_list() {
    let config = merged(
        "host = \"0.0.0.0\"\n",
        &EnvSource::default(),
        &CliOverrides::default(),
    );
    let error = config.validate().expect_err("should fail");
    assert_eq!(error.exit_code(), crate::exit::ExitCode::Config);
    assert!(error.to_string().contains("token"), "{error}");

    let config = merged(
        "host = \"0.0.0.0\"\ntoken = \"s3cret\"\n",
        &EnvSource::default(),
        &CliOverrides::default(),
    );
    let error = config.validate().expect_err("should fail");
    assert!(error.to_string().contains("allowed_hosts"), "{error}");

    let config = merged(
        "host = \"0.0.0.0\"\ntoken = \"s3cret\"\nallowed_hosts = [\"imap.qq.com\"]\n",
        &EnvSource::default(),
        &CliOverrides::default(),
    );
    config.validate().expect("both set is fine");
}

#[test]
fn loopback_hosts_are_recognised() {
    for host in ["127.0.0.1", "localhost", "::1", "[::1]"] {
        let config = merged(
            &format!("host = \"{host}\"\n"),
            &EnvSource::default(),
            &CliOverrides::default(),
        );
        config
            .validate()
            .unwrap_or_else(|error| panic!("{host} 应当是本机地址：{error}"));
    }
}

#[test]
fn limits_and_backoff_ladders_are_validated() {
    let config = merged(
        "max_connections = 0\n",
        &EnvSource::default(),
        &CliOverrides::default(),
    );
    assert!(config.validate().is_err());

    let config = merged(
        "max_connections = 9999\n",
        &EnvSource::default(),
        &CliOverrides::default(),
    );
    assert!(config.validate().is_err());

    let config = merged(
        "watch_retry_delays_secs = []\n",
        &EnvSource::default(),
        &CliOverrides::default(),
    );
    assert!(
        config
            .validate()
            .expect_err("empty")
            .to_string()
            .contains("不能为空")
    );

    let config = merged(
        "watch_retry_delays_secs = [5, 2]\n",
        &EnvSource::default(),
        &CliOverrides::default(),
    );
    assert!(
        config
            .validate()
            .expect_err("not increasing")
            .to_string()
            .contains("递增")
    );

    let config = merged(
        "watch_retry_delays_secs = [0, 5]\n",
        &EnvSource::default(),
        &CliOverrides::default(),
    );
    assert!(
        config
            .validate()
            .expect_err("zero")
            .to_string()
            .contains("≥ 1")
    );
}

#[test]
fn security_related_settings_produce_persistent_warnings() {
    let config = merged(
        "tls_reject_unauthorized = false\ntrace = true\n",
        &EnvSource::default(),
        &CliOverrides::default(),
    );
    let warnings = config.startup_warnings();
    assert!(warnings.iter().any(|warning| warning.contains("中间人")));
    assert!(warnings.iter().any(|warning| warning.contains("授权码")));
}

#[test]
fn the_deprecated_backpressure_field_warns() {
    let config = merged(
        "backpressure_high_water_mib = 16\n",
        &EnvSource::default(),
        &CliOverrides::default(),
    );
    assert!(
        config
            .warnings
            .iter()
            .any(|warning| warning.contains("backpressure")),
        "{:?}",
        config.warnings
    );
}

#[test]
fn relay_options_carry_the_merged_values() {
    let config = merged(
        "port = 41319\nidle_timeout_secs = 60\nwatch_reidle_secs = 120\n",
        &EnvSource::default(),
        &CliOverrides::default(),
    );
    let options = config.relay_options();
    assert_eq!(options.port, 41319);
    assert_eq!(options.idle_timeout, Duration::from_secs(60));
    assert_eq!(options.watch_reidle, Duration::from_secs(120));
}

#[test]
fn the_config_path_can_be_overridden_by_env_then_cli() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path: &Path = directory.path();
    std::fs::write(path.join("a.toml"), "port = 1111\n").expect("write");
    std::fs::write(path.join("b.toml"), "port = 2222\n").expect("write");

    let paths = Paths::for_test(path.join("data"));
    let env = EnvSource::from_pairs([(
        "RELAY_CONFIG",
        path.join("a.toml").to_string_lossy().to_string(),
    )]);
    let config = Config::load(&paths, &env, &CliOverrides::default()).expect("load");
    assert_eq!(config.relay.port, 1111);

    let cli = CliOverrides {
        config: Some(path.join("b.toml")),
        ..CliOverrides::default()
    };
    let config = Config::load(&paths, &env, &cli).expect("load");
    assert_eq!(config.relay.port, 2222, "CLI 的 --config 压过 RELAY_CONFIG");
}
