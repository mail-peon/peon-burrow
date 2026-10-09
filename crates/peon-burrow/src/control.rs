//! 控制面接线：引擎状态 → 协议类型，命令 → `run()` 的动作。
//!
//! 映射写成**纯函数**（[`process_status`]）是为了能单测：
//! 控制面一旦和引擎状态耦合，测起来就得真起一个中继。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use peon_burrow_core::RelayState;
use peon_burrow_ipc_types::{
    ControlHandler, DoctorReport, HandlerFuture, IPC_PROTOCOL_VERSION, IpcError, IpcErrorCode,
    ProcessStatus, Request, ServiceStatus, StatusReport,
};
use serde_json::{Value, json};

/// `run()` 需要执行的生命周期动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    /// 停下来。
    Stop,
    /// 重启（进程以 `ExitCode::RestartRequested` 结束，由服务管理器拉起）。
    Restart,
}

/// 自更新动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateAction {
    /// 检查（`force` 忽略节流）。
    Check {
        /// 忽略节流。
        force: bool,
    },
    /// 应用。
    Apply,
}

/// 自更新钩子（由 `run()` 接线；没接就回「未启用」）。
pub type UpdateHook = Arc<dyn Fn(UpdateAction) -> HandlerFuture + Send + Sync>;

/// 生命周期钩子：返回 `false` 表示动作没能递出去（服务正在关闭）。
///
/// 用回调而不是 channel：`run()` 自己也要监听同一个信号，而 channel 只能有一个接收端。
pub type LifecycleHook = Arc<dyn Fn(Lifecycle) -> bool + Send + Sync>;

/// 控制面处理器。
pub struct RelayControl {
    snapshot: Arc<dyn Fn() -> RelayState + Send + Sync>,
    service_status: Arc<dyn Fn() -> ServiceStatus + Send + Sync>,
    doctor: Arc<dyn Fn(bool) -> DoctorReport + Send + Sync>,
    lifecycle: LifecycleHook,
    verbose: Arc<AtomicBool>,
    update: Option<UpdateHook>,
    version: String,
    host: String,
    port: u16,
}

impl RelayControl {
    /// 组装。
    // 参数确实多，但每一个都是**必须由 `run()` 注入的协作者**；打包成一个 struct 只是把
    // 同样的清单挪个地方，调用处反而更绕
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        snapshot: Arc<dyn Fn() -> RelayState + Send + Sync>,
        service_status: Arc<dyn Fn() -> ServiceStatus + Send + Sync>,
        doctor: Arc<dyn Fn(bool) -> DoctorReport + Send + Sync>,
        lifecycle: LifecycleHook,
        verbose: Arc<AtomicBool>,
        version: impl Into<String>,
        host: impl Into<String>,
        port: u16,
    ) -> Self {
        Self {
            snapshot,
            service_status,
            doctor,
            lifecycle,
            verbose,
            update: None,
            version: version.into(),
            host: host.into(),
            port,
        }
    }

    /// 接上自更新。
    pub fn with_update(mut self, hook: UpdateHook) -> Self {
        self.update = Some(hook);
        self
    }

    /// 当前的 trace 开关（日志过滤器读它）。
    pub fn verbose_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.verbose)
    }
}

