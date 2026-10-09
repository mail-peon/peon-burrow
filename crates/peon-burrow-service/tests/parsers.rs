//! 端口探测的**解析器**测试：拿真实输出（Windows 与 Unix 两种格式）当夹具。
//!
//! 为什么这些解析器要在集成测试里再测一遍：它们读的是**别的程序打印的文本**
//! （`netstat` / `lsof` / PowerShell），输出格式由系统决定，不由我们决定 ——
//! 所以夹具必须是「真抄来的字符串」，而且两种平台的格式都要能解析。
//! 这样即使只在 Windows 上开发，Unix 那条分支的解析也不会悄悄坏掉。

use peon_burrow_service::{
    PortStatus, parse_lsof_pids, parse_netstat_listeners, parse_powershell_pid,
};

/// 真实 `netstat -ano -p TCP` 的节选（中文系统：表头与「活动连接」是中文的，
/// 但列布局和状态词与英文系统一致）。
const NETSTAT_ZH: &str = "\
活动连接

  协议  本地地址          外部地址        状态           PID
  TCP    0.0.0.0:135            0.0.0.0:0              LISTENING       1244
  TCP    0.0.0.0:445            0.0.0.0:0              LISTENING       4
  TCP    127.0.0.1:41316        0.0.0.0:0              LISTENING       8899
  TCP    127.0.0.1:41316        127.0.0.1:53412        ESTABLISHED     8899
  TCP    127.0.0.1:41316        127.0.0.1:53412        TIME_WAIT       0
  TCP    [::]:41316             [::]:0                 LISTENING       8899
  TCP    [::1]:41317            [::]:0                 LISTENING       7777
  TCP    127.0.0.1:141316       0.0.0.0:0              LISTENING       6000
";

/// 同一台机器的英文表头版本。
const NETSTAT_EN: &str = "\
Active Connections

  Proto  Local Address          Foreign Address        State           PID
  TCP    127.0.0.1:41316        0.0.0.0:0              LISTENING       8899
  TCP    127.0.0.1:53412        127.0.0.1:41316        ESTABLISHED     8899
";

/// 真实 `lsof -nP -ti tcp:<port> -sTCP:LISTEN` 的输出（同一进程 v4 + v6 各一行）。
const LSOF: &str = "\
8899
8899
12044
";

#[test]
fn netstat_picks_the_listening_row_not_the_connections() {
    // 同一个端口既有 LISTENING 又有 ESTABLISHED / TIME_WAIT 行：
    // 占用者是监听那一行的 PID，而「连接数很多」不代表端口被更多人占着。
    assert_eq!(parse_netstat_listeners(NETSTAT_ZH, 41316), Ok(Some(8899)));
    assert_eq!(parse_netstat_listeners(NETSTAT_EN, 41316), Ok(Some(8899)));
}

#[test]
fn netstat_never_matches_a_port_suffix_or_a_remote_address() {
    // 端口必须**整段相等**：`1316` 不能被 `41316` 匹配，`4131` 也不行
    assert_eq!(parse_netstat_listeners(NETSTAT_ZH, 1316), Ok(None));
    assert_eq!(parse_netstat_listeners(NETSTAT_ZH, 4131), Ok(None));
    assert_eq!(parse_netstat_listeners(NETSTAT_ZH, 41315), Ok(None));
    assert_eq!(parse_netstat_listeners(NETSTAT_ZH, 41317), Ok(Some(7777)));

    // 53412 只作为**远端**端口出现（`41316` 的收尾连接）→ 不能被当成占用者
    assert_eq!(parse_netstat_listeners(NETSTAT_ZH, 53412), Ok(None));
    assert_eq!(parse_netstat_listeners(NETSTAT_EN, 53412), Ok(None));
}

#[test]
fn netstat_handles_ipv6_rows() {
    assert_eq!(parse_netstat_listeners(NETSTAT_ZH, 41317), Ok(Some(7777)));
    assert_eq!(
        parse_netstat_listeners(NETSTAT_ZH, 41316),
        Ok(Some(8899)),
        "[::]:41316 与 127.0.0.1:41316 是同一行服务的两条记录"
    );
}

#[test]
fn netstat_reports_a_malformed_pid_column_instead_of_guessing() {
    let broken = "  TCP    127.0.0.1:41316        0.0.0.0:0              LISTENING       ?\n";
    let error = parse_netstat_listeners(broken, 41316).expect_err("PID 列坏了要报错，不能猜");
    assert!(error.contains("PID"), "{error}");
}

#[test]
fn netstat_tolerates_header_only_and_empty_output() {
    assert_eq!(parse_netstat_listeners("", 41316), Ok(None));
    assert_eq!(parse_netstat_listeners("活动连接\n\n", 41316), Ok(None));
    // 只有表头（比如端口确实没人监听）
    assert_eq!(
        parse_netstat_listeners(
            "  协议  本地地址          外部地址        状态           PID\n",
            41316
        ),
        Ok(None)
    );
}

#[test]
fn lsof_output_is_a_pid_list() {
    assert_eq!(
        parse_lsof_pids(LSOF),
        vec![8899, 12044],
        "要去重，且按出现顺序"
    );
    assert_eq!(parse_lsof_pids(""), Vec::<u32>::new());
    // 真实世界里 lsof 的告警会混进输出
    assert_eq!(
        parse_lsof_pids("lsof: WARNING: can't stat() fuse\n8899\n"),
        vec![8899]
    );
}

#[test]
fn powershell_fallback_output_is_a_bare_pid() {
    assert_eq!(parse_powershell_pid("\r\n8899\r\n"), Some(8899));
    assert_eq!(parse_powershell_pid(""), None);
    // 没有匹配项时 PowerShell 打印的是 `$null`（空行），不是数字
    assert_eq!(
        parse_powershell_pid("Get-NetTCPConnection : 找不到任何匹配的对象"),
        None
    );
    // PID 0 是 Windows 的空闲进程，是合法值 —— 解析器不该把它当成「没有占用者」。
    assert_eq!(parse_powershell_pid("0"), Some(0));
}

#[test]
fn occupied_status_prints_pid_and_name_for_humans() {
    let status = PortStatus::occupied(Some(8899), Some("chrome.exe".to_owned()));
    assert_eq!(
        status.describe(41316),
        "端口 41316 已被 chrome.exe（PID 8899）占用"
    );

    // 查不到占用者时也要有话说，且不能暗示「端口是空的」
    let unknown = PortStatus::occupied(None, None);
    assert!(!unknown.free);
    assert!(unknown.describe(41316).contains("没查到占用者"));
}
