//! 退出码与顶层错误。
//!
//! ⚠️ 退出码**只有这里一个来源**（约定 C2）：GUI 与脚本靠它区分「配置错」与「端口冲突」，
//! 别在别处 `process::exit(1)`。

use peon_burrow_core::RelayError;

/// 进程退出码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ExitCode {
    /// 正常结束。
    Ok = 0,
    /// 运行期失败。
    Runtime = 1,
    /// 配置错误（用户改配置就能好）。
    Config = 2,
    /// 端口被占用（**不换端口**，给诊断）。
    PortInUse = 3,
    /// 需要提权。
    NeedElevation = 4,
    /// 自更新完成，需要重启（由服务管理器拉起）。
    RestartRequested = 5,
}

impl ExitCode {
    /// 转成进程退出码。
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

/// 顶层错误。
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// 配置有问题。
    #[error("{0}")]
    Config(String),
    /// 端口被占用。
    #[error("端口 {port} 已被占用：{detail}")]
    PortInUse {
        /// 端口。
        port: u16,
        /// 占用者诊断。
        detail: String,
    },
    /// 需要提权。
    #[error("{0}")]
    NeedElevation(String),
    /// 运行期失败。
    #[error("{0}")]
    Runtime(String),
    /// 自更新失败。
    #[error("更新失败：{0}")]
    Update(String),
}

impl AppError {
    /// 对应的退出码。
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::Config(_) => ExitCode::Config,
            Self::PortInUse { .. } => ExitCode::PortInUse,
            Self::NeedElevation(_) => ExitCode::NeedElevation,
            Self::Update(_) => ExitCode::Runtime,
            Self::Runtime(_) => ExitCode::Runtime,
        }
    }
}

impl From<RelayError> for AppError {
    fn from(error: RelayError) -> Self {
        match error {
            RelayError::PortInUse { port } => Self::PortInUse {
                port,
                detail: "另一个进程正占着这个端口，运行 `burrow doctor` 看占用者诊断".to_owned(),
            },
            other => Self::Runtime(other.to_string()),
        }
    }
}

impl From<std::io::Error> for AppError {
    fn from(error: std::io::Error) -> Self {
        Self::Runtime(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_are_the_documented_numbers() {
        assert_eq!(ExitCode::Ok.as_u8(), 0);
        assert_eq!(ExitCode::Runtime.as_u8(), 1);
        assert_eq!(ExitCode::Config.as_u8(), 2);
        assert_eq!(ExitCode::PortInUse.as_u8(), 3);
        assert_eq!(ExitCode::NeedElevation.as_u8(), 4);
        assert_eq!(ExitCode::RestartRequested.as_u8(), 5);
    }

    #[test]
    fn errors_map_to_exit_codes() {
        assert_eq!(
            AppError::Config("x".to_owned()).exit_code(),
            ExitCode::Config
        );
        assert_eq!(
            AppError::PortInUse {
                port: 41316,
                detail: String::new()
            }
            .exit_code(),
            ExitCode::PortInUse
        );
        assert_eq!(
            AppError::NeedElevation("需要管理员".to_owned()).exit_code(),
            ExitCode::NeedElevation
        );
        assert_eq!(
            AppError::Runtime("boom".to_owned()).exit_code(),
            ExitCode::Runtime
        );
        assert_eq!(
            AppError::Update("boom".to_owned()).exit_code(),
            ExitCode::Runtime
        );
    }

    #[test]
    fn a_relay_port_conflict_becomes_the_port_in_use_exit_code() {
        let error: AppError = RelayError::PortInUse { port: 41316 }.into();
        assert_eq!(error.exit_code(), ExitCode::PortInUse);
        assert!(error.to_string().contains("41316"));

        let other: AppError = RelayError::Io(std::io::Error::other("nope")).into();
        assert_eq!(other.exit_code(), ExitCode::Runtime);
    }
}
