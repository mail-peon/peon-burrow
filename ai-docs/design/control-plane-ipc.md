# 设计 · 控制面（GUI/CLI ↔ 服务）

> 这条通道**与线上协议完全无关**：它只服务「本机的管理动作」。
> 扩展永远不碰它，中继转发邮件也永远不走它。

---

## 1. 它要回答两个问题

> 📦 **分层**：所有**类型**（`Request` / `Response` / `StatusReport` / `ServiceStatus`）在
> `peon-burrow-ipc-types`（只依赖 `serde`，是对外契约）；**传输**（本地 socket / TCP 退路 / 客户端 / 服务端）
> 在 `peon-burrow-ipc`。桌面端两个都依赖（它要客户端），`core`/`service` 只依赖类型层；控制面的**服务端接线**在产品 crate `peon-burrow`（`control.rs`）。

| 问题 | 谁问 | 现状（TS 版） |
| --- | --- | --- |
| **「你活着吗？在哪个端口？什么版本？」** | 安装器 GUI、`doctor`、更新器 | 只能靠「端口连得上吗」猜，且**无法区分**「是我自己的旧实例」还是「别的进程占着」 |
| **「停一下 / 重启一下」** | 安装器 GUI（用户点按钮）、自更新 | 只能 `kill` 进程；TS 版靠 TTY 里敲 `q`/`r`（服务态根本没有 TTY） |

---

## 2. 为什么不是 HTTP

| 方案 | 否决理由 |
| --- | --- |
| **localhost HTTP（又一个端口）** | 中继的价值之一是「只占一个端口」。再加一个控制端口 = 多一个冲突面、多一条防火墙规则、多一份发现文件（端口 A 的扩展要连端口 B） |
| HTTP over 命名管道 | 有（`http+docker` 之类），但为了「一行 JSON」引入一个 HTTP 实现不值得 |
| **命名管道 / Unix socket** ✅ | 零端口占用、权限由 OS 的 ACL / 文件权限管、连不上就等价于「服务没在跑」（天然的健康检查） |
| Windows SCM 控制码（`SERVICE_CONTROL_STOP`） | 只能启停，拿不到「端口/版本/状态」；而且要提权。作为**服务启停的实现手段**保留，不作为 GUI 的接口 |

### 2.1 通道

| 平台 | 类型 | 名字 / 路径 |
| --- | --- | --- |
| Windows | 命名管道 | `\\.\pipe\peon-burrow-<user>`（用户级）/ `\\.\pipe\peon-burrow-system`（系统服务） |
| macOS / Linux | Unix domain socket | 用户级：`$XDG_RUNTIME_DIR/peon-burrow.sock` 或 `~/Library/Application Support/peon-burrow/relay.sock`；系统服务：`/run/peon-burrow.sock` |

实现：`interprocess` crate（本地 socket 抽象同时覆盖两者）或平台分支
（Windows 用 `tokio::net::windows::named_pipe`，Unix 用 `tokio::net::UnixListener`）。
选型与版本见 [`02-tech-stack.md`](../02-tech-stack.md)。

**用户级 vs 系统服务的 ACL**：

| 模式 | 运行身份 | 谁能连 |
| --- | --- | --- |
| 用户级自启（默认） | 当前用户 | 只有该用户（管道默认 DACL / socket 文件 0600） |
| 系统服务 | `LocalSystem` / `root` | Windows：管道 DACL 显式授予 `Authenticated Users` 读写（否则 GUI 连不上）；Unix：socket 属主设 `root`、组设 `peon-burrow` 或 `0600` + 允许 `sudo` 场景 |

⚠️ 系统服务模式下**不能**把管道权限放成 Everyone；必须是「已认证用户」，
并且命令集里**没有**任何能读取凭据或放宽监听地址的操作（见 § 5）。

---

## 3. 帧格式

一行 JSON 请求 → 一行 JSON 响应（`\n` 结尾），**一问一答，答完即断**。
无长连接、无服务端推送、无流式 —— 服务端 `Status` 的实时性靠 GUI 自己轮询（2 秒一次足够）。

请求：

```json
{"v":1,"id":"1","cmd":"status","token":"<32 字节随机的 base64>"}
```

响应：

```json
{"v":1,"id":"1","ok":true,"result":{"running":true,"port":41316,"version":"0.1.0","connections":1}}
```

失败：

