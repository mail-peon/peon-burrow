# 02 · 技术选型

> 版本号核验于 **2026-10-09**（crates.io API）。选型原则：
> ① 优先「一件事一个 crate」；② 许可证必须 MIT / Apache-2.0 系（**不引入 LGPL**）；
> ③ 维护活跃（近半年有发布）；④ 能在 Windows 上跑通（本项目的第一目标平台）；⑤ **许可证保持 MIT**（[`../STABILITY.md § 7`](../STABILITY.md)）。
>
> ⚠️ 版本会漂移：这份表是**决策记录**，不是锁文件。落地时以 `Cargo.toml` 与 `Cargo.lock` 为准。

---

## 1. 总表

| 用途 | 选型 | 版本 | 许可证 | 一句话理由 |
| --- | --- | --- | --- | --- |
| async 运行时 | `tokio`（features: `rt-multi-thread` `net` `io-util` `time` `signal` `macros` `sync`） | 1.x 最新 | MIT | 事实标准；`windows-service` 需要独立线程跑 SCM dispatcher，与 tokio 混用最省事 |
| WebSocket 服务端 | `tokio-tungstenite` | 0.2x 最新 | MIT/Apache-2.0 | 与 tokio 同族；`Message::Binary/Text/Close` 直接对应线上协议 |
| TLS 客户端 | `tokio-rustls` + `rustls` | 0.2x / 0.23.x | MIT/Apache-2.0 | 993 是 implicit TLS；rustls 无 OpenSSL 依赖，交叉编译/静态链接友好 |
| 根证书 | `rustls-platform-verifier`（首选）/ `webpki-roots`（兜底） | 版本待核 | MIT/Apache-2.0 | **用系统信任库**：企业自建根、用户手动导入的证书都能像系统一样生效；`webpki-roots` 只认 Mozilla 列表 |
| Windows 服务（跑 + 管 SCM） | `windows-service` | 0.8.1 | MIT OR Apache-2.0 | 唯一同时提供「服务入口 + SCM 管理（创建/删除/启停/失败动作）」的 crate |
| macOS / Linux 服务注册 | `service-manager` | 0.11.0 | MIT OR Apache-2.0 | 生成 launchd plist / systemd unit 并调 `launchctl` / `systemctl`；`ServiceLevel::User\|System` 两档 |
| Windows 用户级自启 | **自己实现**（任务计划程序，走 `windows` crate 或 `schtasks /XML`） | —— | —— | 见 § 3.2：HKCU `Run` 会让控制台程序**闪一个黑框**，不能接受 |
| 控制面（类型层） | `serde` + `serde_json` | 1.x | MIT/Apache-2.0 | **只依赖这两个** → `peon-burrow-ipc-types`；桌面端能单独拿类型 |
| 控制面（传输层） | `interprocess` | 2.4.4 | 0BSD OR Apache-2.0 | 一套代码覆盖命名管道与 UDS；`auto` 模式下另留 loopback TCP 退路 → `peon-burrow-ipc` |
| 自更新 | `self_update`（features: `checksums` `signatures` `signature` `async`）+ 自研清单解析 | 1.3.0 | MIT | 校验和 + zipsign 验签 + **替换正在运行的自己**（内部用 `self-replace` 1.5.0） |
| 自我替换（备选/兜底） | `self-replace` | 1.5.0 | Apache-2.0 | 若 `self_update` 的 `ReleaseSource` 不够用（我们要自定义清单），直接用它 + `reqwest` |
| HTTP 客户端 | `reqwest`（`rustls-tls`、`json`、可选 `socks`） | 0.13.x | MIT/Apache-2.0 | 自更新下载、`doctor` 的时间检查 |
| 配置 | `serde` + `toml` | 1.x / 0.9.x | MIT/Apache-2.0 | TOML 好手写、带注释（对齐 `Cargo.toml` 的体验） |
| 平台目录 | `directories` | 6.x | MIT/Apache-2.0 | ⚠️ **只允许出现在 `Paths::discover()` 里**；其余地方一律注入（L4），否则测试只能污染真实用户目录 |
| CLI | `clap`（`derive`） | 4.5.x | MIT/Apache-2.0 | 子命令表见 [`design/config-schema.md § 4`](./design/config-schema.md) |
| 日志 | `tracing` + `tracing-subscriber`（`fmt`、`json`、`env-filter`）+ `tracing-appender` | 0.1.x / 0.3.x / 0.2.x | MIT | 结构化事件名（[`logging-and-diagnostics.md § 2.4`](./design/logging-and-diagnostics.md)） |
| 进程 / 端口信息 | `sysinfo` | 0.3x | MIT | 「谁占着 41316」要给出 PID + 进程名 |
| Windows 事件日志（可选） | `windows-eventlog` 或 `tracing-etw` | 待核 | MIT/Apache-2.0 | 只用于「服务启动/停止/崩溃」这类低频事件 |
| 错误 | `thiserror`（库）/ `anyhow`（bin） | 2.x / 1.x | MIT/Apache-2.0 | 库要给人匹配的类型化错误（`FatalError` vs `TransientError`，见 parity E6） |
| 字节缓冲 | `bytes` | 1.x | MIT | 与 tungstenite 同族；**公开 API 只用 `Bytes`，不出现 `Vec<u8>`**（省一次拷贝，且签名一旦公开就难改） |
| 校验和 / 编码 | `sha2` + `hex`（`base64` 给 token） | 0.10.x / 0.4.x | MIT/Apache-2.0 | 自更新与 `SHA256SUMS` |
| 本地 tag / 版本 | `cargo-bumpp`（本地工具，不进依赖） | 0.4.1 | MIT | 与作者其它仓库一致：`cargo bumpp <level>` 改版本、提交、打 tag、推 tag |

