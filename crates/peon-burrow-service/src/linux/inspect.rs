use crate::host::{Autostart, ServiceError};

/// 解析 `systemctl show -p Restart -p RestartUSec` 的输出。
///
/// 真实输出（只有被请求的属性，顺序固定）：
///
/// ```text
/// Restart=always
/// RestartUSec=2s
/// ```
///
/// ⚠️ `Restart=` 的取值里 **`always` 与 `on-failure` 才算「失败会回来」**；
/// `no` 是 systemd 的默认值，也是最容易出现的「装完看着挺好、崩了不回来」。
pub fn inspect_restart_properties(stdout: &str) -> Result<(), ServiceError> {
    let mut restart: Option<&str> = None;
    let mut restart_usec: Option<&str> = None;
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("Restart=") {
            restart = Some(value.trim());
        } else if let Some(value) = line.strip_prefix("RestartUSec=") {
            restart_usec = Some(value.trim());
        }
    }

    match restart {
        Some("always") | Some("on-failure") | Some("on-abnormal") => Ok(()),
        Some(other) => Err(ServiceError::RestartPolicyMissing {
            name: "systemd unit".to_owned(),
            expected: "Restart=always（或 on-failure）+ RestartSec=2".to_owned(),
            found: format!(
                "Restart={other}{}",
                restart_usec
                    .map(|usec| format!("，RestartUSec={usec}"))
                    .unwrap_or_default()
            ),
        }),
        None => Err(ServiceError::RestartPolicyMissing {
            name: "systemd unit".to_owned(),
            expected: "Restart=always".to_owned(),
            found: "systemctl show 的输出里没有 Restart= 这一行".to_owned(),
        }),
    }
}

/// 解析 `systemctl cat`（或任何 unit 正文）里的 `Restart=`。
///
/// 作为 [`inspect_restart_properties`] 的退路：`systemctl show` 在某些版本上
/// 对 `--user` 单元返回空属性列表，那时直接看 unit 正文更可靠。
pub fn inspect_restart_line(text: &str) -> Result<(), ServiceError> {
    let found = text
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("Restart="))
        .map(|value| value.trim().to_owned());

    match found.as_deref() {
        Some("always") | Some("on-failure") | Some("on-abnormal") => Ok(()),
        other => Err(ServiceError::RestartPolicyMissing {
            name: "systemd unit".to_owned(),
            expected: "Restart=always".to_owned(),
            found: other
                .map(|value| format!("Restart={value}"))
                .unwrap_or_else(|| "unit 文件里没有 Restart= 这一行".to_owned()),
        }),
    }
}

/// 解析 `systemctl show -p ActiveState -p SubState -p UnitFileState` 的输出。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SystemdState {
    /// `ActiveState`（`active` = 在跑）。
    pub active_state: Option<String>,
    /// `SubState`（`running` / `dead` / `exited` / `failed`）。
    pub sub_state: Option<String>,
    /// `UnitFileState`（`enabled` = 开机自启）。
    pub unit_file_state: Option<String>,
}

impl SystemdState {
    /// 是否在运行。
    pub fn running(&self) -> bool {
        matches!(self.active_state.as_deref(), Some("active"))
    }

    /// 自启是否开着。
    pub fn autostart(&self) -> Autostart {
        match self.unit_file_state.as_deref() {
            Some("enabled") | Some("enabled-runtime") | Some("alias") => Autostart::Boot,
            _ => Autostart::Off,
        }
    }
}

/// 解析 `systemctl show` 的 `key=value` 行。
///
/// ```text
/// ActiveState=active
/// SubState=running
/// UnitFileState=enabled
/// ```
pub fn inspect_systemd_state(stdout: &str) -> SystemdState {
    let mut state = SystemdState::default();
    for line in stdout.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().to_owned();
        if value.is_empty() {
            continue;
        }
        match key {
            "ActiveState" => state.active_state = Some(value),
            "SubState" => state.sub_state = Some(value),
            "UnitFileState" => state.unit_file_state = Some(value),
            _ => {}
        }
    }
    state
}

/// 从 unit 正文里读 `ExecStart=` 的二进制路径（去掉前导 `-`）。
pub fn inspect_exec_start(unit_text: &str) -> Option<String> {
    let value = unit_text.lines().map(str::trim).find_map(|line| {
        line.strip_prefix("ExecStart=")
            .or_else(|| {
                line.strip_prefix('#')
                    .map(str::trim)
                    .and_then(|line| line.strip_prefix("ExecStart="))
            })
            .map(|value| value.trim())
    })?;
    let value = value.strip_prefix('-').unwrap_or(value);
    let token = if let Some(rest) = value.strip_prefix('"') {
        rest.split('"').next().unwrap_or("")
    } else {
        value.split_whitespace().next().unwrap_or("")
    };
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_owned())
}