impl ControlHandler for RelayControl {
    fn handle(&self, request: Request) -> HandlerFuture {
        let version = self.version.clone();
        let host = self.host.clone();
        let port = self.port;
        let snapshot = Arc::clone(&self.snapshot);
        let service_status = Arc::clone(&self.service_status);
        let doctor = Arc::clone(&self.doctor);
        let lifecycle = self.lifecycle.clone();
        let verbose = Arc::clone(&self.verbose);
        let update = self.update.clone();

        Box::pin(async move {
            match request {
                Request::Ping => Ok(json!({ "pong": true, "version": version })),

                Request::Version => Ok(json!({
                    "version": version,
                    "protocol": IPC_PROTOCOL_VERSION,
                    "target": std::env::consts::OS,
                    "arch": std::env::consts::ARCH,
                })),

                Request::Status => {
                    let report = StatusReport {
                        process: process_status(&snapshot(), &version, &host, port),
                        service: service_status(),
                    };
                    serde_json::to_value(report)
                        .map_err(|error| IpcError::new(IpcErrorCode::Internal, error.to_string()))
                }

                Request::Doctor { verbose: detailed } => serde_json::to_value(doctor(detailed))
                    .map_err(|error| IpcError::new(IpcErrorCode::Internal, error.to_string())),

                Request::Stop { reason } => {
                    if !lifecycle(Lifecycle::Stop) {
                        return Err(IpcError::new(IpcErrorCode::Busy, "服务正在关闭"));
                    }
                    Ok(json!({ "stopping": true, "reason": reason }))
                }

                Request::Restart => {
                    if !lifecycle(Lifecycle::Restart) {
                        return Err(IpcError::new(IpcErrorCode::Busy, "服务正在重启"));
                    }
                    Ok(json!({ "restarting": true }))
                }

                Request::TraceOn { seconds } => {
                    // 上限一小时：trace 会写明文凭据，不能忘在那儿
                    let seconds = seconds.clamp(1, 3600);
                    verbose.store(true, Ordering::Relaxed);
                    let flag = Arc::clone(&verbose);
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(u64::from(seconds))).await;
                        flag.store(false, Ordering::Relaxed);
                    });
                    Ok(json!({ "trace": true, "seconds": seconds }))
                }

                Request::TraceOff => {
                    verbose.store(false, Ordering::Relaxed);
                    Ok(json!({ "trace": false }))
                }

                Request::UpdateCheck { force } => match update {
                    Some(hook) => hook(UpdateAction::Check { force }).await,
                    None => Err(IpcError::new(
                        IpcErrorCode::Busy,
                        "自更新没启用（配置里 update.enabled = false，或安装方式不支持自更新）",
                    )),
                },

                Request::UpdateApply => match update {
                    Some(hook) => hook(UpdateAction::Apply).await,
                    None => Err(IpcError::new(
                        IpcErrorCode::Busy,
                        "自更新没启用（配置里 update.enabled = false，或安装方式不支持自更新）",
                    )),
                },
            }
        })
    }
}

/// 引擎状态 → 协议里的进程状态（纯函数，可单测）。
pub fn process_status(state: &RelayState, version: &str, host: &str, port: u16) -> ProcessStatus {
    ProcessStatus {
        running: state.running,
        host: host.to_owned(),
        port,
        version: version.to_owned(),
        protocol: IPC_PROTOCOL_VERSION,
        started_at: rfc3339(state.started_at),
        connections: state.active_connections,
        watch_connections: state.watch_connections,
        last_error: state.last_error.clone(),
    }
}

/// `SystemTime` → RFC 3339（UTC）。
///
/// 手写而不是拉一个日期库：这里只需要一个**能被 `Date.parse` 解析**的时间戳，
/// 而 `chrono`/`time` 对这一个函数来说太重了。
pub fn rfc3339(time: SystemTime) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();

    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    let (hour, minute, second) = (rest / 3600, (rest % 3600) / 60, rest % 60);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// 天数（1970-01-01 起）→ 公历年月日。