**桌面端（`peon-hall` 仓库）**：`tauri` 2.12.2、`tauri-plugin-shell` 2.4.1、
`tauri-plugin-process` 2.4.0；**不使用** `tauri-plugin-updater`（桌面端不检查更新，见
[`adr-0006`](./decisions/adr-0006-desktop-installer.md)）。

---

## 2. 中继内核：为什么是 tokio + tungstenite + rustls

| 备选 | 否决理由 |
| --- | --- |
| `async-std` + `async-tungstenite` | 生态与 `windows-service` 的示例脱节；没有收益 |
| `hyper` + 手写 WebSocket | WebSocket 帧解析（分片、掩码、关闭握手）自己写等于重造 `tungstenite` |
| `openssl` / `native-tls` | 交叉编译与静态链接的痛苦（尤其 Windows/macOS），而我们需要「单文件二进制」 |
| `webpki-roots` 单一来源 | 只认 Mozilla 根列表：用户导入到系统里的企业根证书**不生效**，而邮件服务器在企业环境里很常见。所以首选 `rustls-platform-verifier`，`webpki-roots` 只作为无法访问系统库时的兜底 |

### 2.1 从 TS 版继承的 TLS 语义（必须一致）

| 语义 | Rust 落点 |
| --- | --- |
| 993 = implicit TLS，连上立刻握手 | `TlsConnector::connect(domain, tcp_stream)` |
| SNI 必须给；**IP 字面量不给** | `ServerName::try_from(host)` 得到 `DnsName` 或 `IpAddress`，IP 时不发 SNI |
| 默认校验链 | 用系统/内置根；`tls_reject_unauthorized = false` 走 `dangerous()` 自定义 verifier（**仅测试**） |
| 自签证书要有可读的错误 | `rustls` 的 `CertificateError`/`Error::InvalidCertificate` 映射成中文提示（parity D8） |
| `tls=0` + 993 → 立刻拒绝 | 策略层拦，不进 TLS 分支（parity C5） |

⚠️ `rustls` 默认后端**不支持 32 位 macOS**。我们不发 32 位 macOS 产物，但要写进 CI 的
「不支持的 target」说明里，免得有人加进矩阵后一头雾水。

---

## 3. 服务化

### 3.1 Windows

**用 `windows-service` 0.8.1**：

- 服务入口：`define_windows_service!` + `service_dispatcher::start` + `service_control_handler::register`；
- 状态上报：必须在 `wait_hint` 内报 `StartPending → Running`，否则 SCM 直接杀掉服务；
- 管理：`service_manager::ServiceManager::{create_service, delete_service, open_service}`；
- 崩溃恢复：`ServiceFailureActions`（重启延时 + 重置周期）——这是「服务崩了能自己回来」的正解。

⚠️ `windows-service` 0.8.1 自身是 **edition 2021 / MSRV 1.71**：
作为 edition 2024 的依赖没问题，但要**钉住版本**，不要指望上游跟着 2024 的 lint 走。

**不要**用 `service-manager` 在 Windows 上装服务：它本质是调 `sc.exe`
（源码里明说「sc.exe 不支持用户级服务」），而且**无法通过 `sc create` 注册失败重启动作**，
还会在检测到 WinSW 时改走 WinSW（一个惊喜依赖）。

### 3.2 Windows 的用户级自启（默认路径）：不用 HKCU `Run`

