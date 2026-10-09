# peon-burrow

> `mail-peon` 浏览器扩展的 **IMAP 中继核心**（苦工的地洞）：一条 `WebSocket ↔ TCP/TLS` 的字节隧道 +
> 替扩展挂 `IDLE` 的常驻监听，做成**可自启的系统服务**，自带控制面与自更新。

![peon-burrow 概念图：替扩展挂 IMAP 长连接的中继](.github/images/burrow.png)

浏览器扩展拿不到裸 TCP（MV3 的 Service Worker 只有 `fetch` / `WebSocket`），
所以 IMAP 必须借一条本机隧道；`IDLE` 长连接也只能由隧道进程替扩展挂着。
完整论证在 `mail-peon` 仓库：
[`adr-0005-imap-needs-relay.md`](../../mail-peon/ai-docs/decisions/adr-0005-imap-needs-relay.md)、
[`relay-deployment.md`](../../mail-peon/ai-docs/decisions/relay-deployment.md)。

**本仓库是这条隧道的最终形态。** 原实现是 `mail-peon/scripts/imap-relay.ts`
（单文件 1901 行，esno 直接跑，面向本地调试）；本仓库把它重写成 Rust，
并补上服务化、控制面与自更新。

## 它是什么（给库用户的一句话）

一个**通用的 `WebSocket ↔ TCP/TLS` 中继库**（访问控制、双向背压、可选 IMAP `IDLE` 推送），
外加两件可独立复用的工具：**跨平台服务托管**与**自更新**。
`mail-peon` 扩展只是接入方之一；`burrow` 命令行可以完全独立使用。

| 你会怎么用它 | 命令 / 依赖 |
| --- | --- |
| 装 CLI | `cargo install peon-burrow` → `burrow` |
| 把中继嵌进自己的程序 | `cargo add peon-burrow-core`（+ `peon-burrow-protocol`） |
| 自己写 GUI / 监控 | `cargo add peon-burrow-ipc` |
| 给自家 daemon 加「装成服务」 | `cargo add peon-burrow-service` |
| 给自家工具做自更新 | `cargo add peon-burrow-update` |

稳定性分级：[`STABILITY.md`](./STABILITY.md) · 接入点总表：[`ai-docs/modules.md § 10`](./ai-docs/modules.md)

---

> 名字的由来：`peon` 是魔兽争霸里兽族的苦工（下矿、驮金、往返搬运），
> `burrow` 是苦工的地洞 —— 既是"掘出来的隧道"，也是苦工躲进去干活的地方。
> 二进制叫 `burrow`，仓库叫 `peon-burrow`。

---

## 两个仓库

| 目录（本地并排） | GitHub | 是什么 |
| --- | --- | --- |
| **本仓库** | `mail-peon/peon-burrow` | 中继核心 + 服务 + CLI + 自更新（仓库 `peon-burrow`，二进制 `burrow`） |
| `../peon-hall` | `mail-peon/peon-hall` | Tauri 2 服务安装器 GUI（**独立仓库、独立发版**） |

```
┌──────────────────┐   ws://127.0.0.1:41316/   ┌──────────────────────────────┐
│ mail-peon 扩展    │ ────────────────────────▶ │ burrow（本仓库）              │
│ (MV3)            │ ◀──────────────────────── │ 隧道 · watch · 服务 · 自更新   │
└──────────────────┘                           └──────────────┬───────────────┘
                                                       TCP/TLS │
                                                              ▼
                                                    imap.x.com:993
```

父目录 `peon/` **不是仓库**，只是一个放两个 checkout 的容器。
两个仓库的边界、tag 规则、类型与产物的共享方式见
[`ai-docs/decisions/adr-0001-two-repos.md`](./ai-docs/decisions/adr-0001-two-repos.md)。

---

## 当前状态

> **文档先行阶段：代码尚未开始写。**

| 项 | 状态 |
| --- | --- |
| TS 中继逻辑逐条梳理（60+ 条行为对照） | ✅ [`ai-docs/04-parity-node-to-rust.md`](./ai-docs/04-parity-node-to-rust.md) |
| 线上协议契约冻结 | ✅ [`ai-docs/design/wire-protocol.md`](./ai-docs/design/wire-protocol.md) |
| 架构 / 选型 / 端口 / 服务模型 / 自更新 / 发版 | ✅ [`ai-docs/`](./ai-docs/README.md) |
| 稳定性分层与发布策略（crates.io 7 个 crate） | ✅ [`STABILITY.md`](./STABILITY.md) · [`adr-0009`](./ai-docs/decisions/adr-0009-crates-io-publishing.md) |
| 代码 | ⬜ |
| CI（三平台发版） | ⬜ |

---

## 计划中的用法

```bash
# 前台跑（调试）
cargo run -p peon-burrow -- run --foreground

# 装成用户级自启（不需要提权）
burrow service install --mode user --autostart logon

# 看状态 / 自检
burrow status
burrow doctor

# 自更新
burrow update check
burrow update apply
```

---

## 文档

从 [`ai-docs/README.md`](./ai-docs/README.md) 开始。最该先读的四篇：

1. [`00-overview.md`](./ai-docs/00-overview.md) —— 目标、非目标、安全边界
2. [`04-parity-node-to-rust.md`](./ai-docs/04-parity-node-to-rust.md) —— TS 行为 → Rust 落点（重写的对照表）
3. [`design/wire-protocol.md`](./ai-docs/design/wire-protocol.md) —— 冻结的线上协议
4. [`implementation-order.md`](./ai-docs/implementation-order.md) —— **要写代码先看这个**：按依赖排的文件清单 + 每步验证

## License

[MIT](./LICENSE)
