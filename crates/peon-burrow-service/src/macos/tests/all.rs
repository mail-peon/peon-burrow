use super::*;
use crate::runner::FakeRunner;

fn sample_opts() -> InstallOptions {
    let mut opts = InstallOptions::new(
        "cn.example.relay",
        "/Users/me/.local/share/peon-burrow/bin/burrow",
    );
    opts.args = vec!["run".to_owned()];
    opts
}

/// `launchctl print gui/501/cn.example.relay` 的真实形状（节选）。
const PRINT_RUNNING: &str = "\
gui/501/cn.example.relay = {
\tactive count = 1
\tpath = /Users/me/Library/LaunchAgents/cn.example.relay.plist
\ttype = LaunchAgent
\tstate = running

\tprogram = /Users/me/.local/share/peon-burrow/bin/burrow
\targuments = {
\t\t/Users/me/.local/share/peon-burrow/bin/burrow
\t\trun
\t}
\tpid = 4242
}
";

const PRINT_LOADED_IDLE: &str = "\
gui/501/cn.example.relay = {
\tactive count = 0
\tpath = /Users/me/Library/LaunchAgents/cn.example.relay.plist
\tstate = exited
}
";

#[test]
fn plist_carries_keep_alive_run_at_load_and_arguments() {
    let plist = plist_body(&sample_opts());
    assert!(
        plist.contains("<key>KeepAlive</key>"),
        "崩溃恢复靠 KeepAlive"
    );
    assert!(
        plist.contains("<key>SuccessfulExit</key>\n        <false/>"),
        "只对非正常退出生效 —— 自更新才能靠它重启"
    );
    assert!(plist.contains("<key>RunAtLoad</key>\n    <true/>"));
    assert!(plist.contains("<string>/Users/me/.local/share/peon-burrow/bin/burrow</string>"));
    assert!(plist.contains("<string>run</string>"));
    assert!(plist.contains("<key>Label</key>\n    <string>cn.example.relay</string>"));
}

#[test]
fn plist_autostart_off_flips_run_at_load_only() {
    let mut opts = sample_opts();
    opts.autostart = Autostart::Off;
    let plist = plist_body(&opts);
    assert!(plist.contains("<key>RunAtLoad</key>\n    <false/>"));
    assert_eq!(inspect_run_at_load(&plist), Some(false));
    assert!(inspect_keep_alive(&plist), "自启关掉不影响 KeepAlive");
}

#[test]
fn plist_escapes_paths_with_ampersands() {
    let opts = InstallOptions::new("relay", "/Users/a & b/bin/burrow");
    let plist = plist_body(&opts);
    assert!(
        plist.contains("/Users/a &amp; b/bin/burrow"),
        "XML 必须转义"
    );
    assert_eq!(
        inspect_binary_path(&plist).as_deref(),
        Some("/Users/a &amp; b/bin/burrow"),
        "（读回来的是转义后的文本，调用方需要时自己反转义）"
    );
}

#[test]
fn keep_alive_inspection_accepts_dict_and_true_but_not_false() {
    assert!(inspect_keep_alive(
        "<key>KeepAlive</key>\n<dict><key>SuccessfulExit</key><false/></dict>"
    ));
    assert!(inspect_keep_alive("<key>KeepAlive</key>\n<true/>"));
    assert!(!inspect_keep_alive("<key>KeepAlive</key>\n<false/>"));
    assert!(!inspect_keep_alive("<plist></plist>"));
}

#[test]
fn with_run_at_load_rewrites_only_that_value() {
    let plist = plist_body(&sample_opts());
    let off = with_run_at_load(&plist, false).expect("有 RunAtLoad 就能改写");
    assert!(off.contains("<key>RunAtLoad</key>\n    <false/>"));
    assert!(
        off.contains("<key>EnvironmentVariables</key>")
            == plist.contains("<key>EnvironmentVariables</key>"),
        "其余内容必须原样保留"
    );
    // ⚠️ 长度**会**差 1：`<true/>` 是 7 个字符、`<false/>` 是 8 个 —— 旧断言写的是
    // 「true/false 同长，长度不该变」（错），而这条测试只在 macOS 上编译，
    // 于是它一直红着、没人看见。这里改成逐字节核对「只改了 RunAtLoad 的那一处值」。
    //
    // ⚠️ 值的**起点**要跳过换行与缩进（`locate_plist_scalar` 就是这么找的）：
    // 上一版直接拿 `<key>RunAtLoad</key>` 的末尾去比，撞在 `\n    ` 上又误报了一次。
    let marker = "<key>RunAtLoad</key>";
    let value_start = |text: &str| {
        let after = text.find(marker).expect("有 RunAtLoad") + marker.len();
        after + (text[after..].len() - text[after..].trim_start().len())
    };
    let (head, tail) = (value_start(&plist), value_start(&off));
    assert!(
        plist[head..].starts_with("<true/>"),
        "样本默认是开着自启的，实际从 {:?} 开始",
        plist[head..].chars().take(16).collect::<String>()
    );
    assert_eq!(
        (&plist[..head], &plist[head + "<true/>".len()..]),
        (&off[..tail], &off[tail + "<false/>".len()..]),
        "除了 RunAtLoad 的值，其余必须逐字节不变"
    );
    assert_eq!(
        off.len(),
        plist.len() + 1,
        "`<true/>` → `<false/>` 正好长一个字符"
    );
    assert!(inspect_keep_alive(&off));

    // 用户手改过的 plist（额外字段）不能被抹掉
    let custom = plist.replace(
        "<key>ThrottleInterval</key>",
        "<key>StandardOutPath</key>\n    <string>/tmp/x.log</string>\n    <key>ThrottleInterval</key>",
    );
    let rewritten = with_run_at_load(&custom, false).expect("改写");
    assert!(rewritten.contains("/tmp/x.log"), "用户加的行必须还在");
}