| 方案 | 问题 |
| --- | --- |
| `HKCU\…\Run`（`auto-launch` 的做法） | 中继是**控制台子系统**程序，登录时会被启动出**一个黑框窗口**；用户视角就是「有个黑窗一直开着」 |
| **任务计划程序（推荐）** | `schtasks /Create /SC ONLOGON /TN burrow /TR "<exe> run" /IT /F`，配 `Hidden`（走 `/XML` 才能设）→ **不闪窗**；且支持「任务失败重启」 |
| 装成系统服务（可选，需提权） | 见 ADR-0003；用户默认路径不选它 |

> 结论：Windows 上**用户级自启用任务计划程序**，`auto-launch` 这个 crate 因此不引入（布局评审 丁1：一个控制台子系统二进制 + 任务计划程序 `Hidden`，不做 GUI 子系统/双 bin）。
> （它在 macOS/Linux 上做的事情，`service-manager` 已经能做，没必要两套机制）。
>
> 待办：`Hidden` 需要 XML 而不是命令行开关，落地时确认最简写法（`schtasks /Create /XML <file>`）。

### 3.3 macOS / Linux

`service-manager` 0.11.0，`ServiceLevel::User` 与 `::System` 两档：

| 平台 | 用户级 | 系统级 |
| --- | --- | --- |
| macOS | `~/Library/LaunchAgents/*.plist` + `launchctl bootstrap gui/<uid>` | `/Library/LaunchDaemons/*.plist` + `launchctl bootstrap system` |
| Linux | `~/.config/systemd/user/*.service` + `systemctl --user enable --now`（无 systemd 时退化为 `~/.config/autostart/*.desktop`） | `/etc/systemd/system/*.service` + `systemctl enable --now` |

崩溃恢复必须显式写上：launchd 的 `KeepAlive`（`{SuccessfulExit: false}`）、
systemd 的 `Restart=always` + `RestartSec=2`。

⚠️ **不要**引入 `systemd` crate（0.10.1）：它是 **LGPL-2.1-or-later WITH GCC-exception**，
与本仓库的 MIT 生态不一致，而且它只提供 `sd_notify`，不负责装 unit。
另外 `linux-systemd` 这个 crate **在 crates.io 上不存在**（别照抄搜索结果）。

---

## 4. 控制面

**用 `interprocess` 2.4.4（本地 socket）+ 可切换的 loopback TCP 退路**，
协议是一行 JSON（[`design/control-plane-ipc.md`](./design/control-plane-ipc.md)）。

| 备选 | 否决理由 |
| --- | --- |
| 只用命名管道 + 手工 DACL | `LocalSystem` 服务与用户进程**跨完整性级别**，DACL 要自己构造，边界情况多；保留为默认，但必须有退路 |
| 只用 loopback TCP | 可用且最简单（非提权 GUI 也能连），但「多一个监听端口」与「本机任何进程都能来敲门（靠 token 挡）」两个缺点都不必要地引入 |
| HTTP（`axum`/`hyper`） | 为了十个命令引入一个 HTTP 栈，收益为负 |
| Windows SCM 控制码 | 非提权进程 `OpenService(SERVICE_STOP)` 会 `ERROR_ACCESS_DENIED`。留给已提权的 CLI/安装器路径 |

> `interprocess` 的 README 自称「passive maintenance」，且带一段反 LLM 声明。
> 这是**采购/许可证备注**，不是技术阻塞（2.4.4 可用）；若将来它停更，
> 换 `tokio::net::windows::named_pipe` + `tokio::net::UnixListener` 两个平台分支即可
> —— `peon-burrow-ipc` 把传输藏在 trait 后面，就是为了这一天。

---

## 5. 自更新

**首选 `self_update` 1.3.0**（features: `checksums`、`signatures`、`signature`、`async`），
配合**我们自己的清单格式**（不走 GitHub API 列表，见 [`design/update-flow.md`](./design/update-flow.md)）。

必须显式设置的项（默认值在守护进程里是**错的**）：

```rust
let status = self_update::backends::github::Update::configure()
    .repo_owner("mail-peon")
    .repo_name("peon-burrow")
    .bin_name("burrow")
    .no_confirm(true)        // ⚠️ 默认 false：会阻塞在 stdin 上问 y/n —— 服务态必死
    .show_output(false)      // ⚠️ 默认 true：往 stdout 打状态块，服务态没人看
    .build()?
    .update()?;
```

