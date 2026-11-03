use super::*;
use crate::runner::FakeRunner;

fn sample_opts() -> InstallOptions {
    let mut opts = InstallOptions::new(
        "peon-burrow",
        "/home/me/.local/share/peon-burrow/bin/burrow",
    );
    opts.args = vec!["run".to_owned()];
    opts
}

/// `systemctl show` 的输出（只有被请求的属性）。
const SHOW_FIXTURE: &str = "\
ActiveState=active
SubState=running
UnitFileState=enabled
";

/// `systemctl show -p Restart -p RestartUSec` 的输出。
const SHOW_RESTART_FIXTURE: &str = "Restart=always\nRestartUSec=2s\n";

#[test]
fn unit_carries_restart_and_install_section() {
    let unit = unit_body(&sample_opts());
    assert!(unit.contains("Restart=always"), "systemd 默认是 Restart=no");
    assert!(
        unit.contains("RestartSec=2"),
        "重启间隔要够短（adr-0003 § 1）"
    );
    assert!(unit.contains("[Install]") && unit.contains("WantedBy=default.target"));
    assert!(
        unit.contains("ExecStart=-/home/me/.local/share/peon-burrow/bin/burrow run"),
        "前导 - 让自更新的非零退出码不被当成启动失败：{unit}"
    );
    assert!(unit.contains("WorkingDirectory=/home/me/.local/share/peon-burrow/bin"));
}