#[test]
fn launchd_output_is_parsed_into_status() {
    let running = inspect_launchd(PRINT_RUNNING);
    assert!(running.loaded);
    assert!(running.running);
    assert_eq!(running.pid, Some(4242));

    let idle = inspect_launchd(PRINT_LOADED_IDLE);
    assert!(idle.loaded, "已加载 = 已安装");
    assert!(!idle.running, "「已安装·未运行」是正常状态");
    assert_eq!(idle.pid, None);
}

#[test]
fn install_writes_plist_then_bootstraps_and_kickstarts() {
    let runner = FakeRunner::new()
        .expect("") // bootstrap
        .expect("") // kickstart
        .expect(plist_body(&sample_opts())); // 自检：cat plist

    let host = MacOsHost::with_runner("cn.example.relay", ServiceLevel::User, runner.clone());
    host.install(&sample_opts()).expect("install");

    let mutations = runner.mutations();
    assert_eq!(mutations.run_count(), 3);
    assert_eq!(mutations.files().len(), 1, "plist 必须先落盘");
    let lines = mutations.command_lines();
    assert!(
        lines[0].starts_with("launchctl bootstrap gui/"),
        "{}",
        lines[0]
    );
    assert!(
        lines[0].ends_with(".plist"),
        "bootstrap 要带上 plist 路径：{}",
        lines[0]
    );
    assert!(
        lines[1].starts_with("launchctl kickstart gui/"),
        "{}",
        lines[1]
    );
    assert!(
        lines[2].starts_with("cat "),
        "自检要读 plist 正文：{}",
        lines[2]
    );
    assert_eq!(runner.expectations_left(), 0);
}

#[test]
fn install_fails_when_plist_has_no_keep_alive() {
    let mut plist = plist_body(&sample_opts());
    plist = plist.replace("<key>KeepAlive</key>", "<key>SomethingElse</key>");
    let runner = FakeRunner::new().expect("").expect("").expect(&plist);
    let host = MacOsHost::with_runner("cn.example.relay", ServiceLevel::User, runner);
    let error = host
        .install(&sample_opts())
        .expect_err("没有 KeepAlive 必须报错");
    assert!(matches!(error, ServiceError::RestartPolicyMissing { .. }));
    assert!(error.to_string().contains("KeepAlive"));
}

#[test]
fn system_level_uses_launch_daemons_and_the_system_domain() {
    let runner = FakeRunner::new().expect("").expect("").expect("<plist/>");
    let host = MacOsHost::with_runner("cn.example.relay", ServiceLevel::System, runner.clone());
    let mut opts = sample_opts();
    opts.level = ServiceLevel::System;
    opts.autostart = Autostart::Boot;
    // 自检会因为没有 KeepAlive 失败，但我们要先看命令与路径
    let _ = host.install(&opts);

    let mutations = runner.mutations();
    match mutations.files().first() {
        Some(crate::runner::Mutation::Install(file)) => {
            assert_eq!(
                file.path.display().to_string(),
                "/Library/LaunchDaemons/cn.example.relay.plist"
            );
        }
        other => panic!("第 1 个动作应当是写 plist：{other:?}"),
    }
    let lines = mutations.command_lines();
    assert!(
        lines[0].starts_with("launchctl bootstrap system "),
        "{}",
        lines[0]
    );
    assert!(
        lines[1].starts_with("launchctl kickstart system/"),
        "{}",
        lines[1]
    );
}

#[test]
fn user_level_requires_home() {
    // 不设 $HOME 时应当明确报错，而不是写到一个猜出来的路径
    let runner = FakeRunner::new();
    let host = MacOsHost::with_runner("relay", ServiceLevel::User, runner);
    if std::env::var_os("HOME").is_none() {
        let error = host
            .install(&sample_opts())
            .expect_err("没有 $HOME 必须报错");
        assert!(matches!(error, ServiceError::InvalidOptions { .. }));
    }
}

#[test]
fn stop_is_idempotent_when_not_loaded() {
    let runner = FakeRunner::new().expect_failure("Could not find service \"relay\" in domain");
    let host = MacOsHost::with_runner("relay", ServiceLevel::User, runner);
    host.stop().expect("没加载时 stop 应当直接成功");
}

#[test]
fn restart_policy_description_mentions_the_strategy() {
    assert!(restart_policy_description().contains("KeepAlive"));
}
