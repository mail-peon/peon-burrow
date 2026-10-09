# 模块划分与 crate 契约

> **这是一个库项目，不是一个插件的配套 daemon。** `mail-peon` 扩展只是接入方之一。
> 依赖图的唯一真相在 [`01-architecture.md § 2`](./01-architecture.md)；
> 稳定性分级与 semver 政策见 [`../STABILITY.md`](../STABILITY.md)。
>
> 布局铁律（违反就是返工）：

| # | 铁律 | 检查方式 |
| --- | --- | --- |
| L1 | **稳定层不许依赖产品层**：`protocol` / `core` / `ipc-types` / `ipc` / `service` / `update` 互不反向依赖 | § 12 的守卫脚本 |
| L2 | **类型与传输分开**：`ipc-types` 只依赖 `serde`；`ipc` 才有 `interprocess`/`tokio` | `cargo tree -p peon-burrow-ipc-types` |
| L3 | **默认值只有一个来源**（`core`） | `RelayOptions::default()` 是唯一处 |
| L4 | **注入而非全局**：`Paths` / 时钟 / 进程探测都是值或 trait | grep `ProjectDirs` 只应命中 `peon-burrow/src/config/paths.rs` |
| L5 | **bin 只有 40 行**：逻辑在同 crate 的 lib 里 | `wc -l crates/peon-burrow/src/main.rs` |

---

## 0. 一页速查

| crate | 类型 | 一句话 | 发布 | 稳定性 |
| --- | --- | --- | --- | --- |
| `peon-burrow-protocol` | lib | **线上协议**：目标解析、watch 报文、关闭码、`WATCH_PROTOCOL_VERSION`。只依赖 `serde`/`url` | ✅ | **稳定** |
| `peon-burrow-core` | lib | **引擎**：server / tunnel / 策略 / 背压 / `RelayState`；feature `imap-watch`（**默认关**） | ✅ | **稳定** |
| `peon-burrow-ipc-types` | lib | **控制面类型**（`IPC_PROTOCOL_VERSION`）。只依赖 `serde` | ✅ | **稳定** |
| `peon-burrow-ipc` | lib | 控制面**传输** + 客户端/服务端 | ✅ | **稳定** |
| `peon-burrow-service` | lib | 跨平台服务托管 + `probe_port` + `RestartStrategy`（**与中继无关，可单独用**） | ✅ | **稳定** |
| `peon-burrow-update` | lib | 自更新：清单/校验/替换/重启（**与中继无关，可单独用**） | ✅ | **稳定** |
| **`peon-burrow`** | lib + **bin `burrow`** | **产品**：config / doctor / 控制面接线 / `ExitCode`；**默认开 `imap-watch`** | ✅ ← 用户装这个 | lib 不承诺稳定 |
| `peon-burrow-testkit` | lib | 假上游（echo / mock IMAP）+ harness | ❌ `publish = false` | 内部 |

```bash
cargo install peon-burrow      # → burrow 命令
cargo add peon-burrow-core     # → 只想把中继嵌进自己的程序
cargo add peon-burrow-service  # → 只想给自家 daemon 加「装成服务」
```

---

## 1. `peon-burrow-protocol`（稳定层）

**最该稳定的一层**：别的语言实现客户端/服务端时，只需要读这个 crate 的文档 + 类型。

```rust
pub const WATCH_PROTOCOL_VERSION: u16 = 1;

pub enum TargetResolve { Target(RelayTarget), NoTarget, Rejected(RejectReason) }
pub struct RelayTarget { pub host: String, pub port: u16, pub tls: bool }
pub fn resolve_target(uri: &Uri) -> TargetResolve;
pub fn is_host_allowed(host: &str, patterns: &[String]) -> bool;

pub struct WatchRequest { /* __watch / accountId / host / port / tls / user / pass / token */ }
pub enum ClientMessage { Mail { account_id: String, exists: u32 }, State(WatchState) }

pub mod close_code { pub const POLICY: u16 = 1008; pub const WATCH_FAILED: u16 = 1011; }
pub fn truncate_close_reason(text: &str) -> String;   // ≤ 120 字节、UTF-8 安全
```

