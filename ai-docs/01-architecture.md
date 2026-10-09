# 01 · 架构

> 读完这篇应该能回答：**有几个进程、谁管谁、代码放哪、产物怎么长出来**。
> 具体选型理由在 [`02-tech-stack.md`](./02-tech-stack.md) 与各 ADR，这里只讲结构与不变量。

---

## 1. 运行时拓扑

### 1.1 稳态（服务已安装）

```
┌──────────────────────────────────────────────────────────────────────────┐
│ 用户机器                                                                  │
│                                                                          │
│  ┌───────────────────┐   ① ws://127.0.0.1:41316/<host>:<port>?tls=1      │
│  │ mail-peon 扩展     │   ────────────────────────────────────────┐      │
│  │ (浏览器 MV3)       │   ② ws 首帧 {"__watch":1,…} → watch 模式 │      │
│  │                   │   ◀──── {"type":"mail","exists":N} ────────┤      │
│  └───────────────────┘                                           ▼      │
│                                                        ┌──────────────────┐
│                                                        │ burrow  │
│                                                        │ (服务进程)        │
│                                                        │  · WS 服务器      │
│                                                        │  · 每连接一条 TCP  │
│                                                        │  · watch: IDLE    │
│                                                        │  · 控制面监听      │
│                                                        └────┬─────────┬───┘
│                                                             │         │
│                          ③ TLS(SNI, 校验证书链)              │         │ ④ 控制面
│                                                             ▼         │ (命名管道/UDS)
│                                                   imap.x.com:993      │
│                                                                       ▼
│                                                        ┌──────────────────────┐
│                                                        │ desktop (Tauri GUI)   │
│                                                        │ 状态 · 装/卸 · 启/停  │
│                                                        └──────────────────────┘
└──────────────────────────────────────────────────────────────────────────┘
```

要点：

- **中继只监听 loopback。** 监听地址固定可配但默认 `127.0.0.1`，服务形态不提供「一键对外」。
- **一条 WebSocket 对应一条 TCP，永不复用。** IMAP 是有状态协议，复用 = 串话。
  多账号 = 多条并发 WebSocket（扩展侧 `watch.ts` 每账号一条）。
- **控制面是另一条通道**，不共用中继的端口，也不共用线上协议。
  GUI 关掉、崩溃、被杀，都不影响中继收信。

### 1.2 调试态（前台跑）

```
终端 → burrow run --foreground   # 前台跑，日志进 stdout，Ctrl+C 退出
       burrow run --port 41317   # 换端口
```

前台模式**不注册服务、不写自启、不碰系统状态**（省得调试把用户环境搞脏）。

### 1.3 一次性/无服务形态（保留）

扩展侧只要求「有个进程在 `127.0.0.1:<port>` 上提供服务」。所以也允许：

```
burrow run   # 直接前台常驻，用户自己拿别的方式保活（比如 Windows 启动目录放个快捷方式）
```

这条路的体验差（无自启、无状态显示），但**必须能跑通**：它是排查
「是服务层的问题还是中继本身的问题」的分界线。TS 版就是这个形态。

---

## 2. 组件划分

### 2.1 本仓库（`peon-burrow`，Rust workspace）

> 本仓库在 GitHub 上是独立的 **`mail-peon/peon-burrow`**；
> 本地与桌面端仓库**并排**放在同一个父目录里（父目录本身不是仓库，见 [adr-0001](./decisions/adr-0001-two-repos.md)）。

```
(peon-burrow/)        # ← 本仓库根 = Cargo workspace 根
├── Cargo.toml                      # workspace（resolver = "3"，edition 2024，rust-version = MSRV）
├── README.md
├── LICENSE
├── ai-docs/                        # 本仓库文档（含协议、ADR）
├── .github/workflows/
└── crates/
    ├── peon-burrow-protocol/             # 稳定：线上协议（目标解析 · watch 报文 · 关闭码）无 IO
    ├── peon-burrow-core/                 # 稳定：引擎（隧道 + 策略 + RelayState）；feature imap-watch 默认关
    ├── peon-burrow-ipc-types/            # 稳定：控制面**类型**（只依赖 serde）
    ├── peon-burrow-ipc/                  # 稳定：控制面**传输** + 客户端/服务端
    ├── peon-burrow-service/              # 稳定：跨平台服务托管（与中继无关，可单独用）
    ├── peon-burrow-update/               # 稳定：自更新（与中继无关，可单独用）
    ├── peon-burrow/                      # 产品：lib（config/doctor/control/exit/run）+ [[bin]] burrow
    ├── peon-burrow-testkit/              # 内部：echo / mock IMAP / harness（publish = false）
    └── peon-burrow-examples/             # 内部：examples/ 里的 7 个接入示例（publish = false）
```

