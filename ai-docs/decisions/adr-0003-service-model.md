# ADR-0003 · 服务模型：默认用户级自启，可选系统服务

- **状态**：已采纳
- **影响面**：`peon-burrow-service`、安装器交互、提权路径、崩溃恢复、端口占用处理、自更新
- **相关**：[`02-tech-stack.md § 3`](../02-tech-stack.md)、[`service-lifecycle.md`](../service-lifecycle.md)、[`adr-0006`](./adr-0006-desktop-installer.md)

---

## 背景

中继必须**常驻**：它替浏览器扩展挂着 IMAP `IDLE`，进程没了 = 收不到实时推送
（扩展还有低频兜底轮询，但「实时」名不副实）。

现状（TS 版）：用户自己敲 `pnpm relay`，终端一关就没了。
`mail-peon/ai-docs/decisions/relay-deployment.md § 2.1` 已经定过方向：
**用户级自启，不做系统服务**，理由是「中继只监听 `127.0.0.1`，跟登录绑定完全够用；
装成系统服务要管理员权限，会让安装过程多一个 UAC/`sudo` 弹窗 —— 那正是要避免的摩擦」。

但本项目的需求里明确写了「系统服务，比如 Windows 服务，可以自启之类的」。
两者不矛盾：**默认用户级（零提权），系统服务作为可选项（一次 UAC）**。

## 问题

| # | 问题 |
| --- | --- |
| Q1 | 用户级自启与系统服务，哪个当默认？ |
| Q2 | 「开机即起（无需登录）」这个能力要不要？ |
| Q3 | 崩溃之后怎么回来？用户级方案没有 SCM 的 failure actions 怎么办？ |
| Q4 | 自更新要重启服务，用户级方案怎么重启？ |
| Q5 | 端口被占用时后台服务**不能**交互提问，怎么办？ |

## 决策

### 1. 默认：**用户级自启**（不需要提权）

| 平台 | 机制 | 自启触发 | 崩溃恢复 |
| --- | --- | --- | --- |
| Windows | **任务计划程序**（`schtasks` / XML，`Hidden`） | `ONLOGON` | 任务的「失败后重新启动」设置（间隔 1 分钟，最多 3 次） |
| macOS | LaunchAgent（`~/Library/LaunchAgents/*.plist`） | `RunAtLoad` + `KeepAlive` | `KeepAlive = { SuccessfulExit = false }` |
| Linux | `systemd --user`（无 systemd 时退 `~/.config/autostart/*.desktop`） | `WantedBy=default.target` | `Restart=always` + `RestartSec=2` |

⚠️ Windows **不用** `HKCU\…\Run`：中继是控制台子系统程序，登录时会被弹出一个黑框窗口。
任务计划程序的 `Hidden` 能避免这一点（这需要 XML 而不是命令行开关，见
[`02-tech-stack.md § 3.2`](../02-tech-stack.md)）。

### 2. 可选：**系统服务**（一次提权）

| 平台 | 机制 | 提权 |
| --- | --- | --- |
| Windows | SCM（`windows-service`），`LocalSystem` 或指定账号 | UAC |
| macOS | `/Library/LaunchDaemons/*.plist` + `launchctl bootstrap system` | `osascript … with administrator privileges` / `SMAppService`（13+） |
| Linux | `/etc/systemd/system/*.service` + `systemctl enable --now` | `pkexec` |

选择入口：安装器 GUI 的高级选项（默认折叠），或 CLI `service install --mode system`。

### 3. Q3：崩溃恢复不依赖「服务」

用户级方案里，重启由**平台自己的调度器**负责（见上表）。但有一个前提：
**安装时必须把失败重启策略写进去**，否则「崩了就一直躺着」。
所以 `service install` 的最后一步是**自检**：

```
安装 → 读取回注册信息（schtasks /query、launchctl print、systemctl show）
     → 确认「失败后重启」字段确实存在
     → 不存在则报错并给出原因（而不是假装装好了）
```

`peon-burrow-update` 与 `service install` 共用这段读取逻辑（[`service-lifecycle.md`](../service-lifecycle.md)）。

### 4. Q4：自更新后的重启，交给调度器

**不让服务自己启动自己**：更新完成后以**非零退出码**（`ExitCode::RestartRequested = 5`）结束，
由 SCM / systemd / launchd / 任务计划程序把它拉起来（完整模型见
[`design/update-flow.md § 6`](../design/update-flow.md)）。
这样权限需求最小（不需要 `sc start` 那种提权调用），语义也最清楚。

### 5. Q5：服务态**不做任何交互**，端口冲突明确失败

| 形态 | 端口被占用时 |
| --- | --- |
| 服务 | 判断占用者是不是自己的旧实例 → 是：视为「已在运行」（退出码 0）；否：**失败退出（退出码 3）**，写状态文件，GUI 显示诊断。**不换端口、不杀进程** |
| 前台 `run` | 同样的判断；额外提供 `--force-port`（显式参数，不是交互提问）才结束占用者 |

