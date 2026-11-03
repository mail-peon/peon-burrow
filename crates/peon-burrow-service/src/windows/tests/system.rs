use super::*;

#[test]
fn system_install_writes_failure_actions_and_self_checks() {
    let runner = FakeRunner::new()
        .expect("") // sc query（存在性检查：空输出 = 没装）
        .expect("") // sc create
        .expect("") // sc description
        .expect("") // sc failure
        .expect(FAILURE_FIXTURE); // 自检：读注册表

    let host = WindowsSystemHost::with_runner("peon-burrow", runner.clone());
    let opts = system_opts();
    host.install(&opts).expect("install");

    let lines = runner.mutations().command_lines();
    assert!(lines[1].contains("sc create peon-burrow"));
    assert!(
        lines[1].contains("start= auto"),
        "开机启动 = auto：{}",
        lines[0]
    );
    assert!(
        lines[3].contains("sc failure peon-burrow"),
        "sc create 写不了 failure actions，必须紧跟 sc failure：{lines:?}"
    );
    assert!(lines[3].contains("actions= restart/5000/restart/5000/restart/5000"));
    assert!(
        lines[4].contains("reg query") && lines[4].contains("FailureActions"),
        "自检要读回注册表里的 FailureActions（`sc qfailure` 的文字会随语言变）：{}",
        lines[4]
    );
}

/// 检查每一条 `sc` 命令的**参数形状**：选项名与值必须是相邻的两个参数。
///
/// 真机踩过：`sc create … "start= auto"` → 退出码 1639「无效 start= 参数」。
/// 这条只能查 argv —— `display_line()` 把 `["start= auto"]` 和 `["start=", "auto"]`
/// 渲染成**完全一样**的一行，所以按命令行写断言是抓不住的。
#[test]
fn sc_options_are_passed_as_separate_arguments() {
    let runner = FakeRunner::new()
        .expect("") // sc query（存在性检查）
        .expect("") // sc create
        .expect("") // sc description
        .expect("") // sc failure
        .expect(FAILURE_FIXTURE); // 读注册表
    let host = WindowsSystemHost::with_runner("peon-burrow", runner.clone());
    let opts = system_opts();
    host.install(&opts).expect("install");

    let commands = commands_of(&runner);
    assert_sc_options_are_separate(&commands);

    // 正向断言：`start=` 与 `auto` 是相邻的两个参数
    // （第一条是存在性检查 `sc query`，所以 create 是第二条）
    let create = commands.get(1).expect("sc create");
    let index = create
        .args
        .iter()
        .position(|arg| arg == "start=")
        .expect("create 里要有 start=");
    assert_eq!(create.args.get(index + 1).map(String::as_str), Some("auto"));
    // binPath 的值里带空格（可执行文件路径 + 参数），它**必须**是一个参数
    let binpath = create
        .args
        .iter()
        .position(|arg| arg == "binPath=")
        .expect("create 里要有 binPath=");
    assert!(
        create
            .args
            .get(binpath + 1)
            .is_some_and(|value| value.contains(' ')),
        "binPath 的值是一个参数（含空格）：{:?}",
        create.args
    );
}

#[test]
fn sc_config_for_autostart_uses_separate_arguments_too() {
    // 同一个 bug 的另一处：系统服务的自启开关走 `sc config start= auto|demand`
    for on in [true, false] {
        let runner = FakeRunner::new().expect("");
        let host = WindowsSystemHost::with_runner("peon-burrow", runner.clone());
        host.set_autostart(on).expect("set_autostart");

        let commands = commands_of(&runner);
        assert_sc_options_are_separate(&commands);
        let config = commands.first().expect("sc config");
        let expected = if on { "auto" } else { "demand" };
        let index = config
            .args
            .iter()
            .position(|arg| arg == "start=")
            .expect("config 里要有 start=");
        assert_eq!(
            config.args.get(index + 1).map(String::as_str),
            Some(expected)
        );
    }
}

#[test]
fn installing_an_existing_system_service_skips_create_but_refreshes_the_command_line() {
    // `sc create` 对已存在的服务报 1073 —— 上一次失败留下的半成品会挡住每一次重装
    //（真机踩过：用户只能自己去 `sc delete`）。存在就跳过创建，只把其余配置补齐。
    //
    // ⚠️ 但**命令行必须刷新**：它随版本变（系统服务从 `… run` 变成
    // `… run --service <名字> --config … --data-dir …`）。不刷新的话重装「成功」了，
    // 服务跑的仍是老命令行 —— 每次启动都是 1053（真机踩过）。
    let runner = FakeRunner::new()
        .expect("SERVICE_NAME: peon-burrow\n        STATE              : 4  RUNNING")
        .expect("") // sc config（刷新 binPath + start）
        .expect("") // sc description
        .expect("") // sc failure
        .expect(FAILURE_FIXTURE); // 自检
    let host = WindowsSystemHost::with_runner("peon-burrow", runner.clone());
    let opts = system_opts();
    host.install(&opts).expect("install");

    let commands = commands_of(&runner);
    let lines = runner.mutations().command_lines();
    assert!(
        !lines.iter().any(|line| line.contains("sc create")),
        "已存在就不能再 create：{lines:?}"
    );
    assert!(
        lines.iter().any(|line| line.contains("sc failure")),
        "失败策略仍要写：{lines:?}"
    );

    let config = commands
        .iter()
        .find(|command| command.args.first().is_some_and(|arg| arg == "config"))
        .expect("已存在的服务要刷新命令行");
    let bin_path = config
        .args
        .iter()
        .position(|arg| arg == "binPath=")
        .and_then(|index| config.args.get(index + 1))
        .expect("config 里要有 binPath=");
    assert!(
        bin_path.contains("--service"),
        "刷新后的命令行要带上 `--service`（否则服务永远不报到）：{bin_path}"
    );
    assert_sc_options_are_separate(&commands);
}