⚠️ **没有 `rust-toolchain.toml`**，这是刻意的（沿用 `cargo-bumpp` / `harbor` 的约定）：
钉死 channel 会让 CI 里的 MSRV 任务失效。MSRV 写在 `Cargo.toml` 的 `rust-version`，
由 CI 里一个专门的 MSRV job 负责验证。

依赖方向（不许反向）：

```
peon-burrow（lib + bin `burrow`）
├── peon-burrow-core ──▶ peon-burrow-protocol        ← 引擎只依赖协议
├── peon-burrow-ipc  ──▶ peon-burrow-ipc-types       ← 控制面传输 vs 类型
├── peon-burrow-service                              ← 与中继无关
└── peon-burrow-update                               ← 与中继无关

布局铁律（违反就是返工，详见 modules.md 开头）：
  L1 稳定层不许依赖产品层 · L2 类型与传输分开 · L3 默认值只在 core · L4 注入而非全局 · L5 bin 只有 40 行
```

`peon-burrow-core` **不依赖**配置来源、不依赖 `std::env`、不读文件：它的输入是一个
`RelayOptions` 值，输出是 `RelayServer`（`start()` / `stop()` / `local_addr()` / `state()`），默认值也只在 core 定义（L3）。
这条纪律是从 TS 版学来的：TS 版把 `PORT` / `ALLOWED_HOSTS` 都写成了模块级常量
（`imap-relay.ts:90-140`），导致**没法在一个进程里跑两个实例**，
测试只能 `spawn` 子进程 + `sleep(1200)` 等它起来（`imap-relay.test.ts:122`）——
Rust 版要能 `#[tokio::test]` 里直接起两个实例。

### 2.2 姊妹仓库（`peon-hall`，Tauri 2）

**不在本仓库里**。GitHub：`mail-peon/peon-hall`。
本地默认并排：`<父目录>/peon-burrow` 与 `<父目录>/peon-hall`。

```
(peon-hall/)     # ← 独立 git 仓库、独立 CI
├── package.json / vite.config.*    # 前端（轻量，见该仓库 ai-docs）
├── src/                            # 前端：状态卡片 + 5 个操作按钮
├── src-tauri/
│   ├── Cargo.toml                  # 首次发布前：git 依赖钉 tag；发布后：peon-burrow-ipc = "0.1" 版本依赖
│   ├── tauri.conf.json             # bundle 目标、externalBin(sidecar)、identifier
│   ├── capabilities/               # Tauri 2 权限声明
│   ├── binaries/                   # 打包时放进来的 core 二进制（不进 git）
│   └── src/                        # 命令实现：调 peon-burrow-ipc + 调 sidecar 做提权操作
└── ai-docs/
```

**桌面端不实现服务注册逻辑**，它只做两件事：

1. 通过 `peon-burrow-ipc` **读状态、发控制命令**（停止、启动、重启、查版本/端口）；
2. 需要提权的操作（安装/卸载系统服务、写 `/Library/LaunchDaemons`、`systemctl enable`）
   **以提权方式调用 core 二进制**的 `service install` / `service uninstall` 子命令。

> ⚠️ 为什么不让 GUI 自己写注册表 / plist / unit 文件：
> 服务注册的细节（SCM 句柄、`launchd` 的 `KeepAlive`、systemd 的 `Restart=`）
> 是**中继自己**必须知道的东西（它要能自更新、要能自己重启），
> 写两份必然漂移。GUI 只当 driver，注册逻辑唯一实现在 `peon-burrow-service`。

**跨仓库的类型共享**（两仓库拆分带来的唯一硬问题）：见
[adr-0001 § 决策 4](./decisions/adr-0001-two-repos.md) ——
类型用 **`peon-burrow-ipc-types`**（发布后是版本依赖，发布前是 git 依赖钉 tag），而不是复制一份类型 —— 见 [`adr-0009`](./decisions/adr-0009-crates-io-publishing.md)。

---

## 3. 数据流

### 3.1 透传（一轮同步）

