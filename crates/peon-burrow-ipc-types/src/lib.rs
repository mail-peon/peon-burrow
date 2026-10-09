//! 控制面类型：GUI / 脚本与**运行中的中继**之间的契约。
//!
//! 只依赖 `serde` / `serde_json` —— 桌面端想自己实现客户端时，只加这一个 crate 就够，
//! 不会被拖进传输依赖（布局铁律 L2）。
//!
//! ⚠️ 改这里 = 改协议：必须同时更新 `ai-docs/design/control-plane-ipc.md`，
//! 并考虑升 [`IPC_PROTOCOL_VERSION`]。命令集是**白名单**：枚举即全部能力。

use std::pin::Pin;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 控制面协议版本（**与扩展侧的 `WATCH_PROTOCOL_VERSION` 分开命名**）。
pub const IPC_PROTOCOL_VERSION: u16 = 1;

/// 单行长度上限：超了直接断开（防止一个本机进程把内存喂爆）。
pub const MAX_LINE_BYTES: usize = 8 * 1024;

/// 一条请求（客户端 → 服务）。
///
/// 变体就是**全部能力**，没有「读凭据」「改监听地址」这类东西（见设计文档 § 5）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "camelCase")]
pub enum Request {
    /// 存活探测（比 `status` 轻）。
    Ping,
    /// 进程状态 + 服务注册状态。
    Status,
    /// 版本与构建信息。
    Version,
    /// 自检结果。
    Doctor {
        /// 是否要详细输出。
        #[serde(default)]
        verbose: bool,
    },
    /// 请求服务**自己**停下来。
    Stop {
        /// 停下来的原因（写进日志）。
        #[serde(default)]
        reason: Option<String>,
    },
    /// 请求服务重启自己。
    Restart,
    /// 检查更新（`force` 忽略节流）。
    UpdateCheck {
        /// 忽略检查节流。
        #[serde(default)]
        force: bool,
    },
    /// 应用更新（成功后服务会以「需要重启」的退出码结束）。
    UpdateApply,
    /// 临时打开明文日志（含凭据！），到点自动关。
    TraceOn {
        /// 持续秒数（上限由服务侧裁剪）。
        seconds: u32,
    },
    /// 关掉明文日志。
    TraceOff,
}

impl Request {
    /// 命令名（日志与错误文案里用）。
    pub fn command(&self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::Status => "status",
            Self::Version => "version",
            Self::Doctor { .. } => "doctor",
            Self::Stop { .. } => "stop",
            Self::Restart => "restart",
            Self::UpdateCheck { .. } => "updateCheck",
            Self::UpdateApply => "updateApply",
            Self::TraceOn { .. } => "traceOn",
            Self::TraceOff => "traceOff",
        }
    }
}

/// 客户端发来的一行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientFrame {
    /// 协议版本。
    pub v: u16,
    /// 客户端自增 id，原样回显（日志关联用）。
    pub id: String,
    /// 鉴权 token。
    #[serde(default)]
    pub token: String,
    /// 具体命令。
    #[serde(flatten)]
    pub request: Request,
}

impl ClientFrame {
    /// 构造一条请求。
    pub fn new(id: impl Into<String>, token: impl Into<String>, request: Request) -> Self {
        Self {
            v: IPC_PROTOCOL_VERSION,
            id: id.into(),
            token: token.into(),
            request,
        }
    }
}

/// 服务回的一行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerFrame {
    /// 协议版本。
    pub v: u16,
    /// 回显的请求 id。
    pub id: String,
    /// 是否成功。
    pub ok: bool,
    /// 成功时的结果。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// 失败时的错误。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<IpcError>,
}

impl ServerFrame {
    /// 成功。
    pub fn ok(id: impl Into<String>, result: Value) -> Self {
        Self {
            v: IPC_PROTOCOL_VERSION,
            id: id.into(),
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    /// 失败。
    pub fn err(id: impl Into<String>, error: IpcError) -> Self {
        Self {
            v: IPC_PROTOCOL_VERSION,
            id: id.into(),
            ok: false,
            result: None,
            error: Some(error),
        }
    }

    /// 取结果，失败则返回错误。
    pub fn into_result(self) -> Result<Value, IpcError> {
        if self.ok {
            Ok(self.result.unwrap_or(Value::Null))
        } else {
            Err(self
                .error
                .unwrap_or_else(|| IpcError::new(IpcErrorCode::Internal, "未知错误")))
        }
    }
}

/// 错误码（GUI 据此决定文案与动作，不要去匹配 message）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum IpcErrorCode {
    /// token 不匹配（或没带）。
    Unauthorized,
    /// 命令不认识（协议不匹配）。
    UnknownCommand,
    /// 客户端协议版本比服务新。
    ProtocolTooNew,
    /// 太长了。
    LineTooLong,
    /// 正忙（例如正在更新）。
    Busy,
    /// 其它内部错误。
    Internal,
}

/// 一条错误。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcError {
    /// 错误码。
    pub code: IpcErrorCode,
    /// 给人看的说明。
    pub message: String,
}

