# ADR-0005 · 自更新：静态清单 + 镜像站 + 校验 + 自我替换

- **状态**：已采纳
- **影响面**：`peon-burrow-update`、CI 的资产与清单生成、服务重启模型、配置项 `[update]`
- **完整设计**：[`design/update-flow.md`](../design/update-flow.md)

---

## 背景

用户需求原文：「核心库要有自更新逻辑，可以检查 GitHub Releases 或镜像站去做自更新，
桌面端是免安装工具，不需要检查更新」。

为什么**必须**有：中继是常驻后台服务，用户装完就再也不会主动看它。
没有自更新 = 内核 bug 只能靠用户重装来修 —— 而用户根本不知道有新版。

为什么**必须**小心：这个进程在内存里握着用户的邮箱授权码。
更新通道被投毒 = 邮箱被拿走。所以「能更新」和「更新安全」是同一件事的两面。

## 问题

| # | 问题 |
| --- | --- |
| Q1 | 怎么发现新版本？（GitHub API？静态清单？） |
| Q2 | 网络不可达 GitHub 怎么办？（镜像站） |
| Q3 | 怎么保证下到的确实是我们的二进制？ |
| Q4 | Windows 上怎么替换**正在运行的自己**？ |
| Q5 | 替换完怎么让新版本跑起来？ |

## 决策

### 1. Q1：**静态清单**，不碰 GitHub API

```
stable: https://github.com/mail-peon/peon-burrow/releases/latest/download/latest.json
beta  : https://github.com/mail-peon/peon-burrow/releases/download/beta-latest/latest.json
```

