//! 端口探测：**唯一实现**。
//!
//! `run` / `doctor` / GUI 都调 [`probe_port`] —— 到处自己解析 `netstat` 是不允许的
//! （`service-lifecycle.md § 2` 的表格下方，以及 `port-and-discovery.md § 5.4`）。
//!
//! 判定顺序：
//!
//! 1. **试着绑 `127.0.0.1:<port>`**：绑得上就是空闲。这一步是跨平台一致的真话；
//! 2. 绑不上就问系统「谁占着」：Windows 解析 `netstat -ano -p TCP`（拿不到就退
//!    `Get-NetTCPConnection`），Unix 用 `lsof -ti tcp:<port> -sTCP:LISTEN`；
//! 3. 拿到的 PID 用 `sysinfo` 换成进程名（用户要看的不是 8899 而是 `chrome.exe`）。
//!
//! ⚠️ 这里**只探测**：不换端口、不杀进程（`adr-0003 § 5`）。
//! 「占用者是不是我们自己的旧实例」需要控制面握手，属于产品层知识，
//! 不在本函数里判断（`port-and-discovery.md § 5.4` 的 `is_self`）。

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener};

use crate::host::ServiceError;
use crate::runner::{CommandSpec, RealRunner, Runner};

/// 探测用的回环地址。
///
/// 只测 `127.0.0.1`：中继默认也只绑这里，
/// 而「`0.0.0.0` 绑不上但 `127.0.0.1` 能绑」这种差异对用户没有解释价值。
pub const PROBE_HOST: Ipv4Addr = Ipv4Addr::LOCALHOST;

/// 默认端口（`port-and-discovery.md § 3`：41316 = `4`-`M`(13)-`P`(16)）。
pub const DEFAULT_PORT: u16 = 41316;

/// 一次探测的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortStatus {
    /// 端口是否空闲（空闲 = 绑得上）。
    pub free: bool,
    /// 占用者 PID（拿不到时为 `None`：可能是权限不足，也可能是探测工具缺失）。
    pub owner_pid: Option<u32>,
    /// 占用者进程名（拿不到时为 `None`）。
    pub owner_name: Option<String>,
}

impl PortStatus {
    /// 空闲的样子。
    pub fn free() -> Self {
        Self {
            free: true,
            owner_pid: None,
            owner_name: None,
        }
    }

    /// 被占用的样子。
    pub fn occupied(pid: Option<u32>, name: Option<String>) -> Self {
        Self {
            free: false,
            owner_pid: pid,
            owner_name: name,
        }
    }

    /// 「三条出路」的文案（`port-and-discovery.md § 5.2`）。
    ///
    /// 服务态**不能**交互提问，所以文案必须自带可复制命令。
    pub fn advice(port: u16) -> String {
        format!(
            "1) 找出占用者： burrow doctor --port\n\
             2) 换一个端口： 改配置 relay.toml 的 port，然后重启服务\n\
             \x20  （注意：扩展里也要填同样的地址 ws://127.0.0.1:{port}/）\n\
             3) 前台临时跑： burrow run --port {}",
            port.wrapping_add(1)
        )
    }

    /// 一行式说明（日志用，`说人话`：不出现 `EADDRINUSE` 这类原生码）。
    pub fn describe(&self, port: u16) -> String {
        if self.free {
            return format!("端口 {port} 空闲");
        }
        match (&self.owner_pid, &self.owner_name) {
            (Some(pid), Some(name)) => format!("端口 {port} 已被 {name}（PID {pid}）占用"),
            (Some(pid), None) => format!("端口 {port} 已被 PID {pid} 占用"),
            _ => format!("端口 {port} 已被占用（没查到占用者，可能是权限不足）"),
        }
    }
}

/// 探测 `127.0.0.1:<port>` 是否空闲，并尽量查出占用者。
///
/// 这是端口探测的唯一实现：调用方不要自己解析 `netstat` / `lsof`。
/// 本函数**只读**（绑定失败即放弃，不做任何破坏性动作），测试可以放心跑。
pub fn probe_port(port: u16) -> Result<PortStatus, ServiceError> {
    probe_addr(IpAddr::V4(PROBE_HOST), port)
}