```json
{"v":1,"id":"1","ok":false,"error":{"code":"unauthorized","message":"token 不匹配"}}
```

| 字段 | 说明 |
| --- | --- |
| `v` | 控制面协议版本（常量 `IPC_PROTOCOL_VERSION`）。中继收到**更高**的 `v` → `error.code = "protocol-too-new"`（GUI 提示「中继版本过旧」）。⚠️ 扩展侧那套叫 `WATCH_PROTOCOL_VERSION`，两者别混（见 [`wire-protocol.md § 5`](./wire-protocol.md)） |
| `id` | 客户端自增，响应原样回显（便于将来做并发；当前一问一答，仅用于日志关联） |
| `token` | 见 § 4 |
| 长度上限 | 单行 **≤ 8 KiB**；超限直接断开（防止一个本机进程把内存喂爆） |

---

## 4. 鉴权

1. 中继启动时生成 **32 字节随机 token**（CSPRNG），写入
   `<data-dir>/control.json`（Windows 同目录 ACL 限当前用户；Unix `0600`），内容：

   ```json
   { "schema": 1, "token": "…", "kind": "named-pipe", "path": "\\\\.\\pipe\\peon-burrow-imba97", "pid": 12345 }
   ```

2. GUI / CLI 读该文件拿 token + 通道名，再连。
3. 中继对每个连接**先校验 token**，失败 → `error.code = "unauthorized"` 并**立即断开**；
   连续 5 次失败 → 该通道**停止接受新连接 30 秒**（防本机暴力猜）。
4. token 在每次重启后**轮换**（发现文件里的 `control` 段只是缓存，读 `control.json` 才是正路）。

> 威胁模型：**本机其它用户 / 低权限进程**。
> 同用户下的恶意进程本来就"什么都能做"（能读扩展的 profile、能读配置文件），
> 控制面不为这一层提供额外保护；它防的是**跨用户**与**网络**（后者根本连不到）。

---

## 5. 命令集（**白名单，只有这些**）

| `cmd` | 参数 | 返回 | 谁用 |
| --- | --- | --- | --- |
| `ping` | — | `{pong:true}` | 存活探测（比 `status` 轻） |
| `status` | — | `StatusReport { process: ProcessStatus, service: ServiceStatus }` —— **两个权威来源**：进程内状态（`RelayState`）+ 服务注册状态（服务管理器）；类型定义在 `peon-burrow-ipc-types` | GUI 状态卡片（四种组合靠它区分） |
| `version` | — | `{version, gitSha, buildTime, protocol}` | GUI 「关于」、更新器 |
| `doctor` | `{verbose?}` | 结构化自检结果（见 [`logging-and-diagnostics.md`](./logging-and-diagnostics.md)） | GUI「诊断」按钮 |
| `stop` | `{reason?}` | `{stopping:true}` | GUI「停止服务」、自更新 |
| `restart` | — | `{restarting:true}` | GUI「重启服务」 |
| `updateCheck` | `{force?}` | `{available, current, latest, notesUrl, assetName}` | GUI 显示「有新版本」 |
| `updateApply` | — | `{started:true}` | GUI「立即更新」（见 [`update-flow.md`](./update-flow.md)） |
| `traceOn` | `{seconds}`（上限 300） | `{until}` | 排查「连不上但看不出原因」 |
| `traceOff` | — | `{ok:true}` | 同上 |

**明确不在命令集里的东西**（写进测试，防后人顺手加）：

| 不做 | 理由 |
| --- | --- |
| 读/改凭据、列账号 | 中继只在内存里转发凭据，**从不落盘**；一旦提供读取接口就等于自建凭据库 |
| 改监听地址 / 端口 | 改端口要重启，且会让扩展的配置失配 —— 走配置文件，且 GUI 只提供「打开配置文件」 |
| 执行任意命令 / 任意文件路径读写 | 控制面不是 shell；`updateApply` 的下载地址也不接受客户端传入 |
| 杀任意 PID | 见 [`port-and-discovery.md § 5.3`](./port-and-discovery.md) |
| 安装 / 卸载服务 | **提权操作**，由 GUI 调用 core 二进制的 `service install` 子命令完成（§ 6） |

---

## 6. 提权操作的路径（安装 / 卸载 / 自启开关）

GUI **不实现**服务注册，它只是 driver：

