# 设计 · 日志与诊断

> 服务化的第一代价：**用户看不到终端了**。
> 所以「日志能不能看」「出问题能不能自证」不是体验问题，是**能不能上线**的问题。

---

## 1. 输出目标

| 目标 | 何时 | 说明 |
| --- | --- | --- |
| **stdout** | `run --foreground` | 人类可读，沿用 TS 版风格 `[relay] 2026-10-09T04:07:40Z …`，便于对照旧文档 |
| **滚动日志文件** | 服务态（默认）与前台（可选） | 默认 `info`；单文件 10 MiB，保留 5 个；跨平台用 `tracing-appender` 的按天/按大小滚动 |
| **Windows 事件日志** | 可选（`--log-target eventlog`） | 只写「服务启动/停止/崩溃/自更新」这类**低频重要事件**；不放连接级日志（事件日志不适合高频写） |
| **控制面 `doctor` / 最近日志** | GUI 点「诊断」 | 返回结构化结果 + 最近 N 行日志（≤ 200 行，脱敏后） |

文件路径见 [`01-architecture.md § 6`](../01-architecture.md)。

---

## 2. 格式与级别

### 2.1 两种格式

```
# 人类可读（默认）
[relay] 2026-10-09T04:07:40.123Z  INFO  relay listening on 127.0.0.1:41316
[relay] 2026-10-09T04:07:52.881Z  INFO  CONNECT imap.qq.com:993 (tls)
[relay] 2026-10-09T04:08:03.002Z  WARN  policy reject: host not allowed: evil.example
[relay] 2026-10-09T04:08:11.410Z  INFO  CLOSE imap.qq.com:993 (ws closed)

# 机器可读（--log-format json，用于 GUI 解析与用户贴工单）
{"t":"2026-10-09T04:07:40.123Z","level":"INFO","event":"relay.listen","addr":"127.0.0.1:41316","mode":"service","version":"0.1.0"}
```

### 2.2 级别

| 级别 | 内容 | 默认 |
| --- | --- | --- |
| `error` | 服务无法提供功能（绑定失败、控制面创建失败、自更新失败后的不可用态） | ✅ |
| `warn` | 可继续但需要人看：策略拒绝、TLS 校验证书失败、背压触发、未知配置字段 | ✅ |
| `info` | 生命周期与连接的**开合**：启动/停止/监听地址、`CONNECT` / `CLOSE`、watch 状态迁移、自更新检查结果 | ✅ |
| `debug` | 每个 IMAP 命令的状态机迁移、tag、重连尝试次数（**不含字节内容**） | ❌ |
| `trace` | **原始字节**（含凭据） | ❌ |

### 2.3 凭据绝不进日志（默认）

| 内容 | 默认行为 |
| --- | --- |
| `pass` / `token` 配置值 | **永不打印**（连 `debug` 都不打）；`doctor` 里只显示「已设置 / 未设置」 |
| `LOGIN` 命令 | `info`/`debug` 下不打印命令原文 |
| 邮件正文 | 永不打印（中继本来也不解析） |
| `trace = true` 时 | 打印原始字节，但**每一行加 `[plaintext]` 前缀**，并且在启动日志里持续打印一段警告 |

```text
[relay] 2026-10-09T04:09:00.000Z  WARN  ┌───────────────────────────────────────────────
[relay] 2026-10-09T04:09:00.000Z  WARN  │ trace 已开启：日志将包含邮箱明文凭据（LOGIN 命令）
[relay] 2026-10-09T04:09:00.000Z  WARN  │ 60 秒后自动关闭。不要把这段时间的日志贴到公开渠道。
[relay] 2026-10-09T04:09:00.000Z  WARN  └───────────────────────────────────────────────
[relay] 2026-10-09T04:09:01.100Z  INFO  [plaintext] → A0001 LOGIN "you@qq.com" "****"
```

> ⚠️ 这里对「密码本身」做了打码（`****`），只保留**命令结构**。
> 理由：排查「命令发出去没有、格式对不对」不需要看密码原文；
> 但 `trace` 的存在意义是看到**走到哪一步**，不是看到凭据。
> 若某天确实需要看原文，必须再开一个更高等级（暂不提供）。

