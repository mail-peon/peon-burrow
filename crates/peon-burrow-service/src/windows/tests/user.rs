use super::*;

#[test]
fn user_install_registers_then_self_checks() {
    let runner = FakeRunner::new()
        // 1) Register-ScheduledTask
        .expect("")
        // 2) 自检：schtasks /Query /XML
        .expect(task_xml_response());

    let host = WindowsUserHost::with_runner("peon-burrow", runner.clone());
    host.install(&sample_opts()).expect("install 应当成功");

    let mutations = runner.mutations();
    let lines = mutations.command_lines();
    assert_eq!(lines.len(), 2, "安装就是「注册 + 自检」两条命令：{lines:?}");
    assert!(lines[0].starts_with("powershell -NoProfile -NonInteractive -Command"));
    assert!(
        lines[0].contains("Register-ScheduledTask"),
        "用户级必须走任务计划程序：{}",
        lines[0]
    );
    assert!(
        lines[1].contains("schtasks /Query /TN"),
        "第 9a 步必须把注册信息读回来：{}",
        lines[1]
    );
    assert_eq!(runner.expectations_left(), 0, "预置的输出必须被用完");
}

#[test]
fn user_install_feeds_the_xml_through_stdin() {
    let runner = FakeRunner::new().expect("").expect(task_xml_response());
    let host = WindowsUserHost::with_runner("peon-burrow", runner.clone());
    host.install(&sample_opts()).expect("install");

    match runner.mutations().nth(0) {
        Some(crate::runner::Mutation::Run(spec)) => {
            let stdin = spec.stdin.expect("任务 XML 必须通过标准输入喂进去");
            assert!(stdin.contains("<RestartOnFailure>"), "喂进去的就是那份 XML");
            assert!(
                stdin.contains("<Hidden>true</Hidden>"),
                "XML 里必须有隐藏设置"
            );
        }
        other => panic!("第 1 个动作应当是命令：{other:?}"),
    }
}

#[test]
fn user_install_fails_when_the_restart_policy_did_not_land() {
    // 自检读回来的 XML 里没有 RestartOnFailure：这正是「装完看着挺好，崩了不回来」
    let runner = FakeRunner::new()
        .expect("")
        .expect("<Task><Settings><Hidden>true</Hidden></Settings></Task>");

    let host = WindowsUserHost::with_runner("peon-burrow", runner);
    let error = host
        .install(&sample_opts())
        .expect_err("策略没写进去就必须报错");
    assert!(matches!(error, ServiceError::RestartPolicyMissing { .. }));
}

#[test]
fn user_lifecycle_actions_are_idempotent_when_not_installed() {
    // 没装过时：卸载 / 自启开关都该是「已经是我们想要的状态」，不是错误。
    let runner = FakeRunner::new()
        // uninstall → is_registered → /Query /XML 失败
        .expect_failure("错误: 系统找不到指定的文件。")
        // set_autostart(false)
        .expect_failure("错误: 系统找不到指定的文件。");
    let host = WindowsUserHost::with_runner("peon-burrow", runner.clone());

    host.uninstall().expect("没装过时卸载必须成功（幂等）");
    let _ = host.set_autostart(false);
    assert_eq!(runner.expectations_left(), 0, "预置的输出必须被用完");
    assert_eq!(
        runner.mutations().run_count(),
        2,
        "只查了一次注册状态，没有再发 /Delete"
    );
}

#[test]
fn user_uninstall_and_autostart_use_schtasks() {
    let runner = FakeRunner::new()
        // uninstall → is_registered → /Query /XML
        .expect(task_xml_response())
        // uninstall → stop → is_running → /Query（没在跑）
        .expect("状态: 就绪")
        // /Delete
        .expect("成功: 计划任务已删除")
        // set_autostart(false)
        .expect("成功")
        // set_autostart(true)
        .expect("成功");

    let host = WindowsUserHost::with_runner("peon-burrow", runner.clone());
    host.uninstall().expect("uninstall");
    host.set_autostart(false).expect("autostart off");
    host.set_autostart(true).expect("autostart on");

    let lines = runner.mutations().command_lines();
    assert!(lines[2].contains("schtasks /Delete /TN"));
    assert!(lines[2].contains("/F"), "卸载不该弹确认框：{}", lines[2]);
    assert!(lines[3].contains("/Change") && lines[3].contains("/Disable"));
    assert!(lines[4].contains("/Change") && lines[4].contains("/Enable"));
}

#[test]
fn user_uninstall_ends_a_running_task_before_deleting_it() {
    // 任务删掉不等于进程结束：正在跑的中继会继续活着（「卸载了但扩展还能收信」）。
    let runner = FakeRunner::new()
        .expect(task_xml_response()) // is_registered
        .expect("状态: 正在运行") // stop → is_running
        .expect("成功: 计划任务已终止") // /End
        .expect("成功: 计划任务已删除"); // /Delete

    let host = WindowsUserHost::with_runner("peon-burrow", runner.clone());
    host.uninstall().expect("uninstall");

    let lines = runner.mutations().command_lines();
    assert!(lines[2].contains("/End"), "先结束：{lines:?}");
    assert!(lines[3].contains("/Delete"), "再删任务：{lines:?}");
    assert_eq!(runner.expectations_left(), 0, "预置的输出必须被用完");
}

#[test]
fn user_start_treats_already_running_as_success() {
    // `schtasks /Query` 报「正在运行」（本地化文本）→ 不该再发 /Run
    let runner =
        FakeRunner::new().expect("文件夹: \\peon-burrow\n任务名: peon-burrow\n状态: 正在运行");
    let host = WindowsUserHost::with_runner("peon-burrow", runner.clone());
    host.start().expect("已经在跑 = 成功");
    assert_eq!(runner.mutations().run_count(), 1, "只查了一次，没有再启动");
}

#[test]
fn user_stop_ignores_not_running() {
    // 幂等靠**先问状态**，不靠匹配 `schtasks` 的报错文案（那文案随系统语言变）。
    let runner = FakeRunner::new().expect("文件夹: \\peon-burrow\n任务名: peon-burrow\n状态: 就绪");
    let host = WindowsUserHost::with_runner("peon-burrow", runner.clone());
    host.stop().expect("没在跑时 stop 必须成功（幂等）");
    assert_eq!(
        runner.mutations().run_count(),
        1,
        "只查了状态，没有再发 /End（发了会拿到一条本地化的报错）"
    );
}

#[test]
fn user_stop_ends_a_running_task() {
    let runner = FakeRunner::new()
        .expect("状态: 正在运行") // 中文 Windows 的措辞
        .expect("成功: 计划任务已终止");
    let host = WindowsUserHost::with_runner("peon-burrow", runner.clone());
    host.stop().expect("stop");
    let lines = runner.mutations().command_lines();
    assert!(lines[1].contains("schtasks /End /TN"), "{lines:?}");
}