```
用户点「安装服务」
  └─ GUI 以提权方式 spawn（macOS/Linux: pkexec/sudo；Windows: UAC runas）
       burrow service install --autostart logon --core-path <安装目录> \
         --desktop-pid <pid>          # 用于「GUI 退出后是否保留服务」这类判断
     └─ 子进程：写注册项 → 启动服务 → 等待控制面可连 → 输出一行 JSON 结果 → 退出
  └─ GUI 读那一行 JSON，刷新状态卡片
```

| 平台 | 提权方式 | 注册机制 |
| --- | --- | --- |
| Windows（用户级，默认） | **不提权** | 任务计划程序 `schtasks /sc onlogon` 或 `HKCU\…\Run` |
| Windows（系统服务，可选） | UAC | SCM（`windows-service` crate 的安装器） |
| macOS（用户级，默认） | 不提权 | `~/Library/LaunchAgents/*.plist`（`launchctl bootstrap`） |
| macOS（系统服务，可选） | `osascript … with administrator privileges` / `SMAppService`（13+） | `/Library/LaunchDaemons/*.plist` |
| Linux（用户级，默认） | 不提权 | `systemd --user` unit 或 `~/.config/autostart/*.desktop` |
| Linux（系统服务，可选） | `pkexec` | `/etc/systemd/system/*.service` + `systemctl enable` |

**为什么默认不提权**：中继只监听 `127.0.0.1`，与用户登录绑定完全够用；
而提权会让安装过程多一个 UAC/`sudo` 弹窗 —— 那正是「装一次就不用管」要避免的摩擦
（原有论证见 `mail-peon/ai-docs/decisions/relay-deployment.md § 2.1`）。

---

## 7. 幂等与并发

| 场景 | 期望行为 |
| --- | --- |
| `stop` 时服务本来就快停了 | `{stopping:true}`，不报错 |
| 两个 GUI 同时 `restart` | 第二次合并进第一次（用一把异步锁，后到的回 `{restarting:true}`） |
| `updateApply` 时正在 `updateApply` | 回 `error.code = "busy"` |
| GUI 在 `stop` 后立刻查 `status` | 允许短暂返回 `running:false` 或连不上（GUI 要把「连不上」渲染成「已停止」而不是「错误」） |
| 服务收到 `stop` 而**没有**任何 watchdog | 直接退出；用户级自启的语义是「登录时启动」，不是「永远拉活」 |

---

## 8. 与「运行状态 / 安装状态」的对应

GUI 的状态卡片是**两个来源**拼出来的：

```
        ┌─────────────────────────── 服务管理器（注册与自启状态，权威）
        │                              Windows SCM / launchd / systemd
        │                              → installed? autostart? serviceState?
        │
状态卡 ─┤
        │
        └─────────────────────────── 控制面（进程内真实状态，权威）
                                       → running? port? version? connections?
                                       连不上 = 工具在，但服务没跑
```

四种可区分的组合（TS 版**完全无法区分**这些）：

| 已注册 | 控制面可连 | 界面文案 | 主按钮 |
| --- | --- | --- | --- |
| ✅ | ✅ | 正在运行 · 端口 41316 · v0.1.0 | 停止 / 重启 |
| ✅ | ❌ | 已安装，**未运行**（可能正在重启） | 启动 |
| ❌ | ✅ | **前台运行中**（未安装为服务） | 安装服务 |
| ❌ | ❌ | 未安装 | 安装服务 |

---

## 9. 验收

| # | 断言 |
| --- | --- |
| 1 | 无 token / 错 token → `unauthorized`，且不影响中继继续收信 |
| 2 | 未知 `cmd` → `unknown-command`，不 panic、不断开服务 |
| 3 | 单行 > 8 KiB → 断开，进程存活 |
| 4 | `stop` 后进程在 3 秒内退出，退出码 0；正在挂 IDLE 的连接不会把关停卡住 |
| 5 | `restart` 后端口不变、发现文件刷新、版本不变 |
| 6 | 连发 5 次错 token → 后续连接被拒 30 秒（防猜） |
| 7 | 系统服务模式下，**另一个用户**连不上（Windows/Unix 都要测） |
| 8 | 控制面命令集合与本文档 § 5 白名单**逐条一致**（测试里写死枚举，加命令必须改测试） |
| 9 | GUI 四种状态组合都能正确渲染（见 § 8） |
| 10 | `doctor` 在「端口被别进程占用」时给出的 PID/进程名正确 |
