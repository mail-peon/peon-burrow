# 03 · 路线图

> 阶段划分原则：**每一步都能独立验收**，且**先把中继本体与 TS 版对齐**，
> 再叠服务化 / GUI / 自更新这些「新东西」。
>
> 理由（沿用 `mail-peon/ai-docs/decisions/relay-deployment.md § 5`）：
> 协议正确性（IMAP、字面量解析、TLS、游标、IDLE）与「用什么语言写、怎么打包」完全无关。
> 在没验证的协议上直接叠服务化 = 两层不确定性。

---

## 阶段总览

| 阶段 | 内容 | 验收（摘要） | 状态 |
| --- | --- | --- | --- |
| **P0** | 文档（本目录 + `peon-hall/ai-docs/`） | 协议冻结文档 + 逐条 parity + 全部 ADR | 🟡 进行中 |
| **P1** | 中继内核（协议对齐 TS 版） | parity § 6 的 **20 条断言**全绿；真邮箱能收信 | ⬜ |
| **P2** | 配置 / 日志 / `doctor` / 服务宿主 / 控制面 | 装成用户级服务、重启机器后仍能收信；`doctor` 13 项检查可用 | ⬜ |
| **P3** | 自更新 | update-flow § 8 的 **14 条验收** | ⬜ |
| **P4** | CI 三平台发版 + `latest.json` | 打 tag → 四个平台产物 + 清单 + SHA256SUMS 都在 release 上 | ⬜ |
| **P5** | 桌面端（`peon-hall` 仓库）+ 扩展联动 | 五种操作可用；扩展默认端口切到 41316 并发布 | ⬜ |
| **P6** | 清理与归档 | TS 实现归档；文档「待实现」标记全部消除 | ⬜ |

---

## P0 · 文档（当前）

| # | 交付物 | 状态 |
| --- | --- | --- |
| 0.1 | [`00-overview.md`](./00-overview.md)（目标/非目标/安全边界/成功判据） | ✅ |
| 0.2 | [`01-architecture.md`](./01-architecture.md)（拓扑/组件/不变量/产物） | ✅ |
| 0.3 | [`04-parity-node-to-rust.md`](./04-parity-node-to-rust.md)（60+ 条行为对照） | ✅ |
| 0.4 | [`design/wire-protocol.md`](./design/wire-protocol.md)（协议冻结） | ✅ |
| 0.5 | [`design/port-and-discovery.md`](./design/port-and-discovery.md) | ✅ |
| 0.6 | [`design/config-schema.md`](./design/config-schema.md) | ✅ |
| 0.7 | [`design/control-plane-ipc.md`](./design/control-plane-ipc.md) | ✅ |
| 0.8 | [`design/logging-and-diagnostics.md`](./design/logging-and-diagnostics.md) | ✅ |
| 0.9 | [`design/update-flow.md`](./design/update-flow.md) | ✅ |
| 0.10 | [`02-tech-stack.md`](./02-tech-stack.md)（含版本/许可证/坑） | ✅ |
| 0.11 | ADR 0001–0007 | ✅ |
| 0.12 | `peon-hall/ai-docs/`（姊妹仓库） | 🟡 |
| 0.13 | 待决项拍板（见文末） | ⬜ |
| 0.14 | 第一轮布局评审 19 条落进文档（单一真相 / 可注入 / 单入口）+ [`implementation-order.md`](./implementation-order.md) | ✅ |
| 0.15 | **library-first 重构**：`protocol` 独立、`peon-burrow` = lib + bin、`imap-watch` 默认关、发布策略 → [`adr-0008`](./decisions/adr-0008-library-first-layout.md) / [`adr-0009`](./decisions/adr-0009-crates-io-publishing.md) / [`../STABILITY.md`](../STABILITY.md) | ✅ |

**P0 完成判据**：parity 文档里每一行都有明确的「必须一致 / 可以不同 / 应该不同」判定；
每篇 design 文档都有「验收」小节；ADR 的每个决策都有「否决的方案 + 理由」。

---

## P1 · 中继内核

**范围**：`peon-burrow-protocol` + `peon-burrow-core`（隧道 + 策略 + `RelayState`；`imap-watch` 用 feature 打开）+ 最小前台入口。**按 [`implementation-order.md`](./implementation-order.md) 的 S0–S5 顺序推进。**