### 2.4 必须出现的事件（结构化事件名）

这些事件名进 `json` 格式与 GUI 的日志视图，**不要随手改**（用户工单里会引用）：

| 事件 | 级别 | 关键字段 |
| --- | --- | --- |
| `relay.start` | info | `version` `mode` `configPath` |
| `relay.listen` | info | `addr` `port` `url` |
| `relay.stop` | info | `reason` `uptimeSecs` `activeConnections` |
| `relay.port_in_use` | error | `port` `ownerPid` `ownerName` `isSelf` |
| `relay.already_running` | info | `pid` `port`（自己的旧实例占用端口） |
| `policy.reject` | warn | `reason` `host` `port` `tls` `remote` |
| `tunnel.connect` | info | `host` `port` `tls` `servername` |
| `tunnel.connect_failed` | warn | `host` `port` `code`（`ECONNREFUSED` / 证书错误…） |
| `tunnel.close` | info | `host` `port` `reason` `inBytes` `outBytes` `durationMs` |
| `tunnel.backpressure` | warn | `host` `port` `inFlightBytes` |
| `watch.start` | info | `host` `port` `accountId` |
| `watch.state` | info | `accountId` `state`（`watching` / `reconnecting` / `failed`） |
| `watch.mail` | info | `accountId` `exists`（**只有数字**） |
| `watch.fatal` | warn | `accountId` `reason`（分类，不落原文里的凭据） |
| `update.check` | info | `current` `latest` `available` `source` |
| `update.apply` | info | `from` `to` `asset` `verified` |
| `update.failed` | error | `stage` `reason` |
| `service.install` / `service.uninstall` | info | `mode` `autostart` `path` |

---

## 3. `doctor`：把「怎么排查」变成一条命令

`burrow doctor [--json]` 输出**逐项检查 + 结论 + 下一步动作**。
这是替代「用户在终端里手敲 netstat」的核心工具。

| # | 检查项 | 判定 | 失败时的下一步（文案要点） |
| --- | --- | --- | --- |
| 1 | 配置文件可读、可解析、通过校验 | 读文件 → 校验规则 | 指出**具体哪个键、什么值、期望什么** |
| 2 | 版本与环境 | 打印 `version` / `gitSha` / OS / 架构 | —— |
| 3 | 系统时间偏差 | 与 HTTPS 响应头比对，偏差 > 5 分钟报警 | TLS 证书校验依赖系统时间；偏差大会导致「证书未生效/已过期」 |
| 4 | TLS 根证书可用 | `webpki-roots` 能否构造 ClientConfig | 缺根证书 → 所有 993 连接都会失败 |
| 5 | 端口状态 | 空闲 / 本中继占用 / 别进程占用（PID + 进程名） | 三条出路（见 [`port-and-discovery.md § 5.2`](./port-and-discovery.md)） |
| 6 | 端口合规性 | IANA 未注册、< 49152、不在 Windows 排除区间 | 若落进排除区间 → 「换成 41316/41317…」 |
| 7 | 控制面 | 可连 / 不可连 / token 文件权限过松 | 权限过松（Unix 非 0600）→ 提示 `chmod` |
| 8 | 服务注册状态 | SCM / launchd / systemd / 计划任务：已注册？自启？ | 未注册 → 「在安装器里点『安装服务』」 |
| 9 | 发现文件一致性 | 与真实状态比对（含陈旧 PID） | 陈旧 → 按需重建；端口不一致 → 报警 |
| 10 | 日志目录 | 存在、可写、剩余空间 > 50 MiB | 不可写 → 给出具体路径与权限建议 |
| 11 | 安全组合 | `host` 非 loopback 但缺 token/白名单；`tls_reject_unauthorized=false`；`trace=true` | 每条都给「这意味着什么」的一句话 |
| 12 | 自更新状态 | 上次检查时间、是否可用、上次失败原因 | 失败 → 给出镜像站配置建议（网络受限场景） |
| 13 | 扩展可见性（尽力而为） | 打印「扩展里应填的地址」（来自发现文件） | 用户最容易错的一步：**一键复制这个地址** |

输出示例：

