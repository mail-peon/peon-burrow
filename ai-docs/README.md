# peon-burrow — 开发文档

> 本目录是**本仓库**（core）的文档。姊妹仓库（桌面端 GUI）有自己的 `ai-docs/`：
> 本地 [`../../peon-hall/ai-docs/`](../../peon-hall/ai-docs/README.md) ·
> GitHub `peon-hall`。
>
> 📌 **稳定性分级与 semver 政策**在仓库根 [`../STABILITY.md`](../STABILITY.md)（首发即生效）。
>
> 写法沿用 `mail-peon` 仓库的约定：**结论 → 理由 → 反例（踩过的坑）**，关键处标 ⚠️。
> 目标读者是「下一个接手的人 / AI 协作者」，要求「看完即可着手写」。

---

## 目录

### 项目层

| 文件 | 说明 |
| --- | --- |
| [00-overview.md](./00-overview.md) | 目标 / 非目标、交付物、术语、**安全与隐私边界**、成功判据 |
| [01-architecture.md](./01-architecture.md) | 运行时拓扑、组件划分、数据流、不变量、文件位置、产物矩阵 |
| [02-tech-stack.md](./02-tech-stack.md) | crate 选型（含版本、许可证、维护状态、每个坑） |
| [03-roadmap.md](./03-roadmap.md) | 阶段划分、每阶段验收、当前进度 |
| [04-parity-node-to-rust.md](./04-parity-node-to-rust.md) | **TS 中继逐条梳理 + Rust 落点**（重写时的对照表，最重要的一篇） |
| [05-release-and-versioning.md](./05-release-and-versioning.md) | 版本与 tag 规则、与桌面端的版本绑定、发布渠道 |
| [implementation-order.md](./implementation-order.md) | **按依赖排的文件清单**：先写哪个文件、写完怎么验证、两条布局守卫 |
| [modules.md](./modules.md) | 每个 crate 的职责、公开 API、依赖规则、测试落点 |
| [service-lifecycle.md](./service-lifecycle.md) | 服务生命周期：安装/自启/崩溃恢复/自更新后的重启 |
| [testing.md](./testing.md) | 测试策略：单测、mock IMAP、TLS 回声、三平台矩阵 |
| [release.md](./release.md) | **发版 runbook**（本地打 tag → CI → 产物核对） |

### 设计（`design/`）

| 文件 | 说明 |
| --- | --- |
| [wire-protocol.md](./design/wire-protocol.md) | **扩展 ↔ 中继 的线上协议（冻结）**：URL、JSON 字段、关闭码 |
| [port-and-discovery.md](./design/port-and-discovery.md) | 端口策略（默认 41316、可配置、占用时的三种行为、发现文件） |
| [config-schema.md](./design/config-schema.md) | `relay.toml` + CLI + 环境变量三层优先级与校验 |
| [control-plane-ipc.md](./design/control-plane-ipc.md) | 安装器/CLI ↔ 服务 的控制面：通道、命令白名单、鉴权、提权路径 |
| [logging-and-diagnostics.md](./design/logging-and-diagnostics.md) | 日志落盘与滚动、`doctor` 自检项、面向用户的文案规范 |
| [update-flow.md](./design/update-flow.md) | 自更新：清单、镜像站、校验、替换正在运行的自己、重启服务 |

### 决策（`decisions/`）

| # | 文件 | 说明 |
| --- | --- | --- |
| 1 | [adr-0001-two-repos.md](./decisions/adr-0001-two-repos.md) | 两个独立仓库（同一父目录）：tag、类型共享、sidecar 绑定、文档引用 |
| 2 | [adr-0002-rust-rewrite-scope.md](./decisions/adr-0002-rust-rewrite-scope.md) | 重写范围：协议冻结、扩展不动、TS 版留作参照 |
| 3 | [adr-0003-service-model.md](./decisions/adr-0003-service-model.md) | 服务模型：默认用户级自启，可选系统服务；提权边界 |
| 4 | [adr-0004-port-default.md](./decisions/adr-0004-port-default.md) | 默认端口 41316 与「绝不静默换端口」 |
| 5 | [adr-0005-self-update.md](./decisions/adr-0005-self-update.md) | 自更新方案：清单 + 镜像站 + 校验 + 自我替换 |
| 6 | [adr-0006-desktop-installer.md](./decisions/adr-0006-desktop-installer.md) | 桌面端形态（Tauri 安装器、不做自更新、提权路径）——**core 要提供的接口由它定义** |
| 7 | [adr-0007-release-pipeline.md](./decisions/adr-0007-release-pipeline.md) | 发版流水线：tag 触发、三平台矩阵、`latest.json` |
| 8 | [adr-0008-library-first-layout.md](./decisions/adr-0008-library-first-layout.md) | **library-first 布局**：crate 边界与稳定性分层（取代 `-app` 与伞 crate） |
| 9 | [adr-0009-crates-io-publishing.md](./decisions/adr-0009-crates-io-publishing.md) | **发布到 crates.io**：发哪些、顺序、binstall 元数据、semver-checks 门禁 |