| 做 | 不做 |
| --- | --- |
| 协议形状与规则（解析、校验、序列化、关闭码截断） | 任何 IO（不依赖 tokio / rustls） |
| 白名单**匹配规则** | 策略**执行**（那是 core 的 `Policy`） |

⚠️ **改这里 = 改协议**：必须同时更新 [`design/wire-protocol.md`](./design/wire-protocol.md)
与 `mail-peon` 扩展，并升 `WATCH_PROTOCOL_VERSION`（向后兼容规则见该文档 § 5）。

---

## 2. `peon-burrow-core`（稳定层，可嵌入）

| 做 | 不做 |
| --- | --- |
| WebSocket 服务 + 透传隧道（双向背压） | 读配置/环境变量（值由调用方给） |
| 策略**执行**（默认 `Policy`：token / 白名单 / loopback / `tls=0`+993） | 服务注册、自更新、控制面 |
| `RelayState`（可查询状态广播） | 校验**配置组合**（那是产品层） |
| feature `imap-watch` 时的 IMAP IDLE 监听 | 解析邮件内容 |

```rust
pub struct RelayOptions { /* 默认值唯一定义处（L3） */ }
impl Default for RelayOptions { /* … */ }

/// 接入点 1：访问控制可替换
pub trait Policy: Send + Sync {
    fn check(&self, target: &RelayTarget, req: &RequestContext) -> Result<(), RejectReason>;
}
/// 接入点 2：TLS 可注入（私有 CA / 客户端证书 / 自签）
pub struct TlsConfig { /* roots / verifier / alpn */ }

pub struct RelayServer { /* … */ }
impl RelayServer {
    pub async fn start(options: RelayOptions) -> Result<Self, RelayError>;
    pub async fn start_with(options: RelayOptions, policy: Arc<dyn Policy>, tls: TlsConfig) -> Result<Self, RelayError>;
    pub fn local_addr(&self) -> SocketAddr;
    pub async fn stop(self) -> Result<(), RelayError>;      // 先断连接再关监听
    pub fn state(&self) -> tokio::sync::watch::Receiver<RelayState>;
    pub fn url(&self) -> String;
}
```

### feature：`imap-watch`（**默认关**）

| 构建 | 行为 |
| --- | --- |
| `default`（关） | 纯字节隧道。收到 `__watch` 请求 → 以 `1008` 拒绝，原因写明「本构建未启用 imap-watch」 |
| `imap-watch`（开） | 启用 `watch.rs` + `line_buffer.rs`（IMAP `IDLE` 监听与推送） |

> 开发者按需开：`peon-burrow-core = { version = "0.1", features = ["imap-watch"] }`。
> **CLI 默认开**（不然 mail-peon 那条链路会静默失效），见 § 7。

### 内部模块

| 模块 | 内容 |
| --- | --- |
| `error.rs` | `Transient` / `Fatal` 分类（取代 TS 的正则匹配）、错误 → 用户文案 |
| `policy.rs` | 默认 `Policy` 实现（token / 白名单通配 / loopback / `tls=0`+993） |
| `transport.rs` | **建 TCP/TLS 的唯一实现**：SNI 规则（IP 字面量不发）、根证书、证书错误 → 人话 |
| `dispatch.rs` | 第一帧分流 + 第一帧补投 + 后续帧注册时机 |
| `tunnel.rs` | 透传：立刻建连、双向背压、`shutdown` 去重 |
| `state.rs` | `RelayState` 广播、连接注册表（`JoinSet` + `CancellationToken`） |
| `watch.rs` ＋ `line_buffer.rs` | 仅 `imap-watch`（IMAP 状态机 + CRLF 行边界） |
| `events.rs` | `tracing` 事件名（[`design/logging-and-diagnostics.md § 2.4`](./design/logging-and-diagnostics.md)） |

> ⚠️ **`transport.rs` 只有一份**：TS 版把「TLS 建连 + SNI + 证书错误提示」在
> `imap-relay.ts:685-692` 与 `1016-1023` 各写一遍、证书提示在 `843-847` 与 `1243-1247` 各写一遍。

### 关键约束