```
$ burrow doctor
burrow 0.1.0 (a1b2c3d, 2026-10-09)
系统        : Windows 11 26100 · x86_64
配置        : %APPDATA%\peon-burrow\relay.toml ✅
TLS 根证书  : ✅ webpki-roots
端口 41316  : ⚠ 被占用 —— PID 8899 chrome.exe（不是本中继）
              → 改配置里的 port，或前台临时用 --port 41317
服务        : ✅ 已安装（用户级自启 · 登录时启动）· 未运行
控制面      : ✅ 可连（\\\\.\\pipe\\peon-burrow-imba97）
安全        : ✅ loopback + token 已设置
自更新      : ✅ 24 小时前检查过，当前为最新
扩展地址    : ws://127.0.0.1:41316/    ← 复制到扩展的账号配置里
提示        : 服务未运行，先执行 burrow service start
```

**给 GUI 的 `--json`**：同一份检查结果的结构化版本（`[{id, level, title, detail, action}]`），
GUI 直接渲染成列表 + 「执行建议动作」按钮。

---

## 4. 用户可见文案规范

服务形态的用户**不是开发者**。所有面向用户的错误文案必须满足：

1. **说人话**：不出现 `EADDRINUSE`、`UNABLE_TO_VERIFY_LEAF_SIGNATURE` 这类原生错误码（可以放在括号里作为附加信息）；
2. **给下一步**：每条错误至少一个可执行动作（「点哪个按钮」「改哪个文件的哪一行」）；
3. **不甩锅**：不写「请检查你的网络」这种没有信息量的话；
4. **可复制**：地址、路径、命令一律给成可复制的完整形式（GUI 里就是带复制按钮）。

| 场景 | ❌ 不要 | ✅ 要 |
| --- | --- | --- |
| 端口被占 | `bind failed: EADDRINUSE` | `端口 41316 被 chrome.exe（PID 8899）占用了。要么改 relay.toml 里的 port，要么在安装器里换个端口。` |
| 证书失败 | `invalid peer certificate: UnknownIssuer` | `邮件服务器用了自签证书，当前配置拒绝连接。仅测试环境可把 tls_reject_unauthorized 设为 false。` |
| 密码错 | `A0001 NO [AUTHENTICATIONFAILED]` | `邮箱授权码被拒绝（QQ 邮箱要用「授权码」而不是登录密码）。请在扩展的账号页重新填写。` |
| 白名单拒绝 | `host not allowed` | `imap.163.com 不在允许列表里。在 relay.toml 的 allowed_hosts 里加上它，然后重启服务。` |

---

## 5. 日志滚动与磁盘占用

| 项 | 值 | 理由 |
| --- | --- | --- |
| 单文件上限 | 10 MiB | 足够放下几万条连接级日志 |
| 保留个数 | 5 | 总量上限 50 MiB；一个常驻工具不该吃掉用户几百 MB |
| 滚动时机 | 按大小（跨天也滚一次，便于按日期找） | —— |
| 清理 | 超出保留个数的旧文件自动删 | —— |
| 磁盘紧张时 | 写入失败**不致命**：降级为「只写 stdout（若有）+ 记一条 `error`」，中继继续工作 | 日志写不进去不该导致收不到邮件 |

---

## 6. 验收

| # | 断言 |
| --- | --- |
| 1 | 默认级别下，日志中**不出现**任何 `pass` / `token` 值（测试用固定串扫描全文） |
| 2 | `trace` 打开后：每行带 `[plaintext]` 前缀、密码字段打码、到点自动关闭（不依赖重启） |
| 3 | 滚动：写入 > 10 MiB 后产生新文件，旧文件数不超过 `log_keep_files` |
| 4 | 日志目录不可写 → 中继仍能收信，日志里有一条 `error` |
| 5 | `doctor --json` 的每个 `id` 与本文档 § 3 表格逐条对应（表格即测试清单） |
| 6 | 端口被别进程占用时 `doctor` 报出的 PID/进程名与实际一致 |
| 7 | 系统时间被改到偏差 > 5 分钟 → `doctor` 报警 |
| 8 | 所有 `error` / `warn` 文案里不出现裸的原生错误码作为**主**信息（人工 review + 一条正则守卫） |
| 9 | `json` 格式的事件名与 § 2.4 表格一致（测试里写死集合） |