impl IpcError {
    /// 构造。
    pub fn new(code: IpcErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// `status` 的结果：**两个权威来源**拼出来的。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReport {
    /// 进程内真实状态（来自引擎）。
    pub process: ProcessStatus,
    /// 服务注册状态（来自服务管理器）。
    pub service: ServiceStatus,
}

/// 进程内状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessStatus {
    /// 是否在监听。
    pub running: bool,
    /// 监听地址。
    pub host: String,
    /// 监听端口。
    pub port: u16,
    /// 版本。
    pub version: String,
    /// 控制面协议版本。
    pub protocol: u16,
    /// 启动时间（RFC 3339）。
    pub started_at: String,
    /// 当前连接数。
    pub connections: usize,
    /// 其中 watch 连接数。
    pub watch_connections: usize,
    /// 最近一次错误。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// 服务级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceLevel {
    /// 用户级（零提权，默认）。
    User,
    /// 系统级（装的时候要一次提权）。
    System,
}

/// 自启方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Autostart {
    /// 登录时启动。
    Logon,
    /// 开机启动（需要系统级）。
    Boot,
    /// 不开自启。
    Off,
}

/// 服务注册状态（CLI `service status --json`、控制面 `status`、`doctor` 三处共用）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceStatus {
    /// 是否已注册。
    pub installed: bool,
    /// 是否在运行。
    pub running: bool,
    /// 安装级别（未安装时为 `None`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<ServiceLevel>,
    /// 自启方式（未安装时为 `None`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autostart: Option<Autostart>,
    /// 服务名。
    pub name: String,
    /// 注册的二进制路径。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
    /// 操作是否需要提权。
    pub requires_elevation: bool,
    /// **失败重启策略是否真的写进去了** —— 「服务崩了能不能自己回来」的唯一信号。
    pub restart_policy_configured: bool,
    /// 上次退出码。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_exit_code: Option<i32>,
}

impl ServiceStatus {
    /// 未安装的样子。
    pub fn not_installed(name: impl Into<String>) -> Self {
        Self {
            installed: false,
            running: false,
            level: None,
            autostart: None,
            name: name.into(),
            binary_path: None,
            requires_elevation: false,
            restart_policy_configured: false,
            last_exit_code: None,
        }
    }
}

/// `doctor` 的单条检查结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckResult {
    /// 稳定的检查 id（GUI 与文档按它对照）。
    pub id: String,
    /// 结论等级。
    pub level: CheckLevel,
    /// 一行标题。
    pub title: String,
    /// 细节（可为空）。
    #[serde(default)]
    pub detail: String,
    /// 建议动作（可为空）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

/// 检查结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckLevel {
    /// 正常。
    Ok,
    /// 需要注意。
    Warn,
    /// 有问题。
    Fail,
}

/// `doctor` 的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorReport {
    /// 逐项检查。
    pub checks: Vec<CheckResult>,
}

/// 控制面用的异步处理函数。
pub type HandlerFuture =
    Pin<Box<dyn std::future::Future<Output = Result<Value, IpcError>> + Send + 'static>>;

/// 服务端的命令处理器。
///
/// 实现方**只能**处理 [`Request`] 里的命令 —— 加一个变体就要改这里，这正是我们要的约束。
pub trait ControlHandler: Send + Sync + 'static {
    /// 处理一条请求。
    fn handle(&self, request: Request) -> HandlerFuture;
}

/// 把一条消息编码成一行（末尾带 `\n`）。
pub fn encode_line<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    Ok(line)
}