| # | 任务 | 验收 |
| --- | --- | --- |
| 1.1 | workspace 骨架 + **9 个包**的空壳（含 `testkit` / `examples`）+ 布局守卫（[`modules.md § 12`](./modules.md)） | `cargo clippy --all-targets` 干净；守卫脚本通过 |
| 1.2 | URL 解析与策略（token / 白名单通配 / loopback / `tls=0`+993） | 单测覆盖 parity W2–W6、C4–C6 |
| 1.3 | 透传隧道（tokio + rustls + 双向背压） | parity § 6.1 第 1–5 条 |
| 1.4 | 第一帧分流 + 第一帧补投 + 后续帧注册时机 | parity C7–C11、6.1 第 2 条（回声字节数不翻倍） |
| 1.5 | 关闭语义（1008/1011/1001、120 字节截断、双关去重） | parity W10–W12、F1、6.2 第 13 条 |
| 1.6 | watch 状态机（greeting→login→select→idle⇄done） | 6.2 第 10、11 条（mock IMAP） |
| 1.7 | watch 重连与致命错误分类 | 6.2 第 12 条 |
| 1.8 | `max_connections`、`LOGOUT`、IPv6 监听（parity 缺口 I-2/I-4/I-7） | 新增单测 |
| 1.9 | 把 TS 的 9 条断言搬成 `tunnel_e2e.rs` | **`cargo test` 全绿（含 TLS 自签证书两条）** |
| 1.10 | 真邮箱（QQ）收信验收 | 手动 checklist（`mail-peon/ai-docs/decisions/imap-testing.md § 4`） |

> ⚠️ P1 **不做**：服务注册、GUI、自更新、配置文件（只做 CLI/ENV）。
> 目标是把「协议」这件事从 TS 版**完整搬过来并自动化验证**。

---

## P2 · 配置 / 日志 / 服务 / 控制面

| # | 任务 | 验收 |
| --- | --- | --- |
| 2.1 | `relay.toml` + 三层优先级 + 校验（含「非 loopback 必须有 token/白名单」） | config-schema § 7 的 8 条 |
| 2.2 | 发现文件（原子写 + 陈旧检测） | port-and-discovery § 8 的 1、4、7 |
| 2.3 | 端口占用的三种行为 + `doctor --port` | port-and-discovery § 8 的 2、3、6 |
| 2.4 | 日志：文件滚动、结构化事件、凭据脱敏、`trace` 二次确认 | logging § 6 的 1–4、9 |
| 2.5 | `doctor` 13 项检查 + `--json` | logging § 6 的 5–8 |
| 2.6 | `peon-burrow-service`：Windows（任务计划程序 + SCM 可选）、macOS、Linux | 三平台装/卸/启停/自启 + **安装后自检失败重启已配置** |
| 2.7 | 服务宿主：SCM 状态上报、信号处理、优雅关停（先断连接） | ADR-0003 § 3；parity 6.2 第 19 条 |
| 2.8 | `peon-burrow-ipc-types` 类型 + `peon-burrow-ipc` 传输（socket + TCP 退路 + token + 命令白名单） | control-plane § 9 的 1–10 |
| 2.9 | `peon-burrow` 产品层：config（三层合并 + 组合规则 + `Paths` 注入）/ `doctor` 注册表 / 控制面命令映射 / `ExitCode`+`AppError` / `run()` | control-plane § 9 + 命令映射、退出码、doctor 单测 |

**P2 的里程碑验收**：在干净 Windows 机器上
`service install --mode user` → **重启机器** → 打开扩展（手填 `ws://127.0.0.1:41316/`）→ 能收验证码。

---

## P3 · 自更新

| # | 任务 | 验收 |
| --- | --- | --- |
| 3.1 | 清单解析 + 渠道 + target 匹配 | update-flow § 8 的 4、14 |
| 3.2 | 下载 + size/sha256 校验（+ 签名） | § 8 的 2、3、5 |
| 3.3 | 解压白名单（防 zip-slip）+ 冒烟测试 | § 8 的 6、7 |
| 3.4 | 替换正在运行的自己（rename-aside） | § 8 的 8 |
| 3.5 | 服务管理器重启（SCM/systemd/launchd/任务计划程序）+ 兜底辅助进程 | § 8 的 8、9 |
| 3.6 | 节流、退避、镜像 `base_url`、代理（配置，不读环境） | § 8 的 10–13 |
| 3.7 | 控制面 `updateCheck` / `updateApply` | GUI 能显示与触发 |

---

## P4 · CI 与发版

| # | 任务 | 验收 |
| --- | --- | --- |
| 4.1 | `ci.yaml`（3 平台测试矩阵 + MSRV job + fmt/clippy/doc/package） | PR 上全绿 |
| 4.2 | `assets.yaml`（4 target + 打包脚本 + 上传 + SHA256SUMS） | 打 `v0.0.1-rc.1` 能出全部产物 |
| 4.3 | `manifest` job（`latest.json` + 签名） | 清单内容与实测二进制 sha256 一致 |
| 4.4 | `beta` 渠道演练 | 从 `beta` 装 → 切 `stable` → 不降级且有日志 |
| 4.5 | 首次真实发版 `v0.1.0` | 干净机器上：装 → 收信 → 自更新到 `v0.1.1` → 恢复收信 |
| 4.6 | **crates.io 发布**（7 个 crate）+ `cargo-semver-checks` 门禁 | `cargo install peon-burrow` 装上 `burrow`；破坏性改动被 CI 拦住（[`adr-0009`](./decisions/adr-0009-crates-io-publishing.md)） |

---

## P5 · 桌面端 + 扩展联动

