# 服务生命周期

> 决策依据：[`decisions/adr-0003-service-model.md`](./decisions/adr-0003-service-model.md)。
> 这篇讲**状态、动作、自检、故障处理**。

---

## 1. 状态机

```
                 service install                 service start / 登录时自启
   ┌──────────┐ ─────────────────▶ ┌──────────────┐ ─────────────────▶ ┌───────────┐
   │ 未安装    │                    │ 已安装·未运行 │                    │ 运行中     │
   └──────────┘ ◀───────────────── └──────────────┘ ◀───────────────── └───────────┘
        ▲          service uninstall        ▲          service stop           │
        │                                   │                                  │
        │                          crash / 更新重启                            │
        │                                   └──────────────────────────────────┘
        │
        └── 另有两条「不在服务体系里」的状态：
              ① 前台运行中（run --foreground，未安装为服务）
              ② 安装失败（注册项写了一半 / 自检不过）
```

GUI 要能区分**四种组合**（已注册 × 控制面可连），见
[`decisions/adr-0006-desktop-installer.md § 6`](./decisions/adr-0006-desktop-installer.md)。

⚠️ **「已安装但未运行」是正常状态，不是错误**：更新后、被用户停掉之后都是它。
GUI 的文案与按钮要按状态给（`启动` 而不是 `重试`）。

---

## 2. 各平台动作对照

> 📦 所有「查询」都返回同一个类型 **`ServiceStatus`**（定义在 `peon-burrow-ipc-types`，CLI `service status --json`、
> 控制面 `status`、`doctor` 三处共用）；**端口探测走 `peon-burrow-service::probe_port()`**，到处自己解析 `netstat` 是不允许的。

| 动作 | Windows（用户级） | Windows（系统服务） | macOS（用户级） | macOS（系统级） | Linux（用户级） | Linux（系统级） |
| --- | --- | --- | --- | --- | --- | --- |
| 安装 | `schtasks /Create /XML`（任务计划程序） | SCM `CreateService` | 写 `~/Library/LaunchAgents/*.plist` | 写 `/Library/LaunchDaemons/*.plist` | 写 `~/.config/systemd/user/*.service` | 写 `/etc/systemd/system/*.service` |
| 启动 | `schtasks /Run` | `StartService` | `launchctl bootstrap gui/<uid>` + `kickstart` | `launchctl bootstrap system` | `systemctl --user start` | `systemctl start` |
| 停止 | `schtasks /End` 或控制面 | `StopService` | `launchctl bootout` | 同左 | `systemctl --user stop` | `systemctl stop` |
| 自启开 | 任务已注册即自启（`ONLOGON`） | `SERVICE_AUTO_START` | `RunAtLoad` | `RunAtLoad` | `enable` | `enable` |
| 自启关 | `schtasks /Change /Disable` | `SERVICE_DEMAND_START` | 删 plist 或改 `RunAtLoad=false` | 同左 | `disable` | `disable` |
| 卸载 | `schtasks /Delete /F` | `DeleteService` | 删 plist + `bootout` | 同左 | 删 unit + `daemon-reload` | 同左 |
| 状态 | `schtasks /Query /XML` | `QueryServiceStatusEx` | `launchctl print` | `launchctl print system/<label>` | `systemctl --user show` | `systemctl show` |
| 崩溃恢复 | 任务设置「失败后重启」（间隔 1 分钟 × 3） | `ServiceFailureActions`（重启 5s，重置 86400s） | `KeepAlive={SuccessfulExit:false}` | 同左 | `Restart=always` `RestartSec=2` | 同左 |

> Windows 用户级**不用** `HKCU\…\Run`（控制台程序会闪黑框），理由见
> [`02-tech-stack.md § 3.2`](./02-tech-stack.md)。

### 2.1 注册进服务管理器的**命令行**

| 级别 | 注册的命令行 |
| --- | --- |
| 用户级 | `…/burrow run` |
| 系统级 | `…/burrow run --service <服务名> --config <配置文件> --data-dir <数据目录>` |

