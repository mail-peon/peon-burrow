# 设计 · 配置（文件 / CLI / 环境变量）

> 目标：**服务形态下配置必须能落盘、能手改、能备份**；同时让 TS 版用户
> 照着旧文档敲的命令与 `PORT=…` 环境变量继续生效。

---

## 1. 优先级

```
CLI 参数  >  环境变量  >  配置文件  >  内置默认值
```

理由：CLI 是「这一次运行」，ENV 是「这台机器/这个服务的注入」，
配置文件是「这个用户的长期偏好」。反过来（配置文件压 ENV）会让
`PORT=41317 burrow run` 这种一次性覆盖失效，与 TS 版行为不一致。

---

## 2. 配置文件

TOML。路径可用 `--config <path>` 覆盖；默认位置：

| 平台 | 路径 |
| --- | --- |
| Windows | `%APPDATA%\peon-burrow\relay.toml` |
| macOS | `~/Library/Application Support/peon-burrow/relay.toml` |
| Linux | `~/.config/peon-burrow/relay.toml` |

> **默认值的唯一来源是 `peon-burrow-core`**（L3）：下面 TOML 里出现的值只是「核心默认值」的展示 ——
> 配置结构里每个字段都是 `Option<T>`，缺省即落到 core 的 `RelayOptions::default()`。**改默认值只改一处。**
>
> ⚠️ 路径来自 **`Paths`（注入值，L4）**：只有 `Paths::discover()` 允许碰 `directories`，
> 测试用 `Paths::for_test(tempdir)` —— 别让任何模块直接调 `ProjectDirs`。

完整 schema（含默认值）：

```toml
# ---- 监听 ----------------------------------------------------------------
# 只绑本机的理由见 ai-docs/00-overview.md § 6：中继能看到邮箱明文凭据。
host = "127.0.0.1"
# 默认端口的选择依据见 ai-docs/design/port-and-discovery.md § 3。
port = 41316

# ---- 访问控制 ------------------------------------------------------------
# 非空时，连接必须带 ?token=<该值>（watch 请求里则是 token 字段）。
token = ""
# 允许的邮件服务器，支持 * 通配。空数组 = 不限制目标。
allowed_hosts = []

# ---- TLS -----------------------------------------------------------------
# 是否校验证书链。**关掉等于放弃中间人防护**，只用于测试自签证书的服务器。
tls_reject_unauthorized = true

# ---- 连接行为 ------------------------------------------------------------
# 透传连接的空闲超时（秒）。watch 连接**不适用**（IDLE 会长期静默）。
idle_timeout_secs = 900
# 并发连接上限（一个账号的 watch 占 1 条）。
max_connections = 32
# TCP→WebSocket 方向的在途字节上限（背压阈值），单位 MiB。
backpressure_high_water_mib = 16
# watch 模式：重发 IDLE 的间隔（秒）。RFC 2177 建议 ≤ 29 分钟。
watch_reidle_secs = 1500
# watch 模式：断了之后的重连退避阶梯（秒）。
watch_retry_delays_secs = [2, 5, 15, 30, 60, 120, 300]

# ---- 日志 ----------------------------------------------------------------
# error | warn | info | debug | trace
log_level = "info"
# 是否把原始字节打进日志。**含明文凭据**，默认关；服务态需二次确认（见 § 5）。
trace = false
# 日志文件上限（MB）与保留个数；0 表示只写 stdout。
log_max_mib = 10
log_keep_files = 5

# ---- 服务 ----------------------------------------------------------------
[service]
# 用户级自启（默认，不提权）还是系统服务（需提权）
mode = "user"                 # user | system
# 自启触发方式
autostart = "logon"           # logon | boot | off
# 服务名（Windows 服务名 / launchd label / systemd unit 名）
name = "peon-burrow"

# ---- 自更新 --------------------------------------------------------------
[update]
enabled = true
channel = "stable"            # stable | beta
check_interval_hours = 24
# 可选：镜像站前缀。最终 URL = base_url + "/<owner>/<repo>/releases/download/…"
# base_url = "https://gh-proxy.com/https://github.com"
# 需要校验和（从不跳过）
verify_checksum = true

# ---- 控制面 --------------------------------------------------------------
[control]
enabled = true
# 只在系统服务模式下有意义：额外允许哪些本机用户连接（默认只有服务账号自己）
# allowed_users = []
```

### 2.1 校验规则

> 校验属于 **`peon-burrow` 的 `config` 模块**（产品层，组合规则）；`peon-burrow-core` 只做**运行期**的目标级检查
> （`TargetResolve::Rejected`）。两处各写一半是这次评审特意消掉的重复（乙2）。

| 规则 | 违反时 |
| --- | --- |
| `port` ∈ 0..=65535（`0` = 内核分配，仅测试/前台） | 退出码 `2`，指出字段与取值 |
| `host` 非空 | 同上 |
| **`host` 非 loopback 时，`token` 与 `allowed_hosts` 都非空** | 退出码 `2`（这是对 TS 版的收紧，见 [`port-and-discovery.md § 7`](./port-and-discovery.md)） |
| `tls_reject_unauthorized = false` | 允许，但启动日志与 GUI 状态页**持续**显示警告 |
| `max_connections` ≥ 1 且 ≤ 4096 | 退出码 `2` |
| `watch_retry_delays_secs` 非空、递增、每项 ≥ 1 | 退出码 `2` |
| `log_level` / `channel` / `mode` / `autostart` 必须是枚举值 | 退出码 `2`，列出可选值 |
| **未知字段** | `warn` 一行日志（不失败）：允许用户写注释性字段、也允许旧配置里有已被移除的键 |

