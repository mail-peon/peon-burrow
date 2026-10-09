# 模块划分与 crate 契约

> [`01-architecture.md § 2`](./01-architecture.md) 讲了「有哪些 crate、谁依赖谁」；
> 这篇讲**每个 crate 的职责边界、公开 API、明确不做什么、测试落在哪**。
>
> 写代码时如果发现「这个功能不知道该放哪个 crate」，大概率是它跨了边界 —— 先改文档。

---

## 0. 一页速查

| crate | 类型 | 一句话 | 依赖 |
| --- | --- | --- | --- |
| `peon-burrow-core` | lib | 协议与隧道本体：URL/策略/透传/watch。**不碰配置来源、不碰系统** | `peon-burrow-ipc`（只用状态类型）、`tokio`、`tokio-tungstenite`、`tokio-rustls`、`tracing` |
| `peon-burrow-config` | lib | 三层配置合并与校验、平台目录 | `serde`、`toml`、`clap`、`directories` |
| `peon-burrow-ipc` | lib | 控制面协议（类型 + 编解码 + 传输抽象 + 客户端/服务端） | `serde`、`interprocess`、`tokio` |
| `peon-burrow-service` | lib | 服务安装/启停/自启/状态（三平台） | `windows-service`、`service-manager`、`peon-burrow-config` |
| `peon-burrow-update` | lib | 自更新（清单/校验/替换/重启） | `self_update`、`reqwest`、`sha2`、`peon-burrow-config` |
| `burrow` | bin | 组装：CLI、日志、服务入口、控制面服务端 | 以上全部 |

**依赖方向铁律**：`peon-burrow-core` 是叶子（除 `peon-burrow-ipc` 的状态类型），
`peon-burrow-config` 只被上层用，`burrow` 不允许被任何库依赖。

---

## 1. `peon-burrow-core`

### 职责

| 做 | 不做 |
| --- | --- |
| 解析升级请求的 URL → `RelayTarget` | 读配置文件、读环境变量（由 `peon-burrow-config` 传入） |
| 策略检查（token / 白名单 / loopback / `tls=0`+993） | 决定日志输出到哪（只发 `tracing` 事件） |
| WebSocket 服务 + 透传隧道（含双向背压） | 服务注册、进程守护 |
| watch 状态机（IDLE） | 自更新 |
| 关闭码与关闭原因（120 字节截断） | 控制面命令的语义（只管「报告状态」） |

### 公开 API（骨架，落地时以实现为准）

```rust
pub struct RelayOptions {
    pub host: String,                  // 默认 127.0.0.1
    pub port: u16,                     // 默认 41316；0 = 内核分配（测试用）
    pub token: Option<String>,
    pub allowed_hosts: Vec<String>,    // 支持 * 通配
    pub tls_reject_unauthorized: bool, // 默认 true
    pub idle_timeout: Duration,        // 默认 15min（不作用于 watch）
    pub max_connections: usize,        // 默认 32
    pub backpressure_high_water: usize,// 默认 16 MiB
    pub watch_reidle: Duration,        // 默认 25min
    pub watch_retry_delays: Vec<Duration>,
    pub trace: bool,
}

impl RelayOptions {
    /// 「非 loopback 但没有 token / 白名单」这类组合在这里被拒绝，
    /// 而不是等到 bind 之后（配置错误要早失败）。
    pub fn validate(&self) -> Result<(), ConfigError>;
}

pub struct RelayServer { /* ... */ }

impl RelayServer {
    pub async fn start(options: RelayOptions) -> Result<Self, RelayError>;
    pub fn local_addr(&self) -> SocketAddr;
    /// 先主动断开所有连接，再关监听（否则 IMAP 的 IDLE 会把关停卡住）
    pub async fn stop(self) -> Result<(), RelayError>;
    pub fn stats(&self) -> watch::Receiver<RelayStats>;
    /// 运行期发现端口被占用时（绑不定）由调用方处理：失败要能区分 PortInUse
    pub fn url(&self) -> String;
}

pub struct RelayStats {
    pub active_connections: usize,
    pub watch_connections: usize,
    pub tunnels: usize,
    pub bytes_to_server: u64,
    pub bytes_to_client: u64,
    pub started_at: SystemTime,
}

#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    #[error("端口 {0} 已被占用")]
    PortInUse(u16),
    #[error("监听地址无效：{0}")]
    InvalidAddr(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Tls(#[from] rustls::Error),
}
```

### 内部模块

