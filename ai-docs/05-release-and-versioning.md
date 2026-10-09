# 05 · 版本与发版策略

> 两个仓库、两条版本线、两条发版流水线。本文只讲**规则**；
> 具体命令与核对步骤看 [`release.md`](./release.md)（core）与
> `peon-hall` 仓库的 `ai-docs/04-release.md`。

---

## 1. 两条版本线

| | core（本仓库） | desktop（姊妹仓库） |
| --- | --- | --- |
| 版本号来源 | `Cargo.toml` 的 `[workspace.package] version` | `package.json` 的 `version`（Tauri 的 `tauri.conf.json` 必须与之相同，见 § 5） |
| tag | `v<major>.<minor>.<patch>`，例 `v0.1.0` | 同名规则，各自仓库内 |
| 触发 | `push: tags: ["v*.*.*"]` | 同名 |
| 产物 | 三/四平台归档 + `latest.json` + `SHA256SUMS` | 三平台安装包 |
| 节奏 | 跟内核/协议 bug 走（可能频繁） | 跟 UI 走（慢） |
| 自更新 | ✅ | ❌ |

⚠️ **两条线不要求版本号相同**。强行对齐只会制造无意义的发版。
它们的关系由**兼容性矩阵**（§ 4）描述。

---

## 2. 为什么各自的 tag 不再需要前缀

单仓方案里需要 `core-v*` / `desktop-v*` 来区分两条线；**两个仓库后不需要了**
（[`adr-0001`](./decisions/adr-0001-two-repos.md)）。各自用 `v*.*.*`，与作者其它仓库一致。

⚠️ 但**发布清单里必须写清**：`peon-burrow v0.2.0` / `peon-hall v0.1.0`。
GitHub Release 标题建议写成 `v0.2.0`（仓库名已经说明了归属）。

---

## 3. 发布渠道

| 渠道 | 触发 | 用户怎么拿到 |
| --- | --- | --- |
| `stable` | 正常 release | `releases/latest/download/latest.json` 重定向 |
| `beta` | 预发布 release（`--prerelease`）+ 滚动 tag `beta-latest` | `releases/download/beta-latest/latest.json` |

规则：

- **预发布版本绝不出现在 `latest` 里**（GitHub 的语义），所以打 `v0.2.0-rc.1` 时
  `stable` 渠道用户**不会**收到它 —— 这正是我们想要的；
- 用户切换到 `beta` 后可以切回 `stable`，但**不会自动降级**（[`design/update-flow.md § 8`](./design/update-flow.md) 第 14 条）；
- 首版发 `beta` 做一次完整演练（下载、校验、替换、重启），再发 `stable`。

---

## 4. 兼容性矩阵

桌面端会**捆绑**一个确定的 core 版本，所以「哪个 GUI 带哪个 core」必须能查。

| desktop | 捆绑 core | `peon-burrow-ipc` 契约 | 备注 |
| --- | --- | --- | --- |
| 0.1.x | 0.1.x | v1 | 首个版本；五种操作 + 诊断 |
| （后续每行在发版时补） | | | |

维护方式：桌面端仓库根提交 `core-version.txt`（记录它默认拉取的 core tag）；
每次桌面端发版，把该文件的值得复制进本表。**只加行，不改历史行。**

---

## 5. 版本号的「多处同步」问题（桌面端特别容易踩）

| 位置 | 谁读 |
| --- | --- |
| `desktop/package.json` | 前端构建、npm 生态 |
| `desktop/src-tauri/tauri.conf.json` 的 `version` | Tauri 打进安装包元数据与文件名 |
| `desktop/src-tauri/Cargo.toml` 的 `version` | Rust 侧 `--version` |
| git tag | CI 触发 |

参考项目里 `w3wright-studio` 就是**四处各写各的**（`0.1.0` / `0.0.0`），
而 `clash-verge-rev` 用了一个 CI 校验（tag 与 `package.json` 必须相等）。

**本仓库的规则**（core）：版本只有一个来源 —— `[workspace.package] version`，
所有 crate 用 `version.workspace = true`；`cargo-bumpp` 改的就是它。

**桌面端的规则**：以 `package.json` 为唯一来源，CI 里加一步
「tag 去掉 `v` 必须等于 `package.json` 的 version」，并在 build 前把该版本注入
`tauri.conf.json`（`tauri-action` 支持 `--config` 覆盖，或用一个 `beforeBuildCommand` 脚本写回）。
**不要**四处手改。

---

## 6. 与 `mail-peon` 扩展的版本关系

扩展与中继之间**没有版本锁**（协议是向后兼容的），但有两个连接点：

| 连接点 | 现状 | 计划 |
| --- | --- | --- |
| 中继地址默认值 | 扩展里写死 `ws://127.0.0.1:8787/`（`providers/imap/index.ts:62`） | P5 改成 `41316`（[`adr-0004`](./decisions/adr-0004-port-default.md)） |
| 协议版本协商 | **没有**版本字段 | `__watch` 请求加 `protocol`（缺省 = 1），`watching` 回包里带 `protocol` / `relayVersion`（[`design/wire-protocol.md § 5`](./design/wire-protocol.md)） |

> 在协商落地之前，「扩展更新了但 exe 没更新」只能靠用户自己发现（表现为行为不一致）。
> 这是 P5 之后要补的一块。

---

## 7. 用户怎么看到版本

| 途径 | 输出 |
| --- | --- |
| CLI | `burrow version` → `0.1.0 (a1b2c3d 2026-10-09)` |
| 控制面 | `version` 命令 → `{version, gitSha, buildTime, protocol}` |
| 发现文件 | `relay.json` 的 `version` |
| 日志 | `relay.start` 事件带 `version` |
| GUI | 状态卡片上直接显示（用户最可能看的地方） |

`gitSha` 与 `buildTime` 由构建脚本注入（`build.rs` 或 CI 的环境变量 +
`option_env!`），用于「用户报的版本对应的到底是哪个 commit」这种排查。

---

## 8. `latest.json` 与安装包的一致性

- 桌面端安装包里内嵌的 core 二进制，其 sha256 **必须**等于 core release 里同名资产的 sha256
  （用 `SHA256SUMS` 校验）；桌面端 release notes 里写清 core 版本；
- core 的 `latest.json` 只描述 **peon-burrow 自己的**资产，不要把安装包写进去
  （否则「自更新」会试图把自己换成 GUI）。

---

## 9. 发版前 checklist（core）

见 [`release.md § 3`](./release.md)。摘要：

1. `cargo fmt --check` / `clippy --all-targets` / `test --locked` / `doc` 全绿；
2. 复核默认端口仍然干净（IANA CSV + `netsh show excludedportrange`）；
3. `cargo bumpp <level>` 改版本 + 提交 + 打 tag + 推 tag；
4. 等 CI：`Checks` → `Archive (4×)` → `SHA256SUMS` → `Update manifest`；
5. 核对 release 上的资产清单与本文件 § 1 的产物矩阵一致；
6. 在**干净机器**上装一次（用户级），重启，收一封信；
7. `update apply` 演练一次（用 `v0.x.y-1` 的 beta 清单）；
8. 补齐 § 4 的兼容性矩阵行（若本次影响桌面端）。