```
扩展                                   中继                                      邮件服务器
 │  WS 升级 /imap.qq.com:993?tls=1&token=… │                                          │
 ├───────────────────────────────────────▶│ ① URL 解析 → RelayTarget                 │
 │                                        │ ② 策略检查（token / 白名单 / tls=0+993）  │
 │                                        │ ③ 立刻建 TCP/TLS（不等第一帧）───────────▶│
 │  A0001 LOGIN …（二进制帧）               │                                          │
 ├───────────────────────────────────────▶│ ④ 原样写入 socket ──────────────────────▶│
 │                                        │ ◀────────────── 响应字节 ─────────────────│
 │  ◀────────────── 原样回传（含背压）──────┤ ⑤                                        │
 │  ws.close()                            │ ⑥ 关闭策略：任一方向关闭 → 关另一边         │
 └───────────────────────────────────────▶│                                          │
```

三个**不得退化**的行为（TS 版各踩过一次，见 [`04-parity`](./04-parity-node-to-rust.md)）：

- ③ **必须立刻建连**，不能等第一帧：有些用法一个字节都不发，只等连接结果。
- ② **策略检查必须同步**：被拒的连接不发字节，任何「等消息再决定」都会变成「静默放行」。
- ④ 第一帧被分流逻辑扣下过，透传路径必须**补投**，否则客户端的第一个 IMAP 命令丢失。

### 3.2 watch（常驻监听）

```
扩展                                    中继                                       邮件服务器
 │  文本帧 {"__watch":1,host,port,tls,user,pass,accountId,token}                     │
 ├──────────────────────────────────────▶│ 校验 → 关掉预备的 TCP → 进入 watch        │
 │                                       │  TCP/TLS 连接 ──────────────────────────▶│
 │                                       │ ◀── * OK [CAPABILITY …] ready（未标记）   │
 │                                       │  A0001 LOGIN …  ────────────────────────▶│
 │                                       │  A0002 SELECT INBOX ────────────────────▶│
 │  ◀── {"state":"watching","exists":N} ──┤  基准 = SELECT 时的 EXISTS                │
 │                                       │  A0003 IDLE / <等待>                      │
 │                                       │ ◀── * N EXISTS（服务端主动推）             │
 │  ◀── {"type":"mail","exists":N} ───────┤  只在 N 变大时推                          │
 │                                       │  每 25 分钟 DONE → 重新 IDLE              │
 │  （连接断开）                           │  ◀── 错误 / 断开                          │
 │  ◀── {"state":"reconnecting",…} ───────┤  退避重连（致命错误则不重试）              │
```

**推送里不带邮件内容**，只带「有几封」。扩展被推醒后走**普通的一轮同步**
（透传模式抓游标之后的邮件）—— 这条设计让扩展侧只有一条抓取路径，
也顺带保证了「中继不知道邮件内容」。

---

## 4. 控制面（GUI ↔ 服务）

**主通道是本地 socket，不是 TCP 端口**：

| 平台 | 通道 | 路径 / 名称 |
| --- | --- | --- |
| Windows | 命名管道 | `\\.\pipe\peon-burrow-<user>`（用户级）/ `\\.\pipe\peon-burrow-system`（系统服务） |
| macOS / Linux | Unix domain socket | 用户级：`$XDG_RUNTIME_DIR/peon-burrow.sock` 或 `~/Library/Application Support/peon-burrow/relay.sock`；系统服务：`/run/peon-burrow.sock` |

- **类型**在 `peon-burrow-ipc-types` 里定义（`Request` / `Response` / `ServiceStatus`），**传输**在 `peon-burrow-ipc`：
  GUI 与 CLI 共用同一份类型 —— 手写一份镜像类型必然漂移。
- 协议：一行 JSON 请求 → 一行 JSON 响应（`\n` 分隔），连接即断。无长连接、无推送。
- **状态有两个权威来源**：进程内状态（`RelayState`）与**服务注册状态**（`ServiceStatus`）；`status` 返回两者（`StatusReport`），
  GUI 靠它能区分「未安装 / 已安装未运行 / 前台运行 / 运行中」四种组合。
- 鉴权：socket 文件权限 0600（Unix）+ 随机 token（启动时写到只有当前用户可读的文件里）。

**⚠️ 系统服务模式的例外（保留一条 TCP 退路）**：

