use super::*;

#[test]
fn task_xml_carries_hidden_logon_and_failure_restart() {
    let xml = task_xml(&sample_opts());
    assert!(
        xml.contains("<Hidden>true</Hidden>"),
        "用户级自启必须隐藏，否则登录时会闪黑框"
    );
    assert!(xml.contains("<LogonTrigger>"), "用户级默认是登录时启动");
    assert!(
        xml.contains("<RestartOnFailure>"),
        "失败重启策略必须写进任务 XML（sc create 那种漏写方式在这里被排除）"
    );
    assert!(xml.contains("<Interval>PT1M</Interval>"));
    assert!(xml.contains("<Count>3</Count>"));
    assert!(
        xml.contains("<RunLevel>LeastPrivilege</RunLevel>"),
        "用户级任务绝不能用最高权限跑"
    );
}

#[test]
fn task_xml_hides_the_window_and_keeps_arguments_separate() {
    let mut opts = InstallOptions::new("peon-burrow", r"C:\Program Files\peon & burrow\burrow.exe");
    opts.args = vec!["run".to_owned(), "--port".to_owned(), "41316".to_owned()];
    let xml = task_xml(&opts);

    // 路径在 XML 正文里必须先做 XML 转义（`&` → `&amp;`）
    assert!(xml.contains("peon &amp; burrow"), "XML 正文里的 & 必须转义");

    // ① `<Command>` 是可执行文件，参数独立放在 `<Arguments>`：
    //    塞在一起的话任务计划程序会拿整串当文件名去「打开」（弹「你要如何打开这个文件?」），
    //    而且任务永远起不来（真机踩过：界面显示「已安装」但 running 一直是 false）。
    assert!(
        xml.contains("<Command>") && xml.contains("powershell.exe</Command>"),
        "Command 要指向 powershell（负责隐藏窗口）：{xml}"
    );
    assert!(
        !xml.contains("burrow.exe&quot; run</Command>")
            && !xml.contains("burrow.exe run</Command>"),
        "参数不能留在 Command 里：{xml}"
    );

    // ② 真正的调用是隐藏窗口 + 等待子进程（任务状态才保持 Running）
    assert!(xml.contains("-WindowStyle Hidden"), "必须隐藏窗口：{xml}");
    assert!(
        xml.contains("-Command &quot;&amp; &#39;") || xml.contains("-Command &quot;&amp; '"),
        "用调用运算符（`&`）而不是 Start-Process，PowerShell 才会等子进程：{xml}"
    );
    assert!(
        xml.contains("'run' '--port' '41316'"),
        "参数要在 Arguments 里，而且各自加引号（带空格的值会被 PowerShell 拆成两个词）：{xml}"
    );

    // ③ 自检要能把中继路径读回来（界面上显示的那个路径）
    assert_eq!(
        inspect_command(&xml).as_deref(),
        Some(r"C:\Program Files\peon & burrow\burrow.exe"),
        "要从 Arguments 里读回真正的中继路径"
    );
}

#[test]
fn autostart_off_disables_the_logon_trigger() {
    let mut opts = sample_opts();
    opts.autostart = Autostart::Off;
    let xml = task_xml(&opts);
    assert!(xml.contains("<LogonTrigger>\n      <Enabled>false</Enabled>"));
    assert_eq!(inspect_autostart(&xml), Autostart::Off);
    assert_eq!(
        inspect_autostart(&task_xml(&sample_opts())),
        Autostart::Logon,
        "默认（登录自启）要能被读回成 Logon"
    );
}

#[test]
fn autostart_is_read_from_real_task_scheduler_xml() {
    // 实证（`Export-ScheduledTask` 导出我们装好的任务）：`<LogonTrigger>` 里**只有**
    // `<UserId>`，没有 `<Enabled>` —— 缺省即启用。只认 `Some(true)` 会把登录自启
    // 误报成「已关闭」，而这个值正是界面显示开关状态的依据。
    let xml = r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers>
<LogonTrigger>
  <UserId>DESKTOP-8I3HR5D\imba97</UserId>
</LogonTrigger>
  </Triggers>
  <Settings>
<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
  </Settings>
</Task>"#;
    assert_eq!(inspect_autostart(xml), Autostart::Logon);
}

