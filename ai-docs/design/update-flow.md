# 设计 · 自更新

> 一个**能看到邮箱明文凭据**的常驻进程，它的更新通道就是它的攻击面。
> 所以这里的原则是：**清单固定、下载必校验、替换可失败、重启交给系统**。

---

## 1. 目标与非目标

| | 内容 |
| --- | --- |
| 目标 | ① 无人值守（服务态没有终端）；② 能走镜像站（GitHub 在很多网络里不可达）；③ 校验 + 验签；④ 失败时**保留原二进制**并留下可读日志；⑤ 更新后能恢复收信 |
| 非目标 | ① 不自动降级（不回退到旧版本）；② 不支持「热更新」（不重启就换代码）；③ 不更新桌面端（[`adr-0006`](../decisions/adr-0006-desktop-installer.md)：桌面端是免安装工具，不检查更新） |

---

## 2. 清单（`latest.json`）

### 2.1 URL 策略

| 渠道 | URL | 说明 |
| --- | --- | --- |
| `stable` | `https://github.com/<owner>/<repo>/releases/latest/download/latest.json` | **固定 URL**：`latest` 是 GitHub 的重定向，不需要 API、不计 rate limit、镜像站可以整个前缀替换 |
| `beta` | `https://github.com/<owner>/<repo>/releases/download/beta-latest/latest.json` | 滚动 tag（预发布版本不会出现在 `latest` 里） |
| 镜像 | `<base_url>` 替换 `https://github.com` | 例：`https://gh-proxy.com/https://github.com` |

> 本仓库的实际取值（`<owner>` = `mail-peon`，`<repo>` = `peon-burrow`）：
> `https://github.com/mail-peon/peon-burrow/releases/latest/download/latest.json`

> ⚠️ **不要**用 GitHub API 列表接口（`/releases`）来「自己找最新版本」：
> 匿名 60 次/小时/IP，一台机器一天检查 24 次看起来没事，但同一个出口 IP 后面
> 可能是整个办公室；而且镜像站大多只代理 `download` 路径，不代理 API。
> 用**一个静态清单 URL** 把这两件事一起解决。

### 2.2 格式

```json
{
  "schema": 1,
  "version": "0.2.0",
  "channel": "stable",
  "releasedAt": "2026-10-09T04:00:00Z",
  "notesUrl": "https://github.com/mail-peon/peon-burrow/releases/tag/v0.2.0",
  "assets": [
    {
      "target": "x86_64-pc-windows-msvc",
      "name": "peon-burrow-x86_64-pc-windows-msvc.zip",
      "size": 5242880,
      "sha256": "…64 hex…",
      "signature": "untrusted comment: minisign signature\n…"
    },
    {
      "target": "aarch64-apple-darwin",
      "name": "peon-burrow-aarch64-apple-darwin.tar.gz",
      "size": 4718592,
      "sha256": "…",
      "signature": "…"
    }
  ]
}
```

| 字段 | 用途 |
| --- | --- |
| `version` | 语义化版本；与当前版本比较决定是否更新（**只升不降**，见 § 6） |
| `channel` | 与请求的渠道不符时忽略该清单（防止 beta 清单被 stable 用上） |
| `assets[].target` | 与 `rustc` 的 host triple 精确匹配；匹配不到 → 报「本平台暂无更新」而不是猜 |
| `assets[].sha256` | **必校验**。清单本身由 HTTPS 保护，sha256 防的是「资产被替换/截断」 |
| `assets[].signature` | minisign/zipsign 签名。**可选但强烈建议**：只有它才能证明「是我们签的」而不是「同一个 GitHub 账号发的」 |
| `assets[].size` | 下载前展示、下载后校验（防止被塞一个巨型文件） |

`latest.json` 由 CI 生成（[`adr-0007`](../decisions/adr-0007-release-pipeline.md)），
**每个 release 一份，同时也在 `stable` 的 `latest` 重定向里可拿到**。

### 2.3 信任链

```
HTTPS(GitHub/镜像) ──▶ latest.json ──▶ sha256(资产) ──▶ 签名(资产) ──▶ 解压 ──▶ 替换
        传输完整性         清单完整性         内容完整性        来源真实性
```

- `sha256` 由 CI 从**它自己构建的资产**算出，写在清单里 → 改了资产就必须改清单；
- 签名（minisign 私钥在 CI secrets 里）→ 即使 GitHub 账号/镜像站被拿下，也无法伪造新版本；
- 校验和**不是**签名（清单自己也可能被改），这一点在 README 里对用户明说。

---

## 3. 检查（check）