| 备选 | 否决理由 |
| --- | --- |
| `axoupdater` 0.10.2 | 它绑定 cargo-dist 的安装回执；而我们对 dist 说不（§ 7），且 dist 在 Windows 上有未关闭的临时目录清理 bug |
| `tuf` / `tough` | 完整 TUF 需要自己运营元数据签名仓库（快照/时间戳轮换），对一个二进制过重；`tuf` 稳定版停在 2017 |
| `reqwest` + 手写替换 | 兜底方案（真要适配一个奇怪镜像时用）。要自己处理原子性、回滚、Windows rename —— 这些正是 `self-replace` 已经写对的部分 |

---

## 6. 发版工具链（core）

| 环节 | 选型 | 说明 |
| --- | --- | --- |
| 本地改版本 / 打 tag | `cargo-bumpp` 0.4.1 | 与作者其它仓库一致：`cargo bumpp <level>`（`conventional` 可读提交历史定级别） |
| CI 触发 | `push: tags: ["v*.*.*"]` | 只有 tag 能发版；没有 `workflow_dispatch`（要重跑就重跑 job） |
| 构建矩阵 | 手写 `assets.yaml`（`workflow_call`） | **不用 cargo-dist**：见 § 7 |
| 资产上传 | `gh release create \|\| true` + `gh release upload --clobber` | 官方 CLI，无第三方 action，天然幂等 |
| 校验和 | 独立的 `checksums` job → 一个 `SHA256SUMS` | 与作者其它仓库一致；含「上传后确认」一步 |
| 自更新清单 | 自研 `latest.json` 生成脚本 | 需要「每平台 URL + sha256 + 签名」，没有现成工具能吐我们的格式 |
| crates.io 发布 | **发 7 个 crate**（`harbor` 编排：preflight 算依赖顺序、「已在 registry 上」视为完成，重跑安全） | 这是库项目，用户要能 `cargo add` 与 `cargo install peon-burrow`；见 [`adr-0009`](./decisions/adr-0009-crates-io-publishing.md) |

---

## 7. 为什么不用 `cargo-dist`

`cargo-dist` 0.32.0 会生成整条矩阵 + 安装器 + 机器可读清单，看起来正好覆盖需求。但它：

1. **与作者现有仓库的约定不一致**（`cargo-bumpp` / `harbor` / `bmux-cli` / `crate-plugin-kit`
   全是手写 `ci.yaml` + `release.yaml` + `assets.yaml`），引入它等于仓库间两套风格；
2. 我们有两步它管不了：**自更新清单要带 zipsign 签名**、**桌面端要跨仓库取二进制**；
3. 它有一个**未关闭的 Windows 临时目录清理 bug**（[cargo-dist#1374](https://github.com/axodotdev/cargo-dist/issues/1374)），
   而 Windows 是我们的一等公民。

同理不用 `release-plz`（版本/CHANGELOG 工具，不产资产）与 `cargo-release`
（只做打 tag；`cargo-bumpp` 已经覆盖）。

> **没有 CHANGELOG 文件**，这是作者生态的一贯做法（提交信息用 Conventional Commits，
> Release notes 由 tag 与 PR 描述构成）。本仓库沿用：**不引入 changelog 生成器**。

---

## 8. 风险与待决

| # | 风险 / 待决 | 处理 |
| --- | --- | --- |
| 1 | `windows-service` 是 edition 2021 / MSRV 1.71 | 钉版本；MSRV 取两者较大值 |
| 2 | **没有任何 crate 负责「更新后重启服务」** | 这是本项目自研风险最高的一段代码，方案见 [`design/update-flow.md`](./design/update-flow.md)；用 SCM failure actions + `--apply-update` 辅助进程兜底 |
| 3 | Windows 文件锁：安装目录里任何被加载的 DLL 都会阻止替换 | 中继保持**单一自包含 exe**；`peon-burrow-update` 失败时保留原二进制并上报 |
| 4 | `self_update` 会读 `GH_TOKEN` / `GITHUB_TOKEN`：**过期的 token 会把原本正常的匿名检查变成失败** | 不读环境 token；需要时只从配置里读 |
| 5 | 系统服务不继承用户的代理环境变量 | 代理配置从 `relay.toml` 读；文档里对「镜像站」给出完整 URL 写法 |
| 6 | `interprocess` 维护状态被动 | 传输已抽象；替换成本 = 两个平台分支 |
| 7 | `rustls-platform-verifier` 版本未核 | 落地时核一遍；不可用则退 `webpki-roots` 并在 `doctor` 里说明「只认 Mozilla 根」 |
| 8 | `tracing-appender` 的滚动是「按天 / 按大小」二选一 | 我们的策略是「大小为主 + 跨天也滚」，落地时确认实现方式（可能是自己写一个 rolling 封装） |
