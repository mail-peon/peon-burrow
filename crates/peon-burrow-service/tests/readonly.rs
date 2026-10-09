//! 只读路径的集成测试：`probe_port()`。
//!
//! `modules.md § 13` 给服务层定的落点是「只读路径测试」：
//! 这个文件里**没有**任何 `install` / `uninstall` / `start` / `stop` 调用 ——
//! 那些动作的断言全部走假执行器（见 `src/windows.rs` / `src/macos.rs` / `src/linux.rs`
//! 里各自的 `#[cfg(test)]` 与 `tests/fixtures.rs`），一次都不动机器。

use std::net::TcpListener;

use peon_burrow_service::{DEFAULT_PORT, PROBE_HOST, PortStatus, probe_port};

/// 让内核分配一个空闲端口，然后立刻放开：这是「确定空闲」的唯一可靠办法。
fn a_port_that_is_free() -> u16 {
    let listener = TcpListener::bind((PROBE_HOST, 0)).expect("绑定临时端口");
    let port = listener.local_addr().expect("取本地地址").port();
    drop(listener);
    port
}

#[test]
fn a_free_port_reports_free() {
    let port = a_port_that_is_free();
    let status = probe_port(port).expect("探测空闲端口不该失败");

    assert!(status.free, "{port} 刚被放开，应当是空闲的：{status:?}");
    assert_eq!(status.owner_pid, None);
    assert_eq!(status.owner_name, None);
    assert_eq!(status.describe(port), format!("端口 {port} 空闲"));
}

#[test]
fn a_port_we_are_listening_on_reports_ourselves() {
    let listener = TcpListener::bind((PROBE_HOST, 0)).expect("绑定临时端口");
    let port = listener.local_addr().expect("取本地地址").port();
    let our_pid = std::process::id();

    let status = probe_port(port).expect("探测被自己占用的端口不该失败");

    assert!(!status.free, "{port} 正在被本进程监听：{status:?}");
    assert_eq!(
        status.owner_pid,
        Some(our_pid),
        "占用者必须就是我们自己的 PID（这也是「本中继 / 别进程」判定的基础）"
    );
    // 进程名要能拿到（拿不到也允许：`sysinfo` 在部分权限下读不到别的进程，
    // 但对自己的进程不该读不到）。
    let name = status.owner_name.clone().expect("至少要能读到自己的进程名");
    assert!(!name.trim().is_empty(), "进程名不该是空白");

    let described = status.describe(port);
    assert!(
        described.contains(&our_pid.to_string()),
        "文案里要有 PID：{described}"
    );
    assert!(
        !described.contains("EADDRINUSE"),
        "面向用户的文案不许出现原生错误码：{described}"
    );
}

#[test]
fn the_default_port_is_the_documented_one() {
    // port-and-discovery.md § 3：41316 = 4-M(13)-P(16)
    assert_eq!(DEFAULT_PORT, 41316);
    assert_eq!(PROBE_HOST.to_string(), "127.0.0.1");
}

#[test]
fn probe_never_claims_a_port_is_free_when_it_is_not() {
    // 两个不同端口：一个被占、一个空闲，判定不能互相串
    let busy = TcpListener::bind((PROBE_HOST, 0)).expect("绑定忙端口");
    let busy_port = busy.local_addr().expect("取地址").port();
    let free_port = a_port_that_is_free();

    assert!(!probe_port(busy_port).expect("探测").free);
    assert!(probe_port(free_port).expect("探测").free);
    drop(busy);
}

#[test]
fn occupied_status_carries_the_advice_text() {
    // 服务态不能交互提问，所以文案必须自带三条出路（port-and-discovery.md § 5.2）
    let advice = PortStatus::advice(DEFAULT_PORT);
    assert!(advice.contains("burrow doctor --port"));
    assert!(advice.contains("relay.toml"));
    assert!(advice.contains("ws://127.0.0.1:41316/"));
    assert!(advice.contains("burrow run --port 41317"));
}