///
/// Howard Hinnant 的 `civil_from_days`：把「闰年 + 世纪」的边界处理成一次线性运算。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// 从 JSON 里取一个字段（控制面响应是 `Value`）。
pub fn field<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.get(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use peon_burrow_ipc_types::ServiceStatus;

    fn state() -> RelayState {
        RelayState {
            running: true,
            active_connections: 3,
            watch_connections: 1,
            bytes_to_server: 100,
            bytes_to_client: 200,
            started_at: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            last_error: Some("boom".to_owned()),
        }
    }

    #[test]
    fn process_status_maps_every_field() {
        let status = process_status(&state(), "0.1.0", "127.0.0.1", 41316);
        assert!(status.running);
        assert_eq!(status.port, 41316);
        assert_eq!(status.version, "0.1.0");
        assert_eq!(status.protocol, IPC_PROTOCOL_VERSION);
        assert_eq!(status.connections, 3);
        assert_eq!(status.watch_connections, 1);
        assert_eq!(status.last_error.as_deref(), Some("boom"));
        assert_eq!(status.started_at, "2023-11-14T22:13:20Z");
    }

    #[test]
    fn rfc3339_matches_known_timestamps() {
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            "2023-11-14T22:13:20Z"
        );
        // 闰日
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_secs(1_709_164_800)),
            "2024-02-29T00:00:00Z"
        );
    }

    async fn handle(control: &RelayControl, request: Request) -> Result<Value, IpcError> {
        control.handle(request).await
    }

    fn control(lifecycle: tokio::sync::mpsc::UnboundedSender<Lifecycle>) -> RelayControl {
        RelayControl::new(
            Arc::new(state),
            Arc::new(|| ServiceStatus::not_installed("peon-burrow")),
            Arc::new(|detailed| DoctorReport {
                checks: vec![peon_burrow_ipc_types::CheckResult {
                    id: "test".to_owned(),
                    level: peon_burrow_ipc_types::CheckLevel::Ok,
                    title: if detailed { "详细" } else { "简略" }.to_owned(),
                    detail: String::new(),
                    action: None,
                }],
            }),
            Arc::new(move |action| lifecycle.send(action).is_ok()),
            Arc::new(AtomicBool::new(false)),
            "0.1.0",
            "127.0.0.1",
            41316,
        )
    }

    #[tokio::test]
    async fn ping_and_version_answer() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let control = control(tx);
        assert_eq!(
            field(&handle(&control, Request::Ping).await.unwrap(), "pong"),
            Some(&json!(true))
        );
        assert_eq!(
            field(
                &handle(&control, Request::Version).await.unwrap(),
                "version"
            ),
            Some(&json!("0.1.0"))
        );
    }

    #[tokio::test]
    async fn status_combines_process_and_service() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let control = control(tx);
        let value = handle(&control, Request::Status).await.unwrap();
        assert_eq!(value["process"]["port"], json!(41316));
        assert_eq!(value["service"]["installed"], json!(false));
    }

    #[tokio::test]
    async fn doctor_passes_the_verbose_flag() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let control = control(tx);
        let brief = handle(&control, Request::Doctor { verbose: false })
            .await
            .unwrap();
        assert_eq!(brief["checks"][0]["title"], json!("简略"));
        let full = handle(&control, Request::Doctor { verbose: true })
            .await
            .unwrap();
        assert_eq!(full["checks"][0]["title"], json!("详细"));
    }

    #[tokio::test]
    async fn stop_restart_and_trace_are_dispatched() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let control = control(tx);

        handle(
            &control,
            Request::Stop {
                reason: Some("test".to_owned()),
            },
        )
        .await
        .unwrap();
        assert_eq!(rx.recv().await, Some(Lifecycle::Stop));

        handle(&control, Request::Restart).await.unwrap();
        assert_eq!(rx.recv().await, Some(Lifecycle::Restart));

        let flag = control.verbose_flag();
        assert!(!flag.load(Ordering::Relaxed));
        handle(&control, Request::TraceOn { seconds: 60 })
            .await
            .unwrap();
        assert!(flag.load(Ordering::Relaxed));
        handle(&control, Request::TraceOff).await.unwrap();
        assert!(!flag.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn update_commands_fail_closed_without_a_hook() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let control = control(tx);
        let error = handle(&control, Request::UpdateApply)
            .await
            .expect_err("no hook");
        assert_eq!(error.code, IpcErrorCode::Busy);
        assert!(error.message.contains("自更新"));
    }

    #[tokio::test]
    async fn trace_on_is_clamped_to_one_hour() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let control = control(tx);
        let value = handle(&control, Request::TraceOn { seconds: 999_999 })
            .await
            .unwrap();
        assert_eq!(value["seconds"], json!(3600));
    }
}