| 项 | 规则 |
| --- | --- |
| 触发 | 服务启动后延迟 5 分钟做第一次；之后每 `check_interval_hours`（默认 24）一次 |
| 节流 | 记录 `lastCheckAt` 到状态文件。**进程被反复重启也不会把检查放大**（`self_update` 的 `UpdateCheckGuard` 或自研 stamp） |
| 网络失败 | 退避：失败后 1h → 6h → 24h（上限），日志只在 `warn` 打一行，不刷屏 |
| 代理 | 从配置读（**不读环境变量**：系统服务不继承用户环境），支持 `http://user:pass@host:port`（只支持 HTTP CONNECT，无 SOCKS） |
| 无更新 | 一条 `info`（`update.check`：`available=false`），不动任何文件 |
| 渠道 | `stable` / `beta`；用户切到 `beta` 后可以再切回 `stable`（不做自动降级，见 § 6） |
| 手动 | 控制面 `updateCheck{force}`（GUI 的「检查更新」按钮）忽略节流 |

---

## 4. 应用（apply）

> 输入是一个 **`UpdateContext`**（由 `peon-burrow` 组装）：`install_dir` / `restart: RestartStrategy` /
> `channel` / `base_url` / `proxy` / `require_signature` / `pubkeys`。
> **update 不自己去问配置或服务** —— 安装目录只有 `service` 知道（用户级与系统级不同路径），
> 重启策略有 4 种组合；依赖倒置之后 update 可以用假 context 单测（丁2）。

```
① 下载到 <data-dir>/update/<version>/<asset>    （临时目录，失败即删）
② 校验 size → sha256 → 签名（配置要求时缺失 = 失败）
③ 解压到 <data-dir>/update/<version>/staging/  （只取二进制，路径白名单，防 zip-slip）
④ 冒烟测试：跑 `staging/<exe> version --json`，输出里的版本必须 == 清单 version
⑤ 原子替换安装目录里的二进制（见 § 5）
⑥ 记 `update.apply`（from/to/asset/verified）→ 重启服务（见 § 6）
```

| 步骤失败 | 行为 |
| --- | --- |
| ① 下载中断 | 删除半成品；下次重来。**不重试到爆** |
| ② 校验失败 | **拒绝**并删除下载物，`error` 日志写明「校验和不匹配」；原二进制毫发无损 |
| ③ 解压异常（含路径穿越） | 同上；路径必须在 staging 目录内，否则直接拒绝 |
| ④ 冒烟测试失败 | 同上（这是「下到了一个能跑但不是我们的东西」的最后一道闸） |
| ⑤ 替换失败（文件被锁 / 权限不足） | 保留原二进制，回滚已 rename 的文件，`error` 日志给出路径与原因 |
| ⑥ 重启失败 | 见 § 6 的兜底：靠服务管理器的失败重启把新版本拉起来 |

> ⚠️ **步骤 ④ 是有意加的**：Windows 上「文件替换成功但新二进制起不来」是最难排查的一类故障，
> 而且此时旧版本已经被挤掉。先跑 `version` 能把「架构不对 / 缺 DLL / 被截断」挡在替换之前。

---

## 5. 替换正在运行的自己（Windows 的关键）

Windows **不允许删除**正在运行的可执行文件，但**允许重命名**它。`self-replace` 用的正是这一点：

```
1) 把 burrow.exe 重命名为 burrow.exe.old-<pid>
2) 把新二进制复制到 burrow.exe
3) 用一个 FILE_FLAG_DELETE_ON_CLOSE 的自身副本作为「清道夫」，等父进程退出后删掉 .old
```

由此推出两条**必须写进代码注释**的事实：

| 事实 | 后果 |
| --- | --- |
| `update()` 返回成功后，**正在运行的仍然是旧代码** | 必须立刻退出（或调用 `restart_with`）——否则用户以为更新了，其实没有 |
| 安装目录里任何**被加载的 DLL/资源**都会阻止重命名 | 中继必须是**单一自包含 exe**（无运行时依赖、无旁挂 DLL）；这也是我们不用 OpenSSL 的原因之一 |

`self_update` 的 `restart()`：Windows 上 **spawn 新进程 + 退出（PID 变化）**；
Unix 上 `exec`（PID 不变）。任何「等旧 PID 消失」的辅助进程都必须容忍这一点。

---

## 6. 重启服务（三平台统一模型）

**核心思想：不要自己重启自己，让服务管理器做。**