#[test]
fn an_explicitly_disabled_logon_trigger_is_off() {
    // 用户手工在「任务计划程序」里禁用触发器时，XML **会**写出这一行
    let xml = "<Triggers><LogonTrigger><UserId>x</UserId><Enabled>false</Enabled></LogonTrigger></Triggers>";
    assert_eq!(inspect_autostart(xml), Autostart::Off);
}

#[test]
fn a_disabled_task_is_off_even_with_a_logon_trigger() {
    // 任务被整体停用（`schtasks /Change /DISABLE`）时，settings 里会有 Enabled=false
    let xml = "<Triggers><LogonTrigger><UserId>x</UserId></LogonTrigger></Triggers>\
               <Settings><Enabled>false</Enabled></Settings>";
    assert_eq!(inspect_autostart(xml), Autostart::Off);
}

#[test]
fn restart_policy_inspection_accepts_our_own_xml() {
    verify_task_xml(&task_xml_response(), "peon-burrow").expect("自己生成的 XML 必须通过自检");
}

#[test]
fn restart_policy_inspection_rejects_missing_or_zero_count() {
    let without = "<Task><Settings><Hidden>true</Hidden></Settings></Task>";
    let error =
        verify_task_xml(without, "peon-burrow").expect_err("没有 RestartOnFailure 就该报错");
    assert!(
        error.to_string().contains("RestartOnFailure"),
        "错误必须说清缺的是哪一项：{error}"
    );
    assert!(
        error.to_string().contains("peon-burrow"),
        "错误要点名是哪个任务：{error}"
    );
    assert!(
        error.action().contains("install"),
        "错误必须给出下一步动作：{}",
        error.action()
    );

    let zero = "<Task><Settings><RestartOnFailure><Interval>PT1M</Interval><Count>0</Count></RestartOnFailure></Settings></Task>";
    assert!(
        verify_task_xml(zero, "peon-burrow").is_err(),
        "Count=0 等于没有重启"
    );

    let broken = "<Task><Settings><RestartOnFailure><Interval>PT1M</Interval><Count>abc</Count></RestartOnFailure></Settings></Task>";
    assert!(
        verify_task_xml(broken, "peon-burrow").is_err(),
        "解析不了的 Count 要当作没配上"
    );
}

#[test]
fn inspecting_command_takes_the_binary_out_of_the_action() {
    // `Actions` 带属性（`Context="Author"`），解析要能跨过它 —— 真实 XML 就是这样。
    let response = task_xml(&sample_opts());
    assert!(
        response.contains(r#"<Actions Context="Author">"#),
        "夹具要覆盖带属性的开标签"
    );
    assert_eq!(
        inspect_command(&response).as_deref(),
        Some(r"C:\Users\me\AppData\Local\Programs\peon-burrow\burrow.exe"),
        "带引号的动作要能取出纯路径（GUI 要拿它显示 / 校验）"
    );
}

#[test]
fn status_reports_not_installed_without_a_task() {
    // `schtasks /Query` 对不存在的任务返回非零
    let runner = FakeRunner::new().expect_failure("错误: 系统找不到指定的文件。");
    let host = WindowsUserHost::with_runner("peon-burrow", runner);
    let status = host.status().expect("未安装不是错误");
    assert!(!status.installed);
    assert_eq!(status.name, "peon-burrow");
}

#[test]
fn task_name_uses_a_folder_for_user_level_only() {
    assert_eq!(
        task_name("relay", ServiceLevel::User),
        r"\peon-burrow\relay"
    );
    assert_eq!(task_name("relay", ServiceLevel::System), "relay");
}