⚠️ 系统级这三个参数都不是装饰：

1. **`--service <服务名>`**：Windows 上 SCM 拉起的进程必须在 30 秒内通过
   `StartServiceCtrlDispatcher` 报到、把状态转成 `SERVICE_RUNNING`
   （`windows-service` 的 dispatcher，落点 `peon-burrow/src/service.rs`）。
   **缺了它 `sc start` 报 1053**「服务没有及时响应启动或控制请求」—— 服务装得上、
   永远起不来；dispatcher 用的名字还必须与注册名一致（不一致同样是 1053/1063）。
   其它平台上它表示「服务形态」：没有终端（日志落盘）、控制面走 loopback TCP、不等 Ctrl+C；
2. **`--config` / `--data-dir`**：系统服务跑在 `LocalSystem`（或 root）下，而 `directories`
   是按**调用进程的身份**解析的 —— 推出来的是那个账户的配置目录与数据目录，与桌面端
   （登录用户）读的不是同一个。界面会永远显示「已安装，未运行」，扩展里填的地址 / token
   也与服务实际用的那份对不上。把**安装者当时**的路径写进注册信息就对齐了（ADR-0010 § 16）；
3. **失败重启策略**（SCM 的 `sc failure`）只在「进程**没有**报 `SERVICE_STOPPED` 就退出」时执行
   （`SERVICE_FAILURE_ACTIONS_FLAG` 默认 FALSE）。所以服务入口的约定是：
   **干净停下要明确报 STOPPED，非零退出（出错 / 自更新后请求重启）故意不报** ——
   否则自更新之后服务就再也回不来了（§ 5）。
   ⚠️ 顺带一个必须写下来的实现细节：非零退出时**光从 `service_main` return 是不够的** ——
   `StartServiceCtrlDispatcherW` 要等到「进程里所有服务都报了 STOPPED」才返回，
   不报就永远阻塞；必须显式 `process::exit(code)`。第一版漏了这一步，真机表现是
   「`sc query` 显示 RUNNING、端口没人监听」的僵尸进程（还锁着安装目录里的 exe）。

> ⚠️ 注册信息里的命令行**会随版本变**。所以 `service install` 在「服务已存在」时不能只跳过
> `sc create`：必须顺手 `sc config … binPath=` 把命令行刷新到这一版期望的值，否则重装
> 「成功」了、服务跑的仍是老命令行（老命令行没有 `--service`，于是每次启动都是 1053 ——
> 真机踩过）。`sc config` 对**正在运行**的服务只改注册信息，所以安装流程会先停掉旧实例。

---

## 3. `service install` 的步骤（含自检）

```
0. 升级安装：先停掉旧实例，并等它真的退出（exe 映像没放开就复制不了）
1. 解析配置（含 --mode / --autostart）
2. 决定安装目录
     用户级 : %LOCALAPPDATA%\Programs\peon-burrow\ / ~/Library/Application Support/peon-burrow/bin/
              / ~/.local/share/peon-burrow/bin/
     系统级 : %ProgramFiles%\peon-burrow\ / /usr/local/libexec/
3. 复制当前二进制到安装目录（若已在安装目录里跑，跳过；并记录「正在运行的 exe 路径」）
4. 写默认配置文件（**已存在则不覆盖**）
5. 创建日志目录
6. 注册服务/任务（含失败重启策略；已存在时刷新命令行，见 § 2.1）
7. 启动
8. 等控制面可连（超时 15 秒，每 500ms 重试一次）
9. **自检**：
     a. 重新读取注册信息 → 确认已注册、自启状态、失败重启策略都在
     b. 控制面 `ping` → 确认真的起来了
     c. 把结果写进发现文件，并输出一行 JSON（供 GUI 解析）
   任一项失败 → 回滚（反注册 + 删安装目录里的新文件）并返回非零退出码
```

实现状态（与上面这段的差）：