> 「未知字段只警告不报错」是刻意的：服务如果因为用户多写一个键就起不来，
> 用户看到的是「装完不能用」，而他根本没有线索。

---

## 3. 环境变量映射（**必须向后兼容**）

| TS 版变量 | Rust 版 | 说明 |
| --- | --- | --- |
| `PORT` | `PORT` | 保留 |
| `HOST` | `HOST` | 保留 |
| `RELAY_TOKEN` | `RELAY_TOKEN` | 保留 |
| `ALLOWED_HOSTS` | `ALLOWED_HOSTS` | 逗号分隔（与 TOML 数组等价） |
| `TLS_REJECT_UNAUTHORIZED` | `TLS_REJECT_UNAUTHORIZED` | `0` = 关（**只有字面 `0`**，与 TS 一致） |
| `RELAY_TRACE` | `RELAY_TRACE` | `1` = 开 |
| — | `RELAY_CONFIG` | 配置文件路径 |
| — | `RELAY_LOG_LEVEL` | 等价 `log_level` |

布尔解析规则统一：`1/true/yes/on` 为真，`0/false/no/off` 为假，其它 → 配置错误。

---

## 4. CLI 表面

```
burrow <命令>

  run                     前台运行（调试用）
    --port <n>            覆盖端口
    --host <addr>         覆盖监听地址
    --config <path>       指定配置文件
    --force-port          端口被占用时：结束占用者再绑定（**仅前台**，见 port-and-discovery § 5.3）
    --log-level <level>
    --trace[=<secs>]      临时开 trace，到点自动关

  service                 服务管理（install/uninstall/start/stop 需要提权）
    install  [--mode user|system] [--autostart logon|boot|off]
    uninstall
    start | stop | restart
    status                --json 输出，供 GUI 解析
    autostart <on|off>    只切自启，不动服务本身

  status                  = service status + 控制面 status 的合成视图
  doctor [--port] [--json]  自检（见 logging-and-diagnostics.md § 3）
  update                  check | apply [--dry-run]
  version
  completion <shell>
```

退出码是 **`peon_burrow::ExitCode`** 枚举（唯一来源，见 `modules.md § 7`）——
`main` 与各层模块**都不许**硬编码数字；自更新完成后返回 `RestartRequested = 5`。

退出码（GUI 与脚本依赖它区分失败原因）：

| 码 | 含义 |
| --- | --- |
| 0 | 正常（含「已经在运行，无需重复启动」） |
| 1 | 运行期失败（TLS 起不来、控制面创建失败…） |
| 2 | 配置错误（含非法值、非 loopback 但缺 token/白名单） |
| 3 | 端口被占用（**且占用者不是本中继**） |
| 4 | 需要提权（`service install` 等在前台非提权环境下被调用） |
| 5 | 自更新已完成，需要重启进程（调用方应重启服务） |

---

## 5. `trace` 的开启方式（服务态）

| 途径 | 适用 | 约束 |
| --- | --- | --- |
| `run --trace=60` | 前台调试 | 秒级自动关闭 |
| 控制面 `traceOn {seconds}` | GUI「临时记录明文日志」按钮 | 上限 300 秒；**必须**二次确认；日志每行加 `[plaintext]` 前缀 |
| 配置文件 `trace = true` | 极端排查 | 启动时**强制**打印一段警告，GUI 状态页常驻红色标识 |

⚠️ trace 会打印 `LOGIN` 命令（**含邮箱授权码**）。三处必须同时做到：
① 默认关；② 打开时明确告知；③ 日志里能一眼看出「这段含凭据」
（详见 [`logging-and-diagnostics.md § 2.3`](./logging-and-diagnostics.md)）。

---

## 6. 常见配置示例

**① 只想收 QQ 邮箱（最常见）**

```toml
port = 41316
allowed_hosts = ["imap.qq.com"]
```

**② 多邮箱、锁定范围**

```toml
allowed_hosts = ["imap.qq.com", "imap.163.com", "*.gmail.com", "*.outlook.com"]
```

**③ 端口被别的软件占了**

```toml
port = 41317     # 改完记得：扩展里的中继地址也要改成 ws://127.0.0.1:41317/
```

**④ 局域网内给另一台机器用（**强烈不建议**）**

```toml
host = "0.0.0.0"
port = 41316
token = "<32+ 字节随机串>"
allowed_hosts = ["imap.qq.com"]
tls_reject_unauthorized = true
```

**⑤ 测试自签证书的邮件服务器**

```toml
tls_reject_unauthorized = false   # 仅测试环境
```

---

## 7. 验收

| # | 断言 |
| --- | --- |
| 1 | 优先级：`--port` 覆盖 ENV，ENV 覆盖文件，文件覆盖默认 |
| 2 | 旧的 `PORT` / `HOST` / `RELAY_TOKEN` / `ALLOWED_HOSTS` / `TLS_REJECT_UNAUTHORIZED` / `RELAY_TRACE` **全部仍然生效** |
| 3 | 未知字段 → 一行 warn，进程正常启动 |
| 4 | 非法 `port` / `log_level` → 退出码 2，错误信息含字段名、实际值、可选值 |
| 5 | `host = "0.0.0.0"` 且 token/白名单为空 → 退出码 2 |
| 6 | `TLS_REJECT_UNAUTHORIZED=0` 时启动日志与 `doctor` 都报警 |
| 7 | `--config <path>` 在不存在的路径上 → 退出码 2（不是静默用默认值） |
| 8 | 配置里写的所有键都能被 `relay.toml` 示例覆盖到（示例即测试夹具，防文档漂移） |