| 模块 | 内容 | 测试重点 |
| --- | --- | --- |
| `target.rs` | URL 解析（路径 / 查询两种形态）、`TargetResolve::{Target,NoTarget,Rejected}` | parity W2–W4、C4–C6 |
| `policy.rs` | token、白名单通配（`*.gmail.com`）、loopback 判定、`tls=0`+993 | parity W5–W6、C5–C6 |
| `line_buffer.rs` | CRLF 行边界（TLS 分片会在任意位置切开一行） | 喂半行 / `* 12 EX` + `ISTS`（parity E17） |
| `close.rs` | 关闭码常量 + 按**字节**截断到 120（UTF-8 安全） | 中文 + emoji（parity F1） |
| `tunnel.rs` | 透传：立刻建连、双向背压、`shutdown` 去重 | parity § 6.1 第 1–5 条 |
| `watch.rs` | 状态机 `Greeting→Login→Select→Idle⇄Done`、重连、致命错误分类 | parity § 6.2 第 10–12 条 |
| `dispatch.rs` | 第一帧分流 + 第一帧补投 + 后续帧注册时机 | parity C7–C10 |
| `events.rs` | `tracing` 事件名（[`design/logging-and-diagnostics.md § 2.4`](./design/logging-and-diagnostics.md)） | 事件名集合的测试 |

### 关键设计约束（来自 parity，违反就是 bug）

1. **输入是值不是全局**：不许出现「模块级 PORT」这种写法 —— 测试要能在同一个进程里起两个实例；
2. **立刻建连**：拿到目标后马上 `connect`，不要先读第一帧；
3. **策略同步**：在 spawn 之前完成，被拒的连接不发一个字节；
4. **Rust 的背压用 `await`**：`SinkExt::send` 本身就会等，**不要**再叠一层 ws pause/resume，
   也不要用无界 channel（那等于没有背压）；
5. **错误分类而不是字符串匹配**：`WatchError::Fatal` / `::Transient`（parity E6 的偏离）。

---

## 2. `peon-burrow-config`

| 做 | 不做 |
| --- | --- |
| 三层合并（CLI > ENV > 文件 > 默认）、校验、错误信息 | 直接 `std::process::exit`（返回错误，由 bin 决定退出码） |
| 平台目录（配置/日志/状态/安装位置） | 探测端口占用（那是运行期的事） |
| ENV 变量名兼容（`PORT` 等，见 [`design/config-schema.md § 3`](./design/config-schema.md)） | 生成默认配置文件（那是 `service install` 的活） |

```rust
pub struct Config { /* 与 relay.toml 一一对应；serde */ }

pub struct Resolved {
    pub config: Config,
    pub paths: Paths,
    pub config_path: Option<PathBuf>,
    pub warnings: Vec<String>,   // 未知字段等
}

pub fn resolve(cli: Cli, env: EnvSource, explicit_path: Option<&Path>) -> Result<Resolved, ConfigError>;

pub struct Paths { /* config_dir, log_dir, data_dir, install_dir, control_file, discover_file, ... */ }
```

⚠️ 未知字段 → `warnings`（不是错误）。理由：[`design/config-schema.md § 2.1`](./design/config-schema.md)。

---

## 3. `peon-burrow-ipc`

| 做 | 不做 |
| --- | --- |
| 定义 `Request` / `Response`（唯一来源，桌面端 git 依赖它） | 实现任何业务逻辑（它是纯协议 + 传输） |
| 一行 JSON 的编解码 + 8 KiB 上限 | 决定 token 怎么生成（由 bin 决定） |
| 传输抽象：本地 socket / loopback TCP | 服务安装、更新 |
| 客户端 `ControlClient`（GUI 与 CLI 共用） | —— |

```rust
pub const PROTOCOL_VERSION: u16 = 1;

#[derive(Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "camelCase")]
pub enum Request {
    Ping,
    Status,
    Version,
    Doctor { verbose: bool },
    Stop { reason: Option<String> },
    Restart,
    UpdateCheck { force: bool },
    UpdateApply,
    TraceOn { seconds: u32 },
    TraceOff,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "ok")]
pub enum Response {
    Ok { result: serde_json::Value },
    Err { error: IpcError },
}
```

⚠️ **命令集是白名单**：`Request` 的枚举就是全部能力，加一个变体 = 改协议 = 改测试
（[`design/control-plane-ipc.md § 5`](./design/control-plane-ipc.md) 的验收第 8 条）。