/// 探测指定回环地址上的端口。
///
/// 需要探 `[::1]`（中继的 `host` 允许写 `::1`，见 `port-and-discovery.md § 7`）时用它。
/// 默认的 [`probe_port`] 只探 `127.0.0.1` —— 那与中继默认的监听地址一致，
/// 也是扩展会去连的那一个。
pub fn probe_addr(host: IpAddr, port: u16) -> Result<PortStatus, ServiceError> {
    if bind_succeeds(host, port) {
        return Ok(PortStatus::free());
    }

    match find_owner(port) {
        Ok(pid) => {
            let name = pid.and_then(lookup_process_name);
            Ok(PortStatus::occupied(pid, name))
        }
        // 绑不上但连占用者都查不出来：这是真的「探测失败」，
        // 不能报成「空闲」（那会让调用方去 bind，然后拿到一个看不懂的原始错误）。
        Err(detail) => Err(ServiceError::ProbeFailed { port, detail }),
    }
}

/// 要探测的回环地址。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpAddr {
    /// `127.0.0.1`
    V4(Ipv4Addr),
    /// `[::1]`
    V6(std::net::Ipv6Addr),
}

impl IpAddr {
    /// 拼成可绑定的 socket 地址。
    pub fn socket(self, port: u16) -> SocketAddr {
        match self {
            Self::V4(addr) => SocketAddr::V4(SocketAddrV4::new(addr, port)),
            Self::V6(addr) => SocketAddr::V6(std::net::SocketAddrV6::new(addr, port, 0, 0)),
        }
    }
}

/// 试着绑一下；绑得上就立刻放开。
fn bind_succeeds(host: IpAddr, port: u16) -> bool {
    TcpListener::bind(host.socket(port)).is_ok()
}

/// 查出监听 `port` 的进程 PID。
///
/// 返回 `Ok(None)` = 「端口确实被占，但问不出 PID」（权限不足 / 工具缺失，都不是错误）；
/// 返回 `Err(..)` = 连「有没有被占」都判断不了。
#[cfg(windows)]
fn find_owner(port: u16) -> Result<Option<u32>, String> {
    let runner = RealRunner;

    // 首选 netstat：Windows 自带，且 `-ano` 会给出 PID。
    let netstat = CommandSpec::new(
        "netstat",
        ["-ano", "-p", "TCP"],
        "查询端口占用者（netstat）",
    );
    if let Ok(stdout) = runner.run(&netstat) {
        let found = parse_netstat_listeners(&stdout, port)?;
        if found.is_some() {
            return Ok(found);
        }
    }

    // 退路：netstat 被禁用 / 输出格式变了，或者占用来自内核驱动（那时 netstat 不给 PID）。
    let fallback = CommandSpec::new(
        "powershell",
        [
            "-NoProfile".to_owned(),
            "-NonInteractive".to_owned(),
            "-Command".to_owned(),
            format!(
                "(Get-NetTCPConnection -State Listen -LocalPort {port} \
                 -ErrorAction SilentlyContinue | Select-Object -First 1).OwningProcess"
            ),
        ],
        "查询端口占用者（Get-NetTCPConnection）",
    );
    // 这里**不再向上报错**：绑定已经失败，说明端口确实被占；
    // 「查不出占用者」不该让 doctor 整个报错（`describe` 会写「没查到占用者，可能是权限不足」）。
    let stdout = match runner.run(&fallback) {
        Ok(stdout) => stdout,
        Err(error) => {
            tracing::debug!(event = "service.probe.owner_unknown", %error, "查端口占用者失败");
            return Ok(None);
        }
    };
    Ok(parse_powershell_pid(&stdout))
}