| 步骤 | 状态 |
| --- | --- |
| 0（升级先停） | ✅ 已实现（`run.rs` 的 install 分支：`stop()` + 等 `running=false`，最多 5 秒） |
| 3 | ✅ 已实现（第 3 步的「正在运行的 exe」= `current_exe()`） |
| 4 / 5 | ⏳ **没做**：不写默认配置文件（`Config::load` 对不存在的文件用默认值即可）；日志目录由中继启动时的 `Paths::ensure_dirs()` 建 |
| 6 | ✅ 已实现，且已存在时走 `sc config` 刷新命令行（§ 2.1） |
| 8 / 9b | ✅ 已实现：`service install` 在 `start()` 之后**真的等一次控制面 `ping`**（15 秒 / 500ms），失败就返回非零退出码并指出日志路径。`sc start` / `schtasks /Run` 成功只说明「进程被拉起来了」，配置错 / 端口被占都是之后才发生的 |
| 9a | ✅ 已实现（各平台的 `verify_restart_policy()`） |
| 9c | ⏳ **没做**：不写发现文件（发现文件是**中继自己**写的运行时状态，不是安装结果） |
| 回滚 | ⏳ **没做**：第 8/6 步失败时**只报错、不反注册**。理由：注册项本身是好的，留着它用户能接着查（§ 7 的「服务装了但没跑 → 点启动；看日志尾部」就是这条路）；自动反注册会让「一次慢启动」变成「装的东西没了」，更难解释 |

⚠️ 第 3 步的「正在运行的 exe」很关键：如果用户是**直接双击下载的可执行文件**然后点安装，
那么这个 exe 在临时目录里（Windows 上经常被清理）→ 必须复制到安装目录再注册，
否则重启后服务指向一个不存在的路径。这是「装完看着挺好，重启就没了」的经典成因。

⚠️ 第 9a 步不是形式主义：不同平台的失败重启配置项很容易写漏
（Windows 用 `sc create` 就根本写不了 failure actions），而它的失效只在「服务崩了」时才暴露。

---

## 4. `service uninstall`

```
1. 停止服务（若在跑）；等控制面消失（超时 10 秒）
2. 反注册（SCM DeleteService / 删 plist / 删 unit + daemon-reload）
3. 询问（GUI 上是勾选框）是否删除用户数据：
     ☐ 配置 relay.toml
     ☐ 日志
   默认**都保留**（用户可能只是重装）
4. 删安装目录里的二进制（保留数据目录）
5. 删发现文件与控制面 token 文件
```

⚠️ 卸载**必须**先停服务：Windows 上 `DeleteService` 只做标记，正在运行的进程仍会跑
（表现为「卸载了但扩展还能收信」）。

⚠️ 但**「停」的判据是状态，不是 `sc stop` 的退出码**（真机踩过）：中继自己从控制面停下来
（如实报 `SERVICE_STOPPED`）之后再点「停止 / 卸载」，`sc stop` 会报 **1061**
「服务无法在此时接受控制信息」—— 服务处于 `STOPPED` 时 `dwControlsAccepted` 本来就是 0，
而 SCM 对没有声明 `SERVICE_ACCEPT_STOP` 的服务就是回 1061（历史实现只容忍了 1062 / 1060），
于是「已经停好」这个**正确状态**被当成失败，卸载死在这一步。现在的顺序是：

```
1. 先问状态（sc query）：已经 STOPPED → 一条命令都不发，直接成功
2. 否则发 sc stop；1060 / 1061 / 1062 都**不**直接当失败（可能是「其实已经停了」的竞态）
3. 核实：轮询 sc query，直到出现 STOPPED（最多 10 秒）
     停不下来 → StopTimedOut（附 `taskkill` / 重启机器 的下一步），**绝不**带着活进程去 delete
4. sc delete（1060 = 没装 → 幂等成功）
```

判据用「出现 `STOPPED`」而不是「没有 `RUNNING`」：`START_PENDING` / `STOP_PENDING` /
`PAUSED` 期间进程都还活着。