---

## 4. `peon-burrow-service`

统一 trait（三平台实现）：

```rust
pub trait ServiceHost {
    fn status(&self) -> Result<ServiceStatus>;       // installed / running / autostart
    fn install(&self, opts: &InstallOptions) -> Result<()>;
    fn uninstall(&self) -> Result<()>;
    fn start(&self) -> Result<()>;
    fn stop(&self) -> Result<()>;
    fn set_autostart(&self, on: bool) -> Result<()>;
    /// 安装后自检：失败重启策略是否真的写进去了（见 ADR-0003 § 3）
    fn verify_restart_policy(&self) -> Result<()>;
}
```

| 平台 | 用户级实现 | 系统级实现 |
| --- | --- | --- |
| Windows | 任务计划程序（XML，`Hidden` + 失败重启） | SCM（`windows-service`，`ServiceFailureActions`） |
| macOS | LaunchAgent plist（`KeepAlive`） | LaunchDaemon plist |
| Linux | systemd `--user`（退路：XDG autostart） | systemd system unit |

**服务运行入口**（`run_as_service`）在 bin 里，但由本 crate 提供适配：
Windows 需要 SCM dispatcher 在**独立线程**上跑；Unix 需要正确处理 `SIGTERM` 与（systemd 的）`sd_notify` 可选支持。

---

## 5. `peon-burrow-update`

```rust
pub struct UpdateOptions {
    pub channel: Channel,        // Stable | Beta
    pub base_url: Option<String>,// 镜像站前缀
    pub proxy: Option<String>,   // 从配置读（**不读环境变量**）
    pub require_signature: bool, // 默认 true
    pub pubkeys: Vec<String>,
}

pub struct UpdateStatus { pub available: bool, pub current: Version, pub latest: Version, /* … */ }

pub async fn check(current: &Version, opts: &UpdateOptions) -> Result<UpdateStatus, UpdateError>;
pub async fn apply(current: &Version, opts: &UpdateOptions) -> Result<Applied, UpdateError>;
/// 替换成功后调用：请求宿主以「非正常退出」结束，交给服务管理器重启（见 update-flow § 6）
pub fn request_restart_after_update() -> !;
```

**四道闸必须都在**：size → sha256 → 签名 → 冒烟测试（`staging/<exe> version --json`）。
「替换正在运行的自己」用 `self_update` / `self-replace`，但**不要**用它的交互式默认值（`.no_confirm(true)`）。

---

## 6. `burrow`（bin）

| 模块 | 内容 |
| --- | --- |
| `cli.rs` | `clap` 子命令（[`design/config-schema.md § 4`](./design/config-schema.md)），退出码映射 |
| `logging.rs` | `tracing` 初始化（文件滚动 + stdout + 可选事件日志）、凭据脱敏层 |
| `app.rs` | 组装：配置 → 日志 → 控制面 → `RelayServer` → 信号处理 |
| `service_entry.rs` | Windows 服务入口 / Unix 前台入口的差异 |
| `console.rs` | 仅前台 TTY 下的 `q` / `r`（服务态**完全不注册** stdin） |
| `doctor.rs` | 13 项自检（[`design/logging-and-diagnostics.md § 3`](./design/logging-and-diagnostics.md)） |
| `control_server.rs` | 控制面服务端，把 `Request` 映射到 `RelayServer` / `peon-burrow-update` / `peon-burrow-service` |
| `build_info.rs` | `gitSha` / `buildTime`（`build.rs` 或 `option_env!`） |

---

## 7. 测试落点（速查）

| 层 | 位置 | 说明 |
| --- | --- | --- |
| 单测 | 各 crate 的 `#[cfg(test)]` | 纯函数优先：URL 解析、白名单、截断、行缓冲、配置合并 |
| 集成 | `crates/peon-burrow-core/tests/` | 真 TCP/TLS 回声服务器 + `port: 0`（见 [`testing.md`](./testing.md)） |
| 服务层 | `crates/peon-burrow-service/tests/` | 只测「查询与自检」的只读路径；装/卸需要提权 → 手动 checklist |
| 自更新 | `crates/peon-burrow-update/tests/` | 用本地 HTTP server 伪造清单与资产（含各种坏输入） |
| 控制面 | `crates/peon-burrow-ipc/tests/` | token、长度上限、未知命令、并发 |
| 端到端 | 手动 checklist（[`testing.md § 5`](./testing.md)） | 真邮箱、真服务、真更新 |