1. **输入是值不是全局**（测试要能在同进程起两个实例）；
2. **立刻建连**，不要等第一帧；**策略同步**，被拒连接不发一个字节；
3. **背压用 `await`**（不用无界 channel，不叠 ws pause/resume）；
4. **错误分类而非字符串匹配**；**公开 API 用 `Bytes`**（不出现 `Vec<u8>`）；
5. **禁止 `std::sync::Mutex`**；每个 `await` 必须有超时或取消源。

---

## 3. `peon-burrow-ipc-types`（稳定层，契约）

```rust
pub const IPC_PROTOCOL_VERSION: u16 = 1;   // ⚠️ 与 WATCH_PROTOCOL_VERSION 区分（约定 C1）

pub enum Request { Ping, Status, Version, Doctor { verbose: bool }, Stop { reason: Option<String> },
                   Restart, UpdateCheck { force: bool }, UpdateApply, TraceOn { seconds: u32 }, TraceOff }
pub enum Response { Ok { result: Value }, Err { error: IpcError } }

pub struct StatusReport { pub process: ProcessStatus, pub service: ServiceStatus }
pub struct ProcessStatus { /* running/host/port/version/protocol/startedAt/connections/watchConnections/lastError */ }
pub struct ServiceStatus { /* installed/running/level/autostart/name/binaryPath/requiresElevation/restartPolicyConfigured/lastExitCode */ }
```

- **命令集是白名单**：枚举即全部能力，加一个变体 = 改协议 = 改测试；
- `ProcessStatus` 由 `RelayState` 映射而来，**映射发生在产品层**（§ 7）——
  这样 `core` 不必依赖 `ipc-types`（L1）。

---

## 4. `peon-burrow-ipc`（稳定层）

一行 JSON（≤ 8 KiB）请求 → 一行 JSON 响应。默认**本地 socket**（命名管道 / UDS），
系统服务模式下额外开 **loopback TCP**（跨完整性级别）。
见 [`design/control-plane-ipc.md`](./design/control-plane-ipc.md)。

```rust
pub trait Transport: Send + Sync { /* connect / listen */ }
pub struct ControlClient { /* … */ }
impl ControlClient { pub async fn request(&self, req: Request) -> Result<Response, IpcError>; }
pub async fn serve(transport: Box<dyn Transport>, handler: Handler) -> Result<(), IpcError>;
```

---

## 5. `peon-burrow-service`（稳定层，**与中继无关**）

任何「要把自己的 daemon 装成服务」的项目都能用：

```rust
pub trait ServiceHost { fn status(..); fn install(..); fn uninstall(..); fn start(..); fn stop(..);
                        fn set_autostart(..); fn verify_restart_policy(..); }

pub enum ServiceLevel { User, System }
pub enum Autostart { Logon, Boot, Off }
pub enum RestartStrategy { ScmFailureActions, ScheduledTask, Systemd, Launchd, None }
pub fn probe_port(port: u16, control: Option<&ControlClient>) -> Result<PortStatus, ServiceError>;
```

| 平台 | 用户级（零提权，默认） | 系统级（一次提权） |
| --- | --- | --- |
| Windows | 任务计划程序 XML（`Hidden` + 失败重启） | SCM（`windows-service` + `ServiceFailureActions`） |
| macOS | LaunchAgent plist（`KeepAlive`） | LaunchDaemon plist |
| Linux | systemd `--user`（退路：XDG autostart） | systemd system unit |

> ⚠️ **不拆成 `service-windows` / `-macos` / `-linux`**：公开 API 相同，拆开只会得到三个版本号、
> 三份 CI 矩阵与一堆 `cfg` 转发；平台差异用 `windows.rs` / `macos.rs` / `linux.rs` 模块 + 单一 trait 收口。

---

## 6. `peon-burrow-update`（稳定层，**与中继无关**）

依赖倒置：**不自查环境**，环境由调用方组装。

```rust
pub struct UpdateContext { pub current_version: Version, pub install_dir: PathBuf,
                           pub restart: RestartStrategy, pub channel: Channel,
                           pub base_url: Option<String>, pub proxy: Option<String>,
                           pub require_signature: bool, pub pubkeys: Vec<String> }
pub async fn check(ctx: &UpdateContext) -> Result<UpdateStatus, UpdateError>;
pub async fn apply(ctx: &UpdateContext) -> Result<Applied, UpdateError>;
```