| # | 任务 | 仓库 |
| --- | --- | --- |
| 5.1 | Tauri 骨架 + 状态卡片（四种组合）+ 五个操作 | `peon-hall` |
| 5.2 | 控制面客户端（`peon-burrow-ipc`：发布前 git 依赖 / 发布后版本依赖）+ `doctor` 展示 | `peon-hall` |
| 5.3 | sidecar 打包（`core-version.txt` → 下载 → `externalBin`） | `peon-hall` |
| 5.4 | 提权辅助（安装/卸载系统服务、自启开关） | `peon-hall` + `peon-burrow` |
| 5.5 | 桌面端 CI：三平台安装包 | `peon-hall` |
| 5.6 | 扩展默认端口 → `41316`、文案更新、`watch-client.spec.ts` 字面量 | `mail-peon` |
| 5.7 | 端到端：装安装包 → 点「安装服务」→ 扩展里**什么都不填**就能收信 | 三仓库 |

---

## P6 · 清理

| # | 任务 | 前置条件 |
| --- | --- | --- |
| 6.1 | `mail-peon` 仓库把 `scripts/imap-relay*.ts`、`relay-kill.ts`、`relay-*.test.ts` 归档 | 5.6 已发布（扩展默认端口切换完成） |
| 6.2 | `mail-peon/ai-docs/decisions/relay-deployment.md` 更新为「已实现」 | 6.1 |
| 6.3 | 本仓库文档里所有「（待实现）」「计划中」标记清除 | P1–P5 完成 |
| 6.4 | 把端口复核（IANA + 排除区间）写进发版 checklist 并跑一次 | P4 |

---

## 待决（P0 收尾要拍板的）

| # | 待决 | 倾向 | 影响 |
| --- | --- | --- | --- |
| Q1 | 签名方案：minisign vs zipsign | zipsign（与 `self_update` 集成最顺） | `peon-burrow-update`、CI |
| Q2 | 提权辅助形态：第二份 manifest 构建 vs 独立小工具 | 独立小工具 | desktop 产物数 |
| Q3 | `RELAY_TRACE` 在服务态是否开放 + 怎么开 | 控制面临时开（上限 300s，二次确认） | 控制面命令集 |
| Q4 | arm64（Windows/Linux）首版是否发布 | 若 runner 可用则发，否则只发 4 个 target | CI 矩阵、README |
| Q5 | Windows 用户级自启的 `Hidden` XML 最简写法 | 落地时验证；不可行则接受一次黑框并记入已知问题 | `peon-burrow-service` |
| Q6 | `max_connections` 默认值 | 32 | `peon-burrow-core` |
| Q7 | 服务账号（Windows `LocalSystem` vs `LocalService`） | `LocalSystem`（要写自己的目录） | `peon-burrow-service` |
---

## 落地状态（2026-10-09）

> 实现已完成到 S13；S14 只剩**人工验收**（真机 UAC / 重启 / 崩溃恢复 / 真邮箱）。
> 与设计文档的逐条偏差见 [`decisions/adr-0010-implementation-revisions.md`](./decisions/adr-0010-implementation-revisions.md)。

| 阶段 | 状态 | 证据 |
| --- | --- | --- |
| S0 workspace 骨架（9 包） | 完成 | 布局守卫 L1/L2/L3/L5 全过 |
| S1 `protocol`（关闭码 / 目标 / 策略 / watch 报文） | 完成 | 36 单测 |
| S2 `core` 错误与策略 | 完成 | 10 单测 |
| S3 `transport`（TCP/TLS 建连唯一实现） | 完成 | SNI、证书错误映射、注入信任根 |
| S4 `dispatch` / `tunnel` / `state` / `server` | 完成 | 11 条真 TCP 端到端 |
| S5 `watch`（IMAP IDLE 状态机） | 完成 | 6 条端到端（未标记问候、致命不重连） |
| S6 `testkit`（rcgen 证书 / 脚本化 IMAP / 回声） | 完成 | 3 单测，被 core 复用 |
| S7 `ipc-types` + `ipc`（命名管道 / Unix socket / TCP 退路） | 完成 | 15 + 7 单测（含 5 次失败锁定） |
| S8 `service`（三平台 + `probe_port` + 执行缝） | 完成 | 56 测（零改机器）+ 三平台 `--target` clippy |
| S9 `update`（清单 / 四道闸 / 替换运行中的 exe） | 完成 | 65 测（sha256、签名、冒烟、回滚） |
| S10 产品层（config / doctor / control / exit / paths） | 完成 | 三层合并与组合规则全测 |
| S11 CLI + `run` + 13 行 `main` | 完成 | 真二进制：起服务 → status → trace → stop（退出码 0） |
| S12 示例（4 个） | 完成 | `--example watch_protocol` 输出与协议文档逐字一致 |
| S13 CI（3 平台 × lint/test/doc + MSRV + 守卫 + 打包 + 发版） | 完成 | `.github/workflows/{ci,assets,release}.yaml` |
| S14 发版与人工验收 | 待办 | 需要 tag、crates.io token、真邮箱与重启/崩溃恢复演练 |

合计 **299 个测试**通过（含 10 条跨仓库契约测试）。