理由（对 TS 版的偏离）：扩展只认固定默认端口，**静默换端口 = 用户永远连不上**，
比明确失败更糟；而「后台服务有权限杀任意进程」本身就是个不该开的口子。
详见 [`design/port-and-discovery.md § 5`](../design/port-and-discovery.md)。

### 6. 特权边界（明文写死）

| 操作 | 需要的权限 |
| --- | --- |
| 前台 `run` | 无 |
| 用户级安装 / 卸载 / 自启开关 | 无 |
| 系统服务安装 / 卸载 | 管理员 / root（**一次**） |
| 服务启停（系统服务） | 管理员（或已授予 `SERVICE_START/STOP`）；GUI 不做这件事，改由控制面请求服务**自己**停/重启 |
| 自更新（用户级） | 无（安装目录在用户目录下，可写） |
| 自更新（系统服务） | **无**：更新由服务进程自己做（它以 LocalSystem 运行，本来就写得动 `%ProgramFiles%` 下的安装目录） |

最后一行值得强调：**系统服务模式下，自更新不需要再弹一次 UAC** ——
服务自己的身份就有权限替换自己的文件。

### 7. 单入口二进制（不是 GUI 子系统，也不是双 bin）

**一个控制台子系统二进制**同时承担 CLI、服务入口与控制面服务端；**不为服务再构建一份 GUI 子系统二进制**。

| 选择 | 理由 |
| --- | --- |
| 一个 bin | 自更新只替换**一个文件**；桌面端 `externalBin` 只有一个；SCM 与 CLI 共用同一入口（`windows-service` 的 dispatcher 跑在独立线程上） |
| Windows 用户级自启走**任务计划程序 + `Hidden`** | 避免控制台程序在登录时弹黑框（`HKCU\…\Run` 会弹）；`Hidden` 需要 XML 而不是命令行开关 |
| **不做** GUI 子系统（`windows_subsystem = "windows"`） | 那会让 `burrow doctor` 在终端里没有任何输出，还得额外做 `AttachConsole` —— 复杂度换不到收益 |

**代价**：服务态没有 stdout（日志必须落盘 —— 已在 [`design/logging-and-diagnostics.md`](../design/logging-and-diagnostics.md) 设计）。

> 这条是**定论**（布局评审 丁1）：实现时不要再纠结「要不要为服务单独出一个 bin」。

### 8. 重启策略是一个类型，不是散落的 if

`RestartStrategy { ScmFailureActions | ScheduledTask | Systemd | Launchd | None }` 定义在 `peon-burrow-service`；
`install` 时写入、`doctor` 时自检、`relay-update` 时读取。**update 不关心平台细节**（布局评审 丁2）。
## 后果

### 好的

- 默认路径**零提权**：装一次、重启机器、什么都不用管，符合产品目标；
- 「系统服务」作为高级选项满足「开机即起（无需登录）」与运维需求；
- 崩溃恢复用的是平台原生机制，比自研 watchdog 可靠；
- 端口冲突的行为变得可诊断（GUI 能显示「谁占着」），而不是交互式提问（服务态根本没法问）。

### 代价（如实记录）

| 代价 | 说明 |
| --- | --- |
| 两套机制要维护（用户级 + 系统级）× 三平台 | 用 `peon-burrow-service` 一个 trait 收口：`install/uninstall/start/stop/status/set_autostart` |
| 「登录后才运行」 | 用户未登录时中继不工作 —— 对本地代理没有影响（用户不登录时也不在用浏览器） |
| 任务计划程序需要 XML 才能设 `Hidden` | 落地时确认最简实现；若不可行，退化为「接受一次黑框」并记进已知问题 |
| 系统服务模式下 GUI 与服务跨用户 | 控制面必须有 TCP 退路（[`design/control-plane-ipc.md § 2`](../design/control-plane-ipc.md)） |

### 否决的方案

| 方案 | 否决理由 |
| --- | --- |
| 只做系统服务（默认） | 每个用户都要过一次 UAC，与「零摩擦」冲突；且与 `relay-deployment.md` 已定的结论相反 |
| 只做用户级（不做系统服务） | 需求明确要系统服务能力；且「无人登录的机器」场景（服务器/常开的开发机）需要它 |
| 自研 watchdog / `pm2` 式守护 | 引入一个新的常驻进程来守护一个常驻进程；平台原生机制已经够用 |
| 开机启动目录快捷方式 | 无法配置失败重启；控制台程序会闪窗 |

## 后续

1. macOS 13+ 用 `SMAppService` 还是 plist：**先 plist**（兼容面大），
   `SMAppService` 作为后续优化（它能在「系统设置 → 登录项」里正确显示）；
2. Windows 非管理员账号下任务计划程序的可用性（域策略可能禁）→ `doctor` 要能检测并给出替代方案（前台 `run` + 启动目录）；
3. 服务账号是否可选（Windows `LocalService` 而非 `LocalSystem`）→ 优先 `LocalSystem`（需要写自己目录），记为待决。