/// Unix：`lsof -ti tcp:<port> -sTCP:LISTEN`。
///
/// `-t` 只输出 PID（`terse`），正好不需要解析表头 —— 而且 `lsof` 的表头在不同版本 / locale
/// 下会变，只认数字是唯一稳的做法。
#[cfg(unix)]
fn find_owner(port: u16) -> Result<Option<u32>, String> {
    let runner = RealRunner;
    let spec = CommandSpec::new(
        "lsof",
        [
            "-nP".to_owned(),
            "-ti".to_owned(),
            format!("tcp:{port}"),
            "-sTCP:LISTEN".to_owned(),
        ],
        "查询端口占用者（lsof）",
    );
    let stdout = runner.run(&spec).map_err(|error| error.to_string())?;
    Ok(parse_lsof_pids(&stdout).into_iter().next())
}

#[cfg(not(any(windows, unix)))]
fn find_owner(_port: u16) -> Result<Option<u32>, String> {
    Err("这个系统上既没有 netstat 也没有 lsof，无法查询占用者".to_owned())
}

/// 用 `sysinfo` 把 PID 换成进程名（`chrome.exe` 才是用户看得懂的东西）。
///
/// 查不到就返回 `None`：进程可能刚好退出了，或者当前用户没权限看它 ——
/// 这两种情况都不该让探测整体失败。
fn lookup_process_name(pid: u32) -> Option<String> {
    use sysinfo::{Pid, ProcessesToUpdate, System};

    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&[Pid::from_u32(pid)]), true);
    let process = system.process(Pid::from_u32(pid))?;
    let name = process.name().to_string_lossy().trim().to_owned();
    if name.is_empty() { None } else { Some(name) }
}

/// 解析 `netstat -ano -p TCP` 的输出，返回端口 `port` 的监听者 PID。
///
/// 真实 Windows 输出长这样（英文 / 中文系统的差异**只在表头和状态词**上，
/// 列布局与状态列的位置是一样的，所以这里不依赖任何本地化文本）：
///
/// ```text
/// 活动连接
///
///   协议  本地地址          外部地址        状态           PID
///   TCP    0.0.0.0:135            0.0.0.0:0              LISTENING       1244
///   TCP    127.0.0.1:41316        0.0.0.0:0              LISTENING       8899
///   TCP    127.0.0.1:41316        127.0.0.1:53412        ESTABLISHED     8899
///   TCP    [::]:41316             [::]:0                 LISTENING       8899
/// ```
///
/// 规则：
/// - 跳过表头（端口那一列解析不成数字）；
/// - 只认**处于监听状态**的行（`ESTABLISHED` / `TIME_WAIT` 的远端端口也可能是 41316）；
/// - 端口必须**整段相等**：`141316` 不能匹配 `41316`；
/// - `[::]` 的 IPv6 行同样算（同一进程通常会两行都出现，取第一个即可）。
pub fn parse_netstat_listeners(output: &str, port: u16) -> Result<Option<u32>, String> {
    for line in output.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 4 {
            continue;
        }
        // 列布局：协议 本地地址 外部地址 [状态] PID —— 带状态时 5 列，极少数版本没有状态列。
        let (state, pid_text) = if fields.len() >= 5 {
            (fields[fields.len() - 2], fields[fields.len() - 1])
        } else {
            (LISTEN_MARKER, fields[fields.len() - 1])
        };
        if !is_listening(state) {
            continue;
        }
        let Some(local_port) = port_of(fields[1]) else {
            continue;
        };
        if local_port != port {
            continue;
        }
        let pid = pid_text.parse::<u32>().map_err(|_| {
            format!("netstat 的 PID 列不是数字（读到的内容：{pid_text}），输出格式可能变了")
        })?;
        return Ok(Some(pid));
    }
    Ok(None)
}

/// netstat 永远不会打印的占位值：只在「输出里没有状态列」时用来跳过状态判断。
const LISTEN_MARKER: &str = "LISTENING";