`LocalSystem` 跑的服务与普通用户进程之间**跨完整性级别**，
命名管道的 DACL 要手工构造（`interprocess` 的 `SecurityDescriptor` 或裸 `SECURITY_ATTRIBUTES`），
边界情况多且难测。所以控制面在 `peon-burrow-ipc` 里做成**传输可切换**：

| `[control] transport` | 行为 | 何时用 |
| --- | --- | --- |
| `auto`（默认） | 用户级 → 本地 socket；系统服务 → 本地 socket **+** loopback TCP | 都能用，GUI 自己挑 |
| `socket` | 只开本地 socket | 管理员确认 GUI 与服务同用户 |
| `tcp` | 只在 `127.0.0.1:<内核分配端口>` 上监听，端口与 token 写进 `control.json` | 排查 / 兼容性兜底 |

TCP 退路的代价是「多一个监听端口」，但它是**内核分配的临时端口**（不是 41316），
与中继端口不冲突，且只绑 loopback（Windows 上绑 loopback 不会弹防火墙授权框）。

> ⚠️ **不能**用 Windows SCM 控制码来实现「GUI 让服务停下」：
> `OpenService(SERVICE_STOP)` 在默认配置下需要提权，非提权的 GUI 会拿到
> `ERROR_ACCESS_DENIED`。SCM 控制码只留给**已经提权的 CLI / 安装器路径**。

- 命令集见 [`design/control-plane-ipc.md`](./design/control-plane-ipc.md)：
  `ping` / `status` / `version` / `doctor` / `stop` / `restart` / `updateCheck` /
  `updateApply` / `traceOn` / `traceOff`。

---

## 5. 不变量（写代码时不许违反）

| # | 不变量 | 违反的后果 |
| --- | --- | --- |
| I1 | 一条 WebSocket ↔ 一条 TCP，不复用 | IMAP 串话，表现为「收到别人的邮件」 |
| I2 | 默认只绑 `127.0.0.1` | 中继能看到明文凭据，暴露 = 交出邮箱 |
| I3 | 策略检查在建连之前、且同步完成 | 「静默放行」：安全检查最不该有的失败方式 |
| I4 | 被拒绝的连接**不发任何字节** | 同上 |
| I5 | 关闭帧原因 ≤ 120 **字节**（UTF-8 安全截断） | `ws.close()` 抛异常 → 整个服务崩（TS 版真崩过） |
| I6 | 单条连接的异常绝不导致进程退出 | 用户侧表现为「突然所有账号都收不到邮件」，且无任何提示 |
| I7 | 停服/重启时**先主动断开**所有连接再 `close()` | IMAP 的 IDLE 会把关停卡住十几分钟 |
| I8 | 凭据永不进日志（除非显式开 trace 且二次确认） | 泄露邮箱授权码 |
| I9 | 服务模式没有 TTY 依赖 | 服务在无终端环境下挂死或被杀 |
| I10 | 自更新的产物必须验签/校验和 | 一个能看到邮箱密码的进程被投毒 |
| I11 | core 内禁止 `std::sync::Mutex`；**每个 `await` 必须有超时或取消源** | 锁跨 `await`、无取消源的读循环 → 关停卡住、偶发不退出，事后改造代价极高 |
| I12 | `Paths` / 时钟 / 进程探测一律**注入**，不许直接调 `directories` | 测试只能污染真实用户目录 → 必然大重构 |

---

## 6. 配置与状态文件位置

