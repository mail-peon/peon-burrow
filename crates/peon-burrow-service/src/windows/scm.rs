use super::SYSTEM_RESTART_DELAY_MS;
use super::task::parse_windows_program;
use crate::host::{Autostart, ServiceError};

/// `sc query` 的输出里是否显示正在运行。
///
/// `sc` 的字段名与状态词**不做本地化**（`STATE : 4 RUNNING`），所以这里只认 ASCII。
pub(super) fn service_is_running(stdout: &str) -> bool {
    stdout.to_ascii_uppercase().contains("RUNNING")
}

/// `sc query` 的输出里是不是**已经停了**（`STATE : 1  STOPPED`）。
///
/// ⚠️ 判据是「出现 `STOPPED`」而**不是**「没有 `RUNNING`」：`START_PENDING` /
/// `STOP_PENDING` / `PAUSED` 期间进程都还活着，把它们当成「停好了」会让卸载带着
/// 一个活进程去 `sc delete`。
pub(super) fn service_is_stopped(stdout: &str) -> bool {
    stdout.to_ascii_uppercase().contains("STOPPED")
}

/// 从 `sc query` 的输出里读 `WIN32_EXIT_CODE`（服务进程上次的退出码）。
///
/// 真机形状：`WIN32_EXIT_CODE    : 0  (0x0)`（字段名不本地化，冒号后有十进制 + 十六进制）。
/// 读不出来返回 `None`：**不要**谎报 0 —— 界面会把它当成「上次是干净退出的」。
pub(super) fn inspect_sc_exit_code(stdout: &str) -> Option<i32> {
    for line in stdout.lines() {
        if !line.to_ascii_uppercase().contains("WIN32_EXIT_CODE") {
            continue;
        }
        let (_, value) = line.split_once(':')?;
        return value.split_whitespace().next()?.parse::<i32>().ok();
    }
    None
}

/// 失败重启策略（从注册表的 `FailureActions` 里解出来的）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureActions {
    /// 多久没有失败就把失败计数清零（秒）。
    pub reset_secs: u32,
    /// 每个动作：`(类型, 延迟毫秒)`；类型 `1` = 重新启动（`SC_ACTION_RESTART`）。
    pub actions: Vec<(u32, u32)>,
}

impl FailureActions {
    /// 是否至少有一个「重新启动」动作（`RUN` 是跑命令、`REBOOT` 是重启机器，都不算）。
    pub fn restarts(&self) -> bool {
        self.actions.iter().any(|(kind, _)| *kind == 1)
    }

    /// 第一个「重新启动」动作的延迟。
    pub fn first_restart_delay_ms(&self) -> Option<u32> {
        self.actions
            .iter()
            .find(|(kind, _)| *kind == 1)
            .map(|(_, delay)| *delay)
    }

    /// 一行描述（**不含任何本地化文字**，可直接进错误文案）。
    pub fn describe(&self) -> String {
        let actions = self
            .actions
            .iter()
            .map(|(kind, delay)| format!("{}/{}ms", action_name(*kind), delay))
            .collect::<Vec<_>>()
            .join(", ");
        format!("reset={}s, actions=[{actions}]", self.reset_secs)
    }
}

/// 动作类型 → 名字（与 SCM 的 `SC_ACTION_TYPE` 一致）。
fn action_name(kind: u32) -> &'static str {
    match kind {
        0 => "none",
        1 => "restart",
        2 => "reboot",
        3 => "run",
        _ => "unknown",
    }
}