/// 解析一行（容忍末尾的换行）。
pub fn decode_line<T: for<'de> Deserialize<'de>>(line: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line.trim_end_matches(['\n', '\r']))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_request_round_trips_with_camel_case_commands() {
        let cases = vec![
            (Request::Ping, r#"{"cmd":"ping"}"#),
            (Request::Status, r#"{"cmd":"status"}"#),
            (Request::Version, r#"{"cmd":"version"}"#),
            (
                Request::Doctor { verbose: true },
                r#"{"cmd":"doctor","verbose":true}"#,
            ),
            (
                Request::Stop {
                    reason: Some("用户点了停止".to_owned()),
                },
                r#"{"cmd":"stop","reason":"用户点了停止"}"#,
            ),
            (Request::Restart, r#"{"cmd":"restart"}"#),
            (
                Request::UpdateCheck { force: false },
                r#"{"cmd":"updateCheck","force":false}"#,
            ),
            (Request::UpdateApply, r#"{"cmd":"updateApply"}"#),
            (
                Request::TraceOn { seconds: 60 },
                r#"{"cmd":"traceOn","seconds":60}"#,
            ),
            (Request::TraceOff, r#"{"cmd":"traceOff"}"#),
        ];

        for (request, expected) in cases {
            let encoded = serde_json::to_string(&request).expect("encode");
            assert_eq!(encoded, expected, "命令名是契约的一部分");
            let decoded: Request = serde_json::from_str(&encoded).expect("decode");
            assert_eq!(decoded, request);
        }
    }

    #[test]
    fn optional_fields_can_be_omitted() {
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"cmd":"doctor"}"#).unwrap(),
            Request::Doctor { verbose: false }
        );
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"cmd":"stop"}"#).unwrap(),
            Request::Stop { reason: None }
        );
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"cmd":"updateCheck"}"#).unwrap(),
            Request::UpdateCheck { force: false }
        );
    }

    #[test]
    fn an_unknown_command_is_a_deserialize_error() {
        // 命令集是白名单：不认识的就是不认识，没有「兜底变体」
        assert!(serde_json::from_str::<Request>(r#"{"cmd":"readCredentials"}"#).is_err());
        assert!(
            serde_json::from_str::<Request>(r#"{"cmd":"exec","argv":["rm","-rf","/"]}"#).is_err()
        );
    }

    #[test]
    fn a_client_frame_carries_version_id_and_token() {
        let frame = ClientFrame::new("7", "s3cret", Request::Status);
        let encoded = serde_json::to_string(&frame).expect("encode");
        assert_eq!(
            encoded,
            r#"{"v":1,"id":"7","token":"s3cret","cmd":"status"}"#
        );

        let decoded: ClientFrame = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(decoded, frame);
        assert_eq!(decoded.request.command(), "status");
    }

    #[test]
    fn server_frames_have_the_documented_shape() {
        let ok = ServerFrame::ok("1", json!({"pong": true}));
        assert_eq!(
            serde_json::to_string(&ok).expect("encode"),
            r#"{"v":1,"id":"1","ok":true,"result":{"pong":true}}"#
        );

        let err = ServerFrame::err(
            "2",
            IpcError::new(IpcErrorCode::Unauthorized, "token 不匹配"),
        );
        assert_eq!(
            serde_json::to_string(&err).expect("encode"),
            r#"{"v":1,"id":"2","ok":false,"error":{"code":"unauthorized","message":"token 不匹配"}}"#
        );

        assert!(ServerFrame::ok("3", json!(null)).into_result().is_ok());
        assert_eq!(
            ServerFrame::err("4", IpcError::new(IpcErrorCode::Busy, "正在更新")).into_result(),
            Err(IpcError::new(IpcErrorCode::Busy, "正在更新"))
        );
    }

    #[test]
    fn a_status_report_carries_both_sources() {
        let report = StatusReport {
            process: ProcessStatus {
                running: true,
                host: "127.0.0.1".to_owned(),
                port: 41316,
                version: "0.1.0".to_owned(),
                protocol: IPC_PROTOCOL_VERSION,
                started_at: "2026-10-09T06:00:00Z".to_owned(),
                connections: 2,
                watch_connections: 1,
                last_error: None,
            },
            service: ServiceStatus::not_installed("peon-burrow"),
        };
        let encoded = serde_json::to_string(&report).expect("encode");
        assert!(encoded.contains(r#""watchConnections":1"#));
        assert!(encoded.contains(r#""installed":false"#));
        assert!(
            !encoded.contains("lastError"),
            "缺省字段不该出现：{encoded}"
        );
        let decoded: StatusReport = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(decoded, report);
    }

    #[test]
    fn line_helpers_round_trip() {
        let line = encode_line(&ClientFrame::new("1", "", Request::Ping)).expect("encode");
        assert!(line.ends_with('\n'));
        let decoded: ClientFrame = decode_line(&line).expect("decode");
        assert_eq!(decoded.request, Request::Ping);
        assert_eq!(MAX_LINE_BYTES, 8 * 1024);
    }
}