```
           ┌── 服务（旧版本）───────────────────────────────┐
           │ 1. 替换二进制（§ 5）                            │
           │ 2. 记日志 update.apply                          │
           │ 3. 以「非正常退出」的方式停止自己                │
           └────────────────────────────────────────────────┘
                             ▼
           ┌── 服务管理器（系统自带）───────────────────────┐
           │ Windows SCM : ServiceFailureActions → 重启     │
           │ systemd     : Restart=always → 重启            │
           │ launchd     : KeepAlive{SuccessfulExit:false}  │
           │ 任务计划程序: 任务的「失败后重启」设置          │
           └────────────────────────────────────────────────┘
                             ▼
                     新版本进程接管监听
```

| 平台 / 模式 | 怎么让它「失败退出」 | 重启由谁做 |
| --- | --- | --- |
| Windows 系统服务 | `set_service_status(Stopped, exit_code = 5（`ExitCode::RestartRequested`）, wait_hint)` | SCM 的 failure actions（安装时配置：重启 5s、重置周期 86400s、后续 2 次各 5s/10s） |
| 任务计划程序（用户级默认） | 退出码 5（`ExitCode::RestartRequested`） | 任务的「失败后重新启动」设置（`RestartOnFailure`）——若无此设置，则退化为「下次登录才起」（**必须配置**，写进验收） |
| systemd | `exit(4)` | `Restart=always` + `RestartSec=2` |
| launchd | `exit(4)` | `KeepAlive = { SuccessfulExit = false }`（只在非零退出时重启） |

**为什么不用「spawn 一个辅助进程去启动新版本」作为主路径**：
辅助进程要拉起服务，在 Windows 上就要么提权（`sc start` 需要权限）、要么绕过 SCM 直接裸跑
（那就脱离了服务管理，服务状态显示会错乱）。让**系统自己**重启，语义最干净、权限最少。

**例外（唯一需要辅助进程的场景）**：**用户级 + 任务计划程序但没有配失败重启**的机器。
此时 `peon-burrow-update` 会 spawn 一个 detached 的
`burrow --apply-restart --wait-pid <old-pid>` 辅助进程：
它等旧进程退出 → 用任务计划程序的 `/Run` 或直接 spawn 新二进制 → 退出。
这条路径在安装时就该被避免（安装器必须配上失败重启），所以它只是兜底。

### 6.1 更新与「正在工作的连接」

更新会重启服务 → 所有 watch 连接断开。扩展侧本来就有重连（
`openWatchChannel` 的退避重试，`mail-peon/src/adapters/mail/transport/watch.ts:101`），
所以**不需要**特殊处理；但要在日志里明确写清「因更新重启，扩展会在数秒内自动重连」，
免得用户看到「reconnecting」以为坏了。

---

## 7. 与桌面端 / 控制面的关系

| 场景 | 行为 |
| --- | --- |
| GUI 显示版本 | 控制面 `version`；`updateCheck` 返回是否可更新 |
| 用户点「立即更新」 | 控制面 `updateApply` → 服务执行 § 4 并重启；GUI 会看到连接断开，应显示「正在更新并重启」而不是「错误」 |
| GUI 自己更新 | ❌ 不做（桌面端不检查更新）。用户重装新安装包即可 |
| 服务未运行时更新 | CLI `update apply` 直接替换文件（没有「运行中的自己」这个问题），下次启动即新版本 |

---

## 8. 验收

| # | 断言 |
| --- | --- |
| 1 | 清单 URL 只用固定重定向 / 滚动 tag，**不调用任何 GitHub API**（用抓包或依赖审计验证） |
| 2 | sha256 不匹配 → 拒绝替换，原二进制可正常启动，日志含「校验和不匹配」 |
| 3 | 清单里的签名无效 → 拒绝（配置要求签名时） |
| 4 | 清单里没有本平台的 `target` → 报「本平台暂无更新」，不猜、不下载 |
| 5 | 下载被截断（size 不符）→ 拒绝 |
| 6 | 解压含 `../` 路径 → 拒绝（zip-slip） |
| 7 | 冒烟测试失败（塞一个空文件当二进制）→ 拒绝替换 |
| 8 | 替换成功后**旧进程立刻退出**，服务管理器在 10 秒内拉起新版本，`status` 里的版本变成新的 |
| 9 | 更新过程中扩展的 watch 会在 30 秒内自动重连成功（真机验收） |
| 10 | 检查失败（断网）→ 退避生效，日志不刷屏，24 小时内不重复打同样的错 |
| 11 | 进程被反复重启 10 次 → 检查次数不放大（节流生效） |
| 12 | 配置了镜像 `base_url` → 清单与资产都从镜像拉取（断掉 github.com 仍能更新） |
| 13 | 系统服务模式：代理从配置读取（清空 `HTTP_PROXY` 环境变量后仍生效） |
| 14 | 旧版本号 > 清单版本号（渠道回退场景）→ **不降级**，日志说明原因 |