四道闸：`size` → `sha256` → 签名 → 冒烟测试（`staging/burrow version --json`）。
清单格式本身是契约，见 [`design/update-flow.md`](./design/update-flow.md)。

---

## 7. `peon-burrow`（产品层：lib + bin `burrow`）

```
crates/peon-burrow/
├── src/main.rs        # ≈ 40 行：Cli::parse → peon_burrow::run(cli) → ExitCode
├── src/lib.rs         # 产品组装（下表的模块）
├── src/config/        # relay.toml + ENV + CLI 三层合并、组合规则校验、Paths（可注入）
├── src/doctor/        # 检查项注册表（每个检查一个 struct）
├── src/control.rs     # 控制面服务端接线 + Request → 引擎/服务/更新的映射
├── src/run.rs         # run(cli) -> ExitCode
└── src/exit.rs        # ExitCode + AppError（各层错误 From 转换）
```

```toml
# crates/peon-burrow/Cargo.toml 要点
[[bin]] name = "burrow"            # ← cargo install peon-burrow 得到 burrow
[lib]  name = "peon_burrow"        # 产品逻辑放这里 → 集成测试可依赖（L5）

[features]
default    = ["imap-watch"]        # CLI 默认开启，否则 mail-peon 链路静默失效
imap-watch = ["peon-burrow-core/imap-watch"]

[package.metadata.binstall]        # crate 名 ≠ bin 名，必须给
bin-dir = "{ bin }{ binary-ext }"
```

| 模块 | 内容 | 归属理由 |
| --- | --- | --- |
| `config/` | `Config`（TOML 字段全 `Option`）/ 组合规则（非 loopback ⇒ token+白名单）/ `Paths`（`discover()` 是唯一碰 `directories` 的地方，测试用 `for_test()`） | 产品配置不是库 |
| `doctor/` | 检查项注册表 + 两种输出（人类可读 / `--json`） | 产品体检 |
| `control.rs` | 控制面命令映射（纯逻辑，可单测）+ `From<&RelayState> for ProcessStatus` | 跨层映射放这里，稳定层保持独立（L1） |
| `exit.rs` | `ExitCode { Ok=0, Runtime=1, Config=2, PortInUse=3, NeedElevation=4, RestartRequested=5 }` + `AppError` | 退出码唯一来源（C2） |
| `run.rs` | 配置 → 日志 → 控制面 → `RelayServer` → 信号；前台 TTY 才有 `q`/`r` | 组装 |

> `STABILITY.md` 里写明：本 crate 的 **lib 不承诺 API 稳定**（它是产品的内部结构，公开只为可测试与复用）。
> 未来若需要第二个命令（如 Windows 提权辅助 `burrow-elevate`），在同一 crate 再加一个 `[[bin]]`。

---

## 8. `peon-burrow-testkit`（`publish = false`）

| 内容 | 解决什么 |
| --- | --- |
| `echo.rs` | 明文 TCP 回声 + TLS 回声（`rcgen` 现场生成自签证书，不依赖 openssl） |
| `mock_imap.rs` | 脚本化 IMAP 服务器（`Vec<Step>`：Expect / Send / Close / Delay） |
| `harness.rs` | 起中继（`port: 0`）、**等就绪**（不是 `sleep`）、断言 helper |
| `paths.rs` | `Paths::for_test(tempdir)` |

放在独立 crate 而不是 `tests/common/`：`protocol` / `core` / `ipc` / `peon-burrow` 四处都要用，
写在某一个 crate 的 `tests/` 里等于其他三处只能复制。**不发布**（发布它等于承诺一套测试 API）。

---

## 9. `examples/`（workspace 成员，`publish = false`）

**示例就是接入文档**：

```
examples/
├── embed_relay.rs        # 30 行把中继嵌进自己的程序
├── custom_policy.rs      # 换成自己的 Policy（公司白名单 / 动态 token）
├── plain_tunnel.rs       # default-features = false：当纯字节隧道用
├── own_service.rs        # 用 peon-burrow-service 装自己的 daemon
├── self_update.rs        # 用 peon-burrow-update 给自家工具做自更新
├── ipc_client.rs         # 用自己的 GUI/脚本连控制面
└── test_with_testkit.rs  # 用 testkit 写集成测试
```

---

## 10. 接入点总表（「怎么对接」）