---

## 5. 自更新后的重启

> 结束时的退出码是 **`ExitCode::RestartRequested = 5`**（唯一来源：`peon_burrow::ExitCode`）。

以**非零退出码**结束，由服务管理器拉起（完整模型见
[`design/update-flow.md § 6`](./design/update-flow.md)）。这里只强调三件事：

| 事项 | 说明 |
| --- | --- |
| 必须是**非正常退出** | SCM 的 failure actions、launchd 的 `SuccessfulExit=false` 都只对非零退出生效 |
| 重启间隔要够短 | 5 秒内起来，扩展侧的 watch 重连（1s/2s/5s…）能在用户察觉前恢复 |
| 日志要说明是更新触发的 | 否则用户看到「服务重启了」会以为崩了 |

---

## 6. 权限矩阵

| 动作 | 用户级 | 系统级 |
| --- | --- | --- |
| install / uninstall | 无 | 管理员 / root |
| start / stop | 无 | 管理员（**或**由服务自己通过控制面停） |
| 自启开 / 关 | 无 | 管理员 |
| 读状态 | 无 | 无（只读查询通常不需要提权；`launchctl print` 与 `systemctl show` 可读） |
| 自更新 | 无（自己的目录可写） | 无（服务以 LocalSystem 跑，写得动自己的安装目录） |

---

## 7. 故障模式排查表（写给 `doctor` 与 GUI）

| 症状 | 大概率原因 | `doctor` 怎么显示 | 处置 |
| --- | --- | --- | --- |
| 扩展连不上，服务显示运行中 | 端口不是扩展里填的那个 | 打印 `url`（发现文件） | 复制正确地址到扩展；或改端口 |
| 服务装了但没跑 | 上次更新后没起来 / 启动失败 | 「已安装·未运行」+ 上次退出码 | 点启动；看日志尾部 |
| 重启机器后要手动启动 | 自启没配上 / 任务被禁用 | 「已安装·未运行」+ 自启状态 | 重新点「开机自启」；看第 9a 自检日志 |
| 崩了不回来 | 失败重启策略没写进去 | 自检项标红 | 重装服务（`service install` 会重写策略） |
| 端口被别的东西占了 | 端口冲突 | PID + 进程名 + 三条出路 | 改配置端口（**记得同步改扩展**） |
| 收不到新邮件但连接正常 | watch 致命错误（密码/白名单） | `watch.fatal` 事件 + GUI 提示 | 让用户去扩展的账号页改凭据 |
| 内存一直涨 | 背压没生效（回归） | `tunnel.backpressure` 事件频繁 | 报 bug（这是不变量 I6/D4 的回归） |

---

## 8. 验收

| # | 断言 |
| --- | --- |
| 1 | 用户级安装全程**零 UAC / 零 sudo** |
| 2 | 安装后自检能发现「失败重启策略没配上」并回滚 |
| 3 | 重启机器 → 服务自动运行 → 扩展能收信（不手动做任何事） |
| 4 | 杀掉服务进程 → 调度器在 1 分钟内把它拉起来（三平台各测一次） |
| 5 | `service stop` 时挂着的 IDLE 连接不会把关停卡住（≤ 5 秒） |
| 6 | 卸载后：注册项消失、进程消失、发现文件消失；配置与日志按选择保留 |
| 6b | **已停止**时点停止 / 卸载也要成功（幂等）：`sc stop` 对停着的服务会报 1061/1062，那不是失败 |
| 7 | 从临时目录里的 exe 安装 → 重启后仍能工作（第 3 步的复制逻辑） |
| 8 | 系统服务模式下，GUI（非提权）能读状态、能请求停止/重启（控制面 TCP 退路） |
| 9 | 升级安装（已有服务）不覆盖用户的 `relay.toml` |
| 10 | 装到一半失败（人为让第 6 步报错）→ 回滚干净，不留半个注册项 |