- `latest` 是 GitHub 的**重定向**（不是 API）：不计 rate limit，且**一个静态 URL**；
- 匿名 API 是 60 次/小时/**IP**，一个办公室共用出口时会被打满；
- 清单里有 `version` / `channel` / `assets[]`（`target` / `size` / `sha256` / `signature`）。

❌ 否决：`/releases` API 列表 + 自己挑最新（rate limit + 镜像站不代理 API）。
❌ 否决：**滚动 tag 作为 stable 渠道**（如 `core-latest`）—— 需要额外维护一个会被强推的 tag；
`releases/latest/download/` 已经免费提供了同样的效果。

### 2. Q2：镜像站只改**前缀**

`[update] base_url = "https://gh-proxy.com/https://github.com"` → 清单与资产 URL 都基于它拼接。
镜像站要代理的只有两类路径：`/releases/latest/download/…` 与 `/releases/download/…`。
（这也是「不用 API」的第二个好处：镜像实现简单。）

### 3. Q3：三层校验，缺一不可

| 层 | 手段 | 防什么 |
| --- | --- | --- |
| 传输 | HTTPS | 路上被看/被改 |
| 内容 | 清单里的 `sha256` + `size` | 资产被替换/截断（含镜像站作恶） |
| 来源 | minisign / zipsign 签名（私钥只在 CI secrets） | 「同一个 GitHub 账号发的假货」/ 账号被拿下 |

**校验和不是签名** —— 清单本身也在 GitHub 上，能改资产的人多半也能改清单。
所以签名是**强烈建议项**：首版默认开启（缺签名 = 拒绝），只有在签名工具链未就绪时才临时降级为「校验和 only」，
且降级要写进 release notes。

### 4. Q4：用 `self-replace` 的 rename-aside 机制（`self_update` 内置）

Windows 允许**重命名**正在运行的 exe，不允许删除：
rename 旧文件 → 写入新文件 → `FILE_FLAG_DELETE_ON_CLOSE` 的自身副本稍后删掉旧的。

由此推出两条硬约束（写进代码注释）：

1. **`update()` 返回后，运行中的仍是旧代码** → 必须立刻退出，否则「以为更新了其实没有」；
2. **安装目录里不能有被加载的 DLL/资源** → 中继保持单一自包含 exe
   （这也是选 `rustls` 而不是 OpenSSL 的原因之一）。

### 5. Q5：重启交给**服务管理器**，不自己拉起自己

更新完成后以**非零退出码**结束，由 SCM failure actions / `systemd Restart=always` /
`launchd KeepAlive{SuccessfulExit=false}` / 任务计划程序的失败重启把它拉起来。

❌ 否决：spawn 辅助进程去 `sc start`（要提权、且绕过 SCM 会让服务状态错乱）。
✅ 兜底：仅在「用户级 + 未配失败重启」的机器上，用 detached 的
`--apply-restart --wait-pid <old>` 辅助进程（安装时就该避免这条路径）。

### 6. 依赖与实现边界

| 用 | 不用 |
| --- | --- |
| `self_update` 1.3.0（`checksums` + `signatures` + `async`），**自定义 `ReleaseSource`** 读我们的清单 | `axoupdater`（绑定 cargo-dist 的安装回执） |
| `self-replace` 1.5.0（兜底/底层） | `tuf` / `tough`（要自建元数据签名仓库，过重；`tuf` 稳定版停在 2017） |
| `reqwest`（`rustls-tls`） | `curl` 子进程（Windows 上不可靠） |

**必须显式设置**（默认值在守护进程里是错的）：

```rust
.no_confirm(true)     // 默认 false：会阻塞在 stdin 问 y/n → 服务态必死
.show_output(false)   // 默认 true：往 stdout 打状态块
```

**不读 `GH_TOKEN` / `GITHUB_TOKEN`**：一个过期的环境 token 会把原本正常的匿名下载变成 401。

> ⚠️ 落地时要确认 `self_update` 的 `ReleaseSource` trait 是公开可实现的；
> 若它被 sealed，就退到 `reqwest` + `sha2` + 签名校验 + `self-replace` 自研那 60 行 ——
> 清单是我们自己的格式，本来就要自己解析。

### 7. 与桌面端的边界

| | 自更新 |
| --- | --- |
| peon-burrow（服务） | ✅ 本 ADR |
| desktop（免安装 GUI） | ❌ **不做**。用户重装新版安装包即可；GUI 只显示当前 core 版本，并可请求 core 立即更新 |

## 后果

### 好的

- 一条固定 URL 覆盖「发现 + 镜像 + 校验」，实现与运维都简单；
- 更新失败**永远保留原二进制**（四道闸：size、sha256、签名、冒烟测试）；
- 「谁重启我」这件事由平台负责，权限需求最小；
- 用户无感：不需要重装、不需要点确认。

### 代价（如实记录）

| 代价 | 说明 |
| --- | --- |
| 自签名密钥要自己管 | minisign 私钥进 CI secrets；丢了就没法发可信更新（所以公钥要能轮换 —— 清单里支持多公钥，首版先一个） |
| **「更新并重启」是本项目自研风险最高的一段** | 四道闸 + 服务管理器重启 + 兜底辅助进程，验收清单 14 条（[`design/update-flow.md § 8`](../design/update-flow.md)） |
| 需要安装目录可写 | 用户级天然满足；系统服务由 LocalSystem 自己写，不需要再提权 |
| 首版没有签名工具链时要降级 | 降级是「校验和 only」，必须在 release notes 里说明 |

### 否决的方案

| 方案 | 否决理由 |
| --- | --- |
| 检查 GitHub API 拿最新版本 | rate limit（按 IP 池化）+ 镜像站不代理 API |
| 只在启动时检查、不节流 | 服务被反复重启会把检查放大（被打成 429） |
| 自动降级到旧版本 | 用户不会理解「昨天好好的，今天回退了」；回退由用户手动装 |
| 用 Tauri 的 updater 更新服务 | `tauri-plugin-updater` 只能更新 Tauri 应用自己的安装包，且 Windows 上会**自动退出应用**；它管不到 sidecar 服务 |

## 后续

1. 签名工具链：minisign 还是 zipsign（`self_update` 的 `signatures` feature 原生支持 zipsign）→ **落地前拍板**，
   倾向 zipsign（与 `self_update` 集成最顺）；
2. 公钥轮换策略：清单里放多把公钥（`pubkeys: []`），首版先单把；
3. `beta` 渠道的实际用途：我们自己先用它验证清单/镜像链路，再对外；
4. 是否需要「延迟更新」（正在同步时不要重启）→ 首版不做，重启只影响 watch 连接，扩展会自动重连。