/// 从 `reg query … /v FailureActions` 的输出里解出失败策略。
///
/// ⚠️ 为什么不用 `sc qfailure` 的文字：它的输出**会随系统语言变**。真机上出现过
/// `[SC] QueryServiceConfig2 成功`（同一台机器另一次跑是 `SUCCESS`），动作行也可能被本地化，
/// 于是「找 `RESTART` 子串」把已配好的策略误判成「没有策略」，安装直接中止。
/// 顺带修掉另一个更隐蔽的问题：真机的动作行是 `RESTART -- Delay = 5000 milliseconds.`，
/// 而解析只认 `DELAY:` → 延迟永远是 `None` → 「≤5 秒」那条约束**从来没有被真正校验过**。
///
/// 注册表值的布局（实测 44 字节 / 3 个动作）：
/// ```text
/// 0x00 reset_secs (u32)   0x04 reboot 偏移   0x08 command 偏移
/// 0x0C 动作数             0x10 动作数组偏移（实测 20）
/// 0x14 起：每个动作 8 字节 = (类型 u32, 延迟毫秒 u32)
/// ```
pub fn parse_failure_actions(stdout: &str) -> Option<FailureActions> {
    let hex = stdout
        .lines()
        .find(|line| line.contains("REG_BINARY"))
        .and_then(|line| line.split_whitespace().last())?;
    let bytes = decode_hex(hex)?;

    if bytes.len() < 20 {
        return None;
    }
    let reset_secs = u32::from_le_bytes(bytes.get(0..4)?.try_into().ok()?);
    let count = u32::from_le_bytes(bytes.get(12..16)?.try_into().ok()?) as usize;
    let start = u32::from_le_bytes(bytes.get(16..20)?.try_into().ok()?) as usize;

    let mut actions = Vec::with_capacity(count);
    for index in 0..count {
        let offset = start.checked_add(index.checked_mul(8)?)?;
        let kind = u32::from_le_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?);
        let delay = u32::from_le_bytes(bytes.get(offset + 4..offset + 8)?.try_into().ok()?);
        actions.push((kind, delay));
    }

    Some(FailureActions {
        reset_secs,
        actions,
    })
}

/// 十六进制串 → 字节（奇数长度或非法字符一律 `None`）。
fn decode_hex(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if text.is_empty() || !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(text.get(index..index + 2)?, 16).ok())
        .collect()
}

/// 校验读到的失败策略。
///
/// 判定（`service_name` 只进错误文案，让用户知道是哪个服务）：
/// 1. 必须有一个 `restart` 动作（`run` 是「跑命令」，不算失败重启策略）；
/// 2. 重启延迟必须 ≤ 5 秒：`service-lifecycle.md § 5` 要求 5 秒内起来，
///    否则扩展侧的 watch 重连（1s/2s/5s…）会在用户察觉前还没恢复。
pub fn verify_failure_actions(stdout: &str, service_name: &str) -> Result<(), ServiceError> {
    let expected = format!("失败后重新启动（restart/{SYSTEM_RESTART_DELAY_MS}）");

    let Some(actions) = parse_failure_actions(stdout) else {
        return Err(ServiceError::RestartPolicyMissing {
            name: service_name.to_owned(),
            expected,
            found: first_interesting_line(stdout),
        });
    };

    if !actions.restarts() {
        return Err(ServiceError::RestartPolicyMissing {
            name: service_name.to_owned(),
            expected,
            found: actions.describe(),
        });
    }

    match actions.first_restart_delay_ms() {
        Some(delay) if u64::from(delay) > SYSTEM_RESTART_DELAY_MS => {
            Err(ServiceError::RestartPolicyMissing {
                name: service_name.to_owned(),
                expected: format!("重启延迟不超过 {SYSTEM_RESTART_DELAY_MS} ms"),
                found: actions.describe(),
            })
        }
        Some(_) => Ok(()),
        None => Err(ServiceError::RestartPolicyMissing {
            name: service_name.to_owned(),
            expected,
            found: actions.describe(),
        }),
    }
}

/// 从 `sc query` 的输出里读启动类型。
pub fn inspect_sc_autostart(stdout: &str) -> Autostart {
    // `sc query` 不打印启动类型，`sc qc` 才打印。这里的输入可能是 `sc qc` 的输出：
    // START_TYPE : 2   AUTO_START  (DELAYED)
    let upper = stdout.to_ascii_uppercase();
    if upper.contains("AUTO_START") {
        Autostart::Boot
    } else if upper.contains("DEMAND_START") {
        Autostart::Off
    } else {
        // 读不出来时不要谎报「开着」
        Autostart::Off
    }
}

/// 从 `sc qc` 的输出里读 `BINARY_PATH_NAME`（**只取路径，不取参数**）。
pub fn inspect_sc_binary_path(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        let upper = line.to_ascii_uppercase();
        if !upper.contains("BINARY_PATH_NAME") {
            continue;
        }
        let (_, value) = line.split_once(':')?;
        return parse_windows_program(value);
    }
    None
}

/// `sc qfailure` 里第一行有信息的内容（没有失败策略时给用户看它）。
fn first_interesting_line(stdout: &str) -> String {
    stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.to_ascii_uppercase().contains("SUCCESS"))
        .unwrap_or("（输出为空）")
        .to_owned()
}