---

## 阅读顺序

1. **第一次读**：[`00-overview.md`](./00-overview.md) → [`01-architecture.md`](./01-architecture.md) → [`03-roadmap.md`](./03-roadmap.md)
2. **要动中继逻辑**：[`04-parity-node-to-rust.md`](./04-parity-node-to-rust.md)（60+ 条不得退化的行为）
3. **要动协议**：先读 [`design/wire-protocol.md`](./design/wire-protocol.md) 的「冻结」声明；改协议 = 改扩展，是**跨仓库破坏性变更**
4. **要动服务 / 安装 / 更新**：[`adr-0003`](./decisions/adr-0003-service-model.md)、[`service-lifecycle.md`](./service-lifecycle.md)、[`design/update-flow.md`](./design/update-flow.md)
5. **要发版**：[`release.md`](./release.md) + [`05-release-and-versioning.md`](./05-release-and-versioning.md)
6. **要动手写代码**：先读 [`implementation-order.md`](./implementation-order.md)（含「哪一步对应哪条 parity 断言」），
   布局铁律见 [`modules.md`](./modules.md) 开头

---

## 引用约定

- 引用本仓库源码：相对路径 + 行号，例如 `crates/peon-burrow-core/src/tunnel.rs:120`
- 引用 TS 参照实现：`` `mail-peon/scripts/imap-relay.ts:685` ``（描述性写法，不写成链接）
- **指向另一个仓库**：写「仓库名 + 路径」的纯文本，**并**给出本地相对链接，例如
  `` 见 peon-burrow 仓库的 `ai-docs/design/wire-protocol.md`（本地：[`wire-protocol.md`](./design/wire-protocol.md)） ``
  —— GitHub 上跨仓库链接会失效，所以纯文本那半句不能省
- 本地路径基准（两个仓库并排时）：

  | 从 | 到 `mail-peon` 扩展仓库 | 到 `peon-hall` 仓库 |
  | --- | --- | --- |
  | `ai-docs/*.md` | `../../../mail-peon/…` | `../../peon-hall/…` |
  | `ai-docs/design/*.md`、`ai-docs/decisions/*.md` | `../../../../mail-peon/…` | `../../../peon-hall/…` |

- 「现状 / 目标」必须分开写：本仓库大量文档描述**尚未实现**的东西，照抄成既成事实会让后来者找不到实现（`mail-peon` 的 `relay-deployment.md § 2.3` 就踩过，见该文件开头的 ⚠️）
- 每篇设计文档尽量包含：**目标 / 契约 / 流程 / 失败模式 / 验收**

---

## 当前状态

> 最近更新：**文档骨架完成，代码未开始**。

| 阶段 | 内容 | 状态 |
| --- | --- | --- |
| P0 | 文档（本目录 + `peon-hall/ai-docs`） | 🟡 进行中 |
| P1 | 中继内核（协议对齐 TS 版）+ 回归测试 20 条 | ⬜ |
| P2 | 配置 / 日志 / `doctor` / 服务宿主 / 控制面 | ⬜ |
| P3 | 自更新 | ⬜ |
| P4 | CI 三平台发版 + `latest.json` | ⬜ |
| P5 | 与 `mail-peon` 扩展联动（默认端口、文案）+ 真邮箱验收 | ⬜ |
| P6 | 删掉 TS 实现（**必须在扩展切换默认端口之后**） | ⬜ |

详见 [`03-roadmap.md`](./03-roadmap.md)。
- [`decisions/adr-0010-implementation-revisions.md`](./decisions/adr-0010-implementation-revisions.md) —— **落地时的偏差总表**（优先于被点名段落）