| 内容 | Windows | macOS | Linux |
| --- | --- | --- | --- |
| 配置 | `%APPDATA%\peon-burrow\relay.toml` | `~/Library/Application Support/peon-burrow/relay.toml` | `~/.config/peon-burrow/relay.toml` |
| 日志 | `%LOCALAPPDATA%\peon-burrow\logs\relay.log` | `~/Library/Logs/peon-burrow/relay.log` | `~/.local/state/peon-burrow/logs/relay.log` |
| 发现文件（实际端口/版本/PID） | `%LOCALAPPDATA%\peon-burrow\relay.json` | `~/Library/Application Support/peon-burrow/relay.json` | `~/.local/state/peon-burrow/relay.json` |
| 控制面句柄 / token | `%LOCALAPPDATA%\peon-burrow\control.json` | 同上 | 同上 |
| 二进制安装位置（服务模式） | `%LOCALAPPDATA%\Programs\peon-burrow\`（用户级） / `%ProgramFiles%\peon-burrow\`（系统服务） | `~/Library/Application Support/peon-burrow/bin/` / `/usr/local/libexec/` | `~/.local/share/peon-burrow/bin/` / `/usr/local/libexec/` |

目录职责分离的理由：**配置要能被用户备份/手改**（`APPDATA`），
**日志与运行时状态不该被云同步带走来带去**（`LOCALAPPDATA` / `Logs` / `state`）。

---

## 7. 产物矩阵

### 7.1 本仓库（core）产出

| 产物 | 平台 / 架构 |
| --- | --- |
| `peon-burrow-x86_64-pc-windows-msvc.zip` | win x64 |
| `peon-burrow-aarch64-pc-windows-msvc.zip` | win arm64 |
| `peon-burrow-x86_64-apple-darwin.tar.gz` | mac x64 |
| `peon-burrow-aarch64-apple-darwin.tar.gz` | mac arm64 |
| `peon-burrow-x86_64-unknown-linux-gnu.tar.gz` | linux x64 |
| `peon-burrow-aarch64-unknown-linux-gnu.tar.gz` | linux arm64 |
| `latest.json` | 自更新清单（版本、各 triple 的 URL + sha256 + 签名） |
| `SHA256SUMS` | 全部资产的校验和 |

> 归档命名沿用作者其它 Rust 项目的约定：`{crate}-{target}.tar.gz` / `.zip`，
> **不带版本号**（为了兼容 `cargo-binstall` 的默认命名），二进制与 `LICENSE` 放在归档根。
> macOS **分架构构建，不做 universal**（同 `cargo-bumpp` / `harbor` 的矩阵）。

**三个名字不要混**：

| 名字 | 值 | 用在哪 |
| --- | --- | --- |
| 仓库 / crate | `peon-burrow` | GitHub 仓库名、`Cargo.toml` 的包名、归档名前缀 |
| 二进制 | `burrow` | 用户敲的命令、服务里跑的进程、归档里的文件名 |
| 桌面端 sidecar 文件 | `burrow-<triple>[.exe]` | Tauri `externalBin` 要求「配置名 + target triple」 |

### 7.2 姊妹仓库（desktop）产出

| 产物 | 平台 |
| --- | --- |
| `peon-hall_<ver>_x64_en-US.msi` / `…-setup.exe` | win x64 |
| `peon-hall_<ver>_x64.dmg` / `_aarch64.dmg` | mac |
| `peon-hall_<ver>_amd64.AppImage` / `.deb` | linux |

**版本绑定规则**：桌面端发版时从 **peon-burrow 仓库的 Release** 下载指定 tag 的二进制（crates.io 不提供预编译产物）
（默认取 core 最近一个稳定 release，可用 workflow 输入 `core_ref` 钉死），
放进 `src-tauri/binaries/` 供 `externalBin` 打包；同时把该 core 版本写进
桌面端的 release notes 与安装包元数据 —— 避免出现「GUI 与 core 版本不匹配」这种没法复现的现场。

详见 [`05-release-and-versioning.md`](./05-release-and-versioning.md)。

---

## 7.3 发布到 crates.io

7 个 crate 发布（`testkit` / `examples` 不发），顺序与编排见
[`adr-0009`](./decisions/adr-0009-crates-io-publishing.md)：`protocol → core → ipc-types → ipc → service → update → peon-burrow`。
用户安装：`cargo install peon-burrow` → `burrow`（binstall 元数据见 [`modules.md § 7`](./modules.md)）。

---

## 8. 版本协商（计划）

常量 `WATCH_PROTOCOL_VERSION` 定义在 `peon-burrow-protocol`（与控制面的 `IPC_PROTOCOL_VERSION` 分开命名）。watch 请求目前**没有**版本字段（TS 版如此）。计划：扩展在 `__watch:1` 请求里加
`clientVersion` / `protocol: 2`，中继在 `state:"watching"` 里回 `relayVersion` / `protocol`。

- **向后兼容**：中继把「缺少 `protocol`」视为 `1`，行为与 TS 版完全一致；
- 扩展发现 `protocol` 高于自己支持的上限时，提示「中继版本过新，请更新扩展」；
- 中继发现扩展要求的 `protocol` 高于自己实现的上限时，回
  `{"state":"failed","error":"…需要更新中继"}` —— 复用现有失败通道，
  不新增关闭码语义。

> 这条属于**协议扩展**，必须先在 mail-peon 仓库落地并回归，见
> [`wire-protocol.md § 5`](./design/wire-protocol.md)。