/// 这一列算不算「正在监听」。
///
/// 不写死 `LISTENING`：中文系统上 netstat 会打印本地化状态词。
/// 反过来排除掉**明确不是监听**的状态，是唯一跨 locale 稳定的判据。
fn is_listening(state: &str) -> bool {
    let upper = state.to_ascii_uppercase();
    !matches!(
        upper.as_str(),
        "ESTABLISHED"
            | "TIME_WAIT"
            | "CLOSE_WAIT"
            | "SYN_SENT"
            | "SYN_RECEIVED"
            | "FIN_WAIT_1"
            | "FIN_WAIT_2"
            | "LAST_ACK"
            | "CLOSING"
            | "DELETE_TCB"
            | "BOUND"
    )
}

/// 从 `127.0.0.1:41316` / `[::]:41316` / `0.0.0.0:135` 里取出端口。
fn port_of(local: &str) -> Option<u16> {
    local.rsplit_once(':')?.1.parse().ok()
}

/// 解析 `powershell -Command "(Get-NetTCPConnection …).OwningProcess"` 的输出。
pub fn parse_powershell_pid(output: &str) -> Option<u32> {
    output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .and_then(|line| line.parse().ok())
}

/// 解析 `lsof -ti tcp:<port> -sTCP:LISTEN` 的输出。
///
/// 真实输出是每行一个 PID（同一进程监听 v4 + v6 时会重复，所以要去重）：
///
/// ```text
/// 8899
/// 8899
/// 12044
/// ```
///
/// lsof 在「没匹配到」时会以退出码 1 结束 —— 那种情况由 [`Runner`] 变成错误，
/// 不会走到这里；这里只管「有输出」的那一半。
pub fn parse_lsof_pids(output: &str) -> Vec<u32> {
    let mut pids = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(pid) = trimmed.parse::<u32>() else {
            continue;
        };
        if !pids.contains(&pid) {
            pids.push(pid);
        }
    }
    pids
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实 `netstat -ano -p TCP` 的节选（表头带中文，两行监听 + 一行已建立连接 + IPv6）。
    const NETSTAT_FIXTURE: &str = "\
活动连接

  协议  本地地址          外部地址        状态           PID
  TCP    0.0.0.0:135            0.0.0.0:0              LISTENING       1244
  TCP    0.0.0.0:445            0.0.0.0:0              LISTENING       4
  TCP    127.0.0.1:41316        0.0.0.0:0              LISTENING       8899
  TCP    127.0.0.1:41316        127.0.0.1:53412        ESTABLISHED     8899
  TCP    127.0.0.1:141316       0.0.0.0:0              LISTENING       6000
  TCP    [::]:41316             [::]:0                 LISTENING       8899
  TCP    [::1]:41317            [::]:0                 LISTENING       7777
";

    /// 英文系统同一段输出（表头不同、状态词相同）。
    const NETSTAT_FIXTURE_EN: &str = "\
Active Connections

  Proto  Local Address          Foreign Address        State           PID
  TCP    127.0.0.1:41316        0.0.0.0:0              LISTENING       8899
  TCP    127.0.0.1:53412        127.0.0.1:41316        TIME_WAIT       0