#[test]
fn system_uninstall_stops_before_deleting() {
    // `sc delete` 只是打标记：不停就删，进程还会继续跑（「卸载了但扩展还能收信」）。
    // 停完之后**还要核实**真的报了 STOPPED 才允许删 —— 所以中间多一次状态查询。
    let runner = FakeRunner::new()
        .expect("STATE : 4 RUNNING") // stop → is_stopped（还在跑）
        .expect("") // sc stop
        .expect("STATE : 1 STOPPED") // 核实：真的停了
        .expect(""); // sc delete

    let host = WindowsSystemHost::with_runner("peon-burrow", runner.clone());
    host.uninstall().expect("uninstall");

    let lines = runner.mutations().command_lines();
    assert_eq!(lines.len(), 4, "查 → 停 → 核实 → 删：{lines:?}");
    assert!(lines[1].contains("sc stop"), "{lines:?}");
    assert!(lines[3].contains("sc delete"), "{lines:?}");
    assert_eq!(runner.expectations_left(), 0, "预置的输出必须被用完");
}

#[test]
fn stopping_a_stopped_system_service_is_a_noop() {
    // 真机踩过：中继自己从控制面停下来之后再点「停止 / 卸载」，
    // `sc stop` 报 **1061**「服务无法在此时接受控制信息」（服务停着时
    // `dwControlsAccepted` 是 0），老实现只容忍 1062/1060 → 卸载被这一步挡死。
    // 现在的判据是**状态**不是 `sc stop` 的退出码：已经停着就一条命令都不发。
    let runner = FakeRunner::new().expect("STATE : 1 STOPPED"); // 只查一次状态

    let host = WindowsSystemHost::with_runner("peon-burrow", runner.clone());
    host.stop().expect("已经停着 = 成功");

    let lines = runner.mutations().command_lines();
    assert_eq!(lines.len(), 1, "只查状态，不发 sc stop：{lines:?}");
    assert_eq!(runner.expectations_left(), 0);
}

#[test]
fn a_rejected_sc_stop_still_counts_if_the_service_really_is_stopped() {
    // 竞态：查完状态它刚好停了（或被别的路径停了）→ `sc stop` 回 1061/1062。
    // 那不算失败，但也**不能**直接当成功 —— 由核实说了算。
    let runner = FakeRunner::new()
        .expect("STATE : 4 RUNNING") // is_stopped → 还在跑
        .expect_failure("[SC] ControlService 失败 1061: 服务无法在此时接受控制信息。")
        .expect("STATE : 1 STOPPED"); // 核实

    let host = WindowsSystemHost::with_runner("peon-burrow", runner.clone());
    host.stop().expect("它确实停了，就该成功");

    let lines = runner.mutations().command_lines();
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert!(lines[1].contains("sc stop"), "{lines:?}");
}

#[test]
fn stopping_fails_when_the_service_never_reports_stopped() {
    // 卡住的服务（没注册控制码处理的老进程 / 卡在启动中）**必须**报错：
    // 卸载要能带着它去 `sc delete` 就完了（「卸载了但扩展还能收信」）。
    let runner = FakeRunner::new()
        .expect("STATE : 4 RUNNING") // is_stopped → 还在跑
        .expect("") // sc stop：请求发出去了
        .expect("STATE : 4 RUNNING"); // 核实：它还是没停

    let host = WindowsSystemHost::with_runner("peon-burrow", runner.clone())
        // 超时设 0：测试不该真等 10 秒（第一轮核实就到截止时间）
        .with_stop_timeout(Duration::ZERO);
    let error = host.stop().expect_err("停不下来必须报错");

    assert!(
        matches!(error, ServiceError::StopTimedOut { .. }),
        "{error:?}"
    );
    assert!(
        error.action().contains("taskkill"),
        "下一步要能照做：{}",
        error.action()
    );
    let lines = runner.mutations().command_lines();
    assert!(
        lines.iter().any(|line| line.contains("sc stop")),
        "状态是 RUNNING 就该发过 sc stop：{lines:?}"
    );
    assert_eq!(runner.expectations_left(), 0);
}