| 想做的事 | 用什么 | 需要依赖 |
| --- | --- | --- |
| 自己实现客户端/服务端（任意语言） | [`design/wire-protocol.md`](./design/wire-protocol.md)（冻结契约） | 无（读文档） |
| Rust 里实现客户端/服务端 | `peon-burrow-protocol` | `protocol` |
| 把中继嵌进自己的程序 | `RelayServer::start_with` | `core` |
| 自定义访问控制 | `Policy` trait | `core` |
| 自定义 TLS（私有 CA / 客户端证书） | `TlsConfig` 注入 | `core` |
| 只要字节隧道，不要 IMAP | `default-features = false` | `core` |
| 自己写 GUI / 监控 | `ControlClient` + `RelayState` | `ipc`（+ `ipc-types`） |
| 免费拿一套「服务安装器」 | `ServiceHost` | `service` |
| 给自己工具做自更新 | `UpdateContext` + 清单格式 | `update` |
| 在测试里起中继 + 假上游 | `testkit` | `testkit`（dev） |

---

## 11. 约定（跨 crate）

| # | 约定 | 说明 |
| --- | --- | --- |
| C1 | 两个版本常量分开 | `WATCH_PROTOCOL_VERSION`（扩展↔中继，在 `protocol`）/ `IPC_PROTOCOL_VERSION`（GUI↔中继，在 `ipc-types`） |
| C2 | 退出码唯一来源 | `peon_burrow::ExitCode`；update 要重启时返回 `RestartRequested = 5` |
| C3 | 错误分层 | 每层有自己的错误类型；`AppError` 只聚合 + `exit_code()`，不吞原因 |
| C4 | 状态 vs 日志 | 状态走 `RelayState` / `ServiceStatus`；日志走 `tracing`。**不许从日志解析状态** |
| C5 | 并发 | 禁 `std::sync::Mutex`；`tokio::sync` + `JoinSet` + `CancellationToken`；每个 `await` 有超时或取消源 |
| C6 | 字节类型 | 公开 API 用 `bytes::Bytes` |
| C7 | 注入 | `Paths` / 时钟 / 进程探测都是值或 trait |
| C8 | 时间与超时 | 默认值集中在 `core`；用 `Duration` 不用裸 `u64` |
| C9 | 稳定层不许依赖产品层 | 见 § 12 |

---

## 12. 依赖与布局守卫（进 CI）

```bash
# C9/L1：稳定层不许反向依赖产品层
! cargo tree -p peon-burrow-core      | grep -E 'peon-burrow '
! cargo tree -p peon-burrow-protocol  | grep -E 'tokio|rustls|interprocess'
# L2：类型层不许引入传输依赖
! cargo tree -p peon-burrow-ipc-types | grep -E 'tokio|interprocess'
# L5：bin 只有 40 行
test "$(wc -l < crates/peon-burrow/src/main.rs)" -le 60
# feature 矩阵：默认关也能编过
cargo check -p peon-burrow-core --no-default-features
cargo check -p peon-burrow      --no-default-features
```

---

## 13. 测试落点

| 层 | 位置 | 说明 |
| --- | --- | --- |
| 纯函数单测 | 各 crate 的 `#[cfg(test)]` | 目标解析、白名单、截断、行缓冲、配置合并与校验 |
| 协议 | `crates/peon-burrow-protocol/tests/` | URL 两形态、关闭码、报文往返（无 IO，最快） |
| 引擎 | `crates/peon-burrow-core/tests/` | 真 TCP/TLS 回声 + `port: 0`；`imap-watch` 用例用 `#[cfg(feature)]` 门控 |
| 控制面 | `crates/peon-burrow-ipc/tests/` | token、8 KiB 上限、未知命令、TCP 退路 |
| 产品 | `crates/peon-burrow/tests/` | 命令映射、`doctor` 注册表、退出码、`Config→RelayOptions` |
| 服务层 | `crates/peon-burrow-service/tests/` | 只读路径（`status` / `probe_port`） |
| 自更新 | `crates/peon-burrow-update/tests/` | 本地 HTTP server 伪造清单与坏输入 |
| 端到端 | 手动 checklist | 真邮箱、真服务、真更新（[`testing.md § 5`](./testing.md)） |