";

    #[test]
    fn netstat_finds_the_listening_pid() {
        assert_eq!(
            parse_netstat_listeners(NETSTAT_FIXTURE, 41316),
            Ok(Some(8899)),
            "同时有 LISTENING 与 ESTABLISHED 行时，只认监听那一行"
        );
    }

    #[test]
    fn netstat_ignores_the_remote_port_column() {
        // 53412 -> 41316 是**远端**端口（TIME_WAIT 的收尾连接）：
        // 解析器只看本地地址列，所以这里必须查不到占用者。
        assert_eq!(parse_netstat_listeners(NETSTAT_FIXTURE_EN, 53412), Ok(None));
        assert_eq!(
            parse_netstat_listeners(NETSTAT_FIXTURE, 53412),
            Ok(None),
            "ESTABLISHED 行同样不能当成占用者"
        );
    }

    #[test]
    fn netstat_does_not_treat_time_wait_as_a_listener() {
        // 端口刚被释放、连接还在 TIME_WAIT：这时端口其实是**能绑**的。
        // 把它当成占用者会让 doctor 报假警，所以本地端口命中但状态不是监听时必须跳过。
        let fixture = "  TCP    127.0.0.1:41316        127.0.0.1:60000        TIME_WAIT       0\n";
        assert_eq!(parse_netstat_listeners(fixture, 41316), Ok(None));
    }

    #[test]
    fn netstat_requires_an_exact_port_match() {
        // 141316 的末四位是 1316，绝不能被 41316 匹配上（这就是「整段相等」的意思）
        assert_eq!(parse_netstat_listeners(NETSTAT_FIXTURE, 1316), Ok(None));
        assert_eq!(parse_netstat_listeners(NETSTAT_FIXTURE, 14131), Ok(None));
    }

    #[test]
    fn netstat_handles_ipv6_and_the_localized_header() {
        assert_eq!(
            parse_netstat_listeners(NETSTAT_FIXTURE, 41317),
            Ok(Some(7777)),
            "[::1] 形式也要能解析端口"
        );
        assert_eq!(
            parse_netstat_listeners(NETSTAT_FIXTURE_EN, 41316),
            Ok(Some(8899)),
            "英文表头同样能解析（表头本来就该被跳过）"
        );
    }

    #[test]
    fn netstat_reports_a_broken_pid_column() {
        let broken = "  TCP    127.0.0.1:41316        0.0.0.0:0              LISTENING       abc\n";
        let error = parse_netstat_listeners(broken, 41316).expect_err("PID 列坏了就必须报错");
        assert!(error.contains("PID"), "错误要说清是哪一列坏了：{error}");
    }

    #[test]
    fn netstat_on_an_empty_output_is_not_an_error() {
        assert_eq!(parse_netstat_listeners("", 41316), Ok(None));
        assert_eq!(parse_netstat_listeners("活动连接\n\n", 41316), Ok(None));
    }

    #[test]
    fn powershell_pid_parsing_skips_blank_lines() {
        assert_eq!(parse_powershell_pid("\r\n  8899  \r\n"), Some(8899));
        assert_eq!(parse_powershell_pid(""), None);
        assert_eq!(
            parse_powershell_pid("Get-NetTCPConnection : 找不到匹配项"),
            None
        );
    }

    #[test]
    fn lsof_pids_are_deduplicated_in_order() {
        // 真实输出：同一进程监听 v4 与 v6 时会各打一行
        let fixture = "8899\n8899\n12044\n";
        assert_eq!(parse_lsof_pids(fixture), vec![8899, 12044]);
        assert_eq!(parse_lsof_pids(""), Vec::<u32>::new());
        assert_eq!(
            parse_lsof_pids("lsof: command not found\n8899\n"),
            vec![8899]
        );
    }

    #[test]
    fn probe_addresses_cover_both_loopbacks() {
        assert_eq!(
            IpAddr::V4(PROBE_HOST).socket(41316).to_string(),
            "127.0.0.1:41316"
        );
        // 中继允许 host = "::1"（port-and-discovery.md § 7），那种部署要用 probe_addr
        assert_eq!(
            IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
                .socket(41316)
                .to_string(),
            "[::1]:41316"
        );
        assert_ne!(
            IpAddr::V4(PROBE_HOST),
            IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
            "两条回环是不同的地址"
        );
    }

    #[test]
    fn advice_is_copy_pasteable() {
        let advice = PortStatus::advice(41316);
        assert!(advice.contains("burrow doctor --port"));
        assert!(advice.contains("relay.toml"));
        assert!(
            advice.contains("ws://127.0.0.1:41316/"),
            "扩展要填的地址必须原样给出：{advice}"
        );
    }

    #[test]
    fn describe_never_leaks_native_error_codes() {
        let occupied = PortStatus::occupied(Some(8899), Some("chrome.exe".to_owned()));
        assert_eq!(
            occupied.describe(41316),
            "端口 41316 已被 chrome.exe（PID 8899）占用"
        );
        assert!(!occupied.describe(41316).contains("EADDRINUSE"));
        assert_eq!(PortStatus::free().describe(41316), "端口 41316 空闲");
    }
}