#[test]
fn system_install_rejects_logon_autostart() {
    let runner = FakeRunner::new();
    let host = WindowsSystemHost::with_runner("peon-burrow", runner.clone());
    let mut opts = system_opts();
    opts.autostart = Autostart::Logon;
    let error = host
        .install(&opts)
        .expect_err("系统服务 + 登录自启 是非法组合");
    assert!(matches!(error, ServiceError::InvalidOptions { .. }));
    assert_eq!(
        runner.mutations().run_count(),
        0,
        "校验失败时一条命令都不该发"
    );
}

#[test]
fn system_install_refuses_a_command_line_without_the_service_flag() {
    // 这条守卫防的是真机踩过的那个 bug：注册信息里少了 `--service`，
    // 服务装得上、`sc start` 报 1053（进程根本不向 SCM 报到）。
    let runner = FakeRunner::new();
    let host = WindowsSystemHost::with_runner("peon-burrow", runner.clone());
    let mut opts = system_opts();
    opts.args = vec!["run".to_owned()];

    let error = host.install(&opts).expect_err("缺 --service 必须被拦住");
    assert!(
        matches!(error, ServiceError::InvalidOptions { .. }),
        "{error:?}"
    );
    assert!(error.to_string().contains("--service"), "{error}");
    assert_eq!(
        runner.mutations().run_count(),
        0,
        "守卫必须在发任何命令之前生效"
    );
}

#[test]
fn system_install_refuses_a_service_flag_that_does_not_match_the_name() {
    // 名字对不上同样是 1053：`StartServiceCtrlDispatcher` 按名字找服务。
    let runner = FakeRunner::new();
    let host = WindowsSystemHost::with_runner("peon-burrow", runner.clone());
    let mut opts = system_opts();
    opts.args = vec![
        "run".to_owned(),
        "--service".to_owned(),
        "other-name".to_owned(),
    ];

    let error = host.install(&opts).expect_err("名字不一致必须被拦住");
    assert!(error.to_string().contains("other-name"), "{error}");
    assert!(error.to_string().contains("peon-burrow"), "{error}");
    assert_eq!(runner.mutations().run_count(), 0);
}

#[test]
fn system_restart_policy_inspection() {
    verify_failure_actions(FAILURE_FIXTURE, "peon-burrow").expect("三个 restart/5000 就该通过");

    // 只有 RUN（跑命令）不算重启：类型 3 是 run
    let run_only = reg_binary_fixture("80510100000000000000000001000000140000000300000088130000");
    let error = verify_failure_actions(&run_only, "peon-burrow").expect_err("run 不算重启");
    assert!(
        error.to_string().contains("run/5000ms"),
        "错误里要写清读到的是什么动作：{error}"
    );

    // 延迟太长：自更新后的重启要在用户察觉前完成（60000ms = 0xEA60）
    let slow = reg_binary_fixture("80510100000000000000000001000000140000000100000060EA0000");
    let error = verify_failure_actions(&slow, "peon-burrow").expect_err("延迟 60 秒太慢");
    assert!(
        error.to_string().contains("60000"),
        "要报出读到的实际值：{error}"
    );
    assert!(
        error.to_string().contains("peon-burrow"),
        "要点名是哪个服务：{error}"
    );

    // 注册表里根本没有 FailureActions 这个值
    let none = "错误: 系统找不到指定的注册表项或值。";
    let error = verify_failure_actions(none, "peon-burrow").expect_err("没有策略要报错");
    assert!(error.action().contains("install"), "{}", error.action());
}

#[test]
fn system_status_reads_sc_output() {
    let query = "SERVICE_NAME: peon-burrow\n        TYPE               : 10  WIN32_OWN_PROCESS\n        STATE              : 4  RUNNING\n";
    let config = "SERVICE_NAME: peon-burrow\n        START_TYPE         : 2   AUTO_START\n        BINARY_PATH_NAME   : \"C:\\Program Files\\peon-burrow\\burrow.exe\" run\n";
    assert_eq!(inspect_sc_autostart(config), Autostart::Boot);
    assert_eq!(
        inspect_sc_autostart("START_TYPE : 3 DEMAND_START"),
        Autostart::Off
    );
    assert_eq!(
        inspect_sc_binary_path(config).as_deref(),
        Some(r"C:\Program Files\peon-burrow\burrow.exe")
    );
    assert!(query.contains("RUNNING"), "（夹具自检）");
}

#[test]
fn the_last_exit_code_comes_from_sc_query() {
    // 真机形状（字段名不本地化，冒号后是十进制 + 十六进制）
    let stopped = "SERVICE_NAME: peon-burrow\n        STATE              : 1  STOPPED\n        WIN32_EXIT_CODE    : 3  (0x3)\n";
    assert_eq!(inspect_sc_exit_code(stopped), Some(3));

    let clean = "        WIN32_EXIT_CODE    : 0  (0x0)";
    assert_eq!(inspect_sc_exit_code(clean), Some(0));

    // 读不出来就是读不出来：`None` 比谎报 0 好（界面会把 0 当成「上次干净退出」）
    assert_eq!(inspect_sc_exit_code("SERVICE_NAME: peon-burrow"), None);
}