#[test]
fn unit_quotes_paths_with_spaces() {
    let opts = InstallOptions::new("relay", "/opt/my relay/burrow");
    let unit = unit_body(&opts);
    assert!(
        unit.contains(r#"ExecStart=-"/opt/my relay/burrow""#),
        "带空格的路径要引用：{unit}"
    );
}

#[test]
fn unit_name_is_normalized() {
    assert_eq!(unit_name("relay"), "relay.service");
    assert_eq!(unit_name("relay.service"), "relay.service");
    assert!(is_valid_unit_name("peon-burrow"));
    assert!(is_valid_unit_name("my.app@1"));
    assert!(
        !is_valid_unit_name("../etc/cron.d/x"),
        "不能借 unit 名写到任意路径"
    );
    assert!(!is_valid_unit_name("a/b"));
}

#[test]
fn restart_properties_are_read_from_systemctl_show() {
    inspect_restart_properties(SHOW_RESTART_FIXTURE).expect("Restart=always 应当通过");
    // on-failure 也算「失败会回来」
    inspect_restart_properties("Restart=on-failure\nRestartUSec=2s\n").expect("on-failure 通过");

    let error = inspect_restart_properties("Restart=no\nRestartUSec=100ms\n")
        .expect_err("Restart=no 就必须报错");
    assert!(
        error.to_string().contains("Restart=no"),
        "要报出读到的实际值：{error}"
    );
    assert!(error.to_string().contains("RestartSec"));
    assert!(
        error.action().contains("install"),
        "要给下一步：{}",
        error.action()
    );

    assert!(
        inspect_restart_properties("").is_err(),
        "读不到 Restart= 时绝不能当作通过"
    );
}

#[test]
fn restart_line_fallback_reads_the_unit_text() {
    let unit = unit_body(&sample_opts());
    inspect_restart_line(&unit).expect("自己生成的 unit 必须通过");
    assert!(inspect_restart_line("[Service]\nType=simple\n").is_err());
    // `systemctl cat` 会在正文前加 `# /path/to/unit` 注释行，不能干扰解析
    let catted = format!("# /home/me/.config/systemd/user/peon-burrow.service\n{unit}");
    inspect_restart_line(&catted).expect("带 `systemctl cat` 头部的输出也要能解析");
}

#[test]
fn systemd_state_parsing() {
    let state = inspect_systemd_state(SHOW_FIXTURE);
    assert_eq!(state.active_state.as_deref(), Some("active"));
    assert_eq!(state.sub_state.as_deref(), Some("running"));
    assert!(state.running());
    assert_eq!(state.autostart(), Autostart::Boot);

    let idle =
        inspect_systemd_state("ActiveState=inactive\nSubState=dead\nUnitFileState=disabled\n");
    assert!(!idle.running(), "「已安装·未运行」是正常状态");
    assert_eq!(idle.autostart(), Autostart::Off);

    let empty = inspect_systemd_state("ActiveState=\nSubState=\nUnitFileState=\n");
    assert_eq!(empty, SystemdState::default(), "空值不该被当成有效状态");
}

#[test]
fn exec_start_is_read_back_without_the_dash_or_quotes() {
    let unit = unit_body(&sample_opts());
    assert_eq!(
        inspect_exec_start(&unit).as_deref(),
        Some("/home/me/.local/share/peon-burrow/bin/burrow")
    );
    let quoted = unit_body(&InstallOptions::new("relay", "/opt/my relay/burrow"));
    assert_eq!(
        inspect_exec_start(&quoted).as_deref(),
        Some("/opt/my relay/burrow")
    );
}

#[test]
fn install_rejects_a_name_that_could_write_outside_the_unit_directory() {
    // unit 名会拼进 `/etc/systemd/system/<name>.service`：带 `../` 的名字等于任意写。
    let runner = FakeRunner::new();
    let host = LinuxHost::with_runner("peon-burrow", ServiceLevel::User, runner.clone());
    let mut opts = sample_opts();
    opts.name = "../../etc/cron.d/evil".to_owned();

    let error = host.install(&opts).expect_err("非法 unit 名必须被拒");
    assert!(matches!(error, ServiceError::InvalidOptions { .. }));
    assert!(
        runner.mutations().run_count() == 0,
        "校验失败时一条命令都不该发"
    );
}

#[test]
fn user_install_writes_unit_reloads_enables_starts_then_self_checks() {
    let runner = FakeRunner::new()
        .expect("") // daemon-reload
        .expect("") // enable
        .expect("") // start
        .expect(SHOW_RESTART_FIXTURE); // 自检

    let host = LinuxHost::with_runner("peon-burrow", ServiceLevel::User, runner.clone());
    host.install(&sample_opts()).expect("install");

    let mutations = runner.mutations();
    let lines = mutations.command_lines();
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert!(
        lines[0].contains("systemctl --user daemon-reload"),
        "{}",
        lines[0]
    );
    assert!(lines[1].contains("systemctl --user enable peon-burrow.service"));
    assert!(lines[2].contains("systemctl --user start peon-burrow.service"));
    assert!(
        lines[3].contains("systemctl --user show"),
        "自检要读回来：{}",
        lines[3]
    );
    assert_eq!(mutations.files().len(), 1, "unit 必须先落盘");
    assert!(mutations.file_content(0).contains("Restart=always"));
}

#[test]
fn install_skips_enable_when_autostart_is_off() {
    let runner = FakeRunner::new()
        .expect("") // daemon-reload
        .expect("") // start
        .expect(SHOW_RESTART_FIXTURE);
    let host = LinuxHost::with_runner("peon-burrow", ServiceLevel::User, runner.clone());
    let mut opts = sample_opts();
    opts.autostart = Autostart::Off;
    host.install(&opts).expect("install");

    let lines = runner.mutations().command_lines();
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert!(
        !lines.iter().any(|line| line.contains("enable")),
        "Autostart::Off 不该发 enable：{lines:?}"
    );
}

#[test]
fn install_fails_when_restart_is_not_always() {
    let runner = FakeRunner::new()
        .expect("")
        .expect("")
        .expect("")
        // 自检：Restart=no —— 这正是「装完看着挺好、崩了不回来」
        .expect("Restart=no\nRestartUSec=100ms\n");
    let host = LinuxHost::with_runner("peon-burrow", ServiceLevel::User, runner);
    let error = host
        .install(&sample_opts())
        .expect_err("Restart=no 必须报错");
    assert!(matches!(error, ServiceError::RestartPolicyMissing { .. }));
}

#[test]
fn verify_falls_back_to_the_unit_text() {
    let runner = FakeRunner::new()
        // systemctl show -p Restart 失败（老版本 / 权限问题）
        .expect_failure("Failed to get properties: Access denied")
        // 退回 systemctl cat
        .expect(unit_body(&sample_opts()));
    let host = LinuxHost::with_runner("peon-burrow", ServiceLevel::User, runner.clone());
    host.verify_restart_policy().expect("退路也要能自检通过");

    let lines = runner.mutations().command_lines();
    assert!(lines[1].contains("systemctl --user cat peon-burrow.service"));
}

#[test]
fn system_level_uses_no_user_flag() {
    let runner = FakeRunner::new()
        .expect("") // daemon-reload
        .expect("") // enable
        .expect("") // start
        .expect(SHOW_RESTART_FIXTURE);
    let host = LinuxHost::with_runner("peon-burrow", ServiceLevel::System, runner.clone());
    let mut opts = sample_opts();
    opts.level = ServiceLevel::System;
    opts.autostart = Autostart::Boot;
    host.install(&opts).expect("install");

    let lines = runner.mutations().command_lines();
    assert!(
        lines[0].starts_with("systemctl daemon-reload"),
        "{}",
        lines[0]
    );
    assert!(
        !lines.iter().any(|line| line.contains("--user")),
        "系统级不能带 --user：{lines:?}"
    );
    match runner.mutations().files().first() {
        Some(crate::runner::Mutation::Install(file)) => assert_eq!(
            file.path.display().to_string(),
            "/etc/systemd/system/peon-burrow.service"
        ),
        other => panic!("第 1 个动作应当是写 unit：{other:?}"),
    }
}

#[test]
fn uninstall_stops_disables_removes_then_reloads() {
    let mut runner = FakeRunner::new();
    // stop / disable 用 best-effort：即使失败也要继续
    runner = runner.expect_failure("Unit peon-burrow.service not loaded.");
    runner = runner.expect_failure("Unit file peon-burrow.service does not exist.");
    runner = runner.expect(""); // daemon-reload
    let host = LinuxHost::with_runner("peon-burrow", ServiceLevel::User, runner.clone());
    host.uninstall()
        .expect("卸载要幂等：停不掉的服务也要能删掉");

    let mutations = runner.mutations();
    let lines = mutations.command_lines();
    assert!(lines[0].contains("stop"));
    assert!(lines[1].contains("disable"));
    assert!(lines[2].contains("daemon-reload"));
    assert!(
        matches!(mutations.nth(3), Some(crate::runner::Mutation::Remove(_))),
        "unit 文件要在停止/关自启之后删：{:?}",
        mutations.all()
    );
}

#[test]
fn status_reports_not_installed_when_systemctl_is_absent() {
    let runner = FakeRunner::new().expect_failure("systemctl: command not found");
    let host = LinuxHost::with_runner("peon-burrow", ServiceLevel::User, runner);
    let status = host.status().expect("探测不到 systemd 不算致命错误");
    assert!(!status.installed);
}

#[test]
fn status_reads_show_and_cat() {
    let runner = FakeRunner::new()
        .expect(SHOW_FIXTURE)
        .expect(unit_body(&sample_opts()));
    let host = LinuxHost::with_runner("peon-burrow", ServiceLevel::User, runner);
    let status = host.status().expect("status");
    assert!(status.installed);
    assert!(status.running);
    assert_eq!(status.autostart, Some(Autostart::Boot));
    assert!(
        status.restart_policy_configured,
        "Restart=always 在，就该报 true"
    );
    assert_eq!(
        status.binary_path.as_deref(),
        Some("/home/me/.local/share/peon-burrow/bin/burrow")
    );
    assert!(!status.requires_elevation, "用户级零提权");
}

#[test]
fn xdg_fallback_is_documented_not_used() {
    let body = xdg_autostart_body(&sample_opts());
    assert!(body.contains("Type=Application"));
    assert!(body.contains("Exec=/home/me/.local/share/peon-burrow/bin/burrow run"));
    assert!(
        !body.contains("Restart="),
        ".desktop 没有失败重启能力 —— 这条退路必须让用户知道"
    );
    assert!(body.contains("无失败重启"));
}
