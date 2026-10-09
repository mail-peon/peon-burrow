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

---

## 3. `service install` 的步骤（含自检）

```
1. 解析配置（含 --mode / --autostart）
2. 决定安装目录
     用户级 : %LOCALAPPDATA%\Programs\peon-burrow\ / ~/Library/Application Support/peon-burrow/bin/
              / ~/.local/share/peon-burrow/bin/
     系统级 : %ProgramFiles%\peon-burrow\ / /usr/local/libexec/
3. 复制当前二进制到安装目录（若已在安装目录里跑，跳过；并记录「正在运行的 exe 路径」）
4. 写默认配置文件（**已存在则不覆盖**）
5. 创建日志目录
6. 注册服务/任务（含失败重启策略）
7. 启动
8. 等控制面可连（超时 15 秒，每 500ms 重试一次）
9. **自检**：
     a. 重新读取注册信息 → 确认已注册、自启状态、失败重启策略都在
     b. 控制面 `ping` → 确认真的起来了
     c. 把结果写进发现文件，并输出一行 JSON（供 GUI 解析）
   任一项失败 → 回滚（反注册 + 删安装目录里的新文件）并返回非零退出码
```

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

---

## 5. 自更新后的重启

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
| 7 | 从临时目录里的 exe 安装 → 重启后仍能工作（第 3 步的复制逻辑） |
| 8 | 系统服务模式下，GUI（非提权）能读状态、能请求停止/重启（控制面 TCP 退路） |
| 9 | 升级安装（已有服务）不覆盖用户的 `relay.toml` |
| 10 | 装到一半失败（人为让第 6 步报错）→ 回滚干净，不留半个注册项 |
