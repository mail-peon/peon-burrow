# 稳定性与版本政策（STABILITY）

> `peon-burrow` 是一个**库项目**：它给外部提供可依赖的 Rust crate，同时也提供一个独立的
> `burrow` 命令行工具。这份文件说明**哪些 API 可以被依赖**、以及我们怎么保证不突然破坏它。
>
> 相关：[`adr-0008`（library-first 布局）](./ai-docs/decisions/adr-0008-library-first-layout.md)、
> [`adr-0009`（发布策略）](./ai-docs/decisions/adr-0009-crates-io-publishing.md)、
> [`modules.md`](./ai-docs/modules.md)。

---

## 1. 三层稳定性

| 层 | crate | 承诺 |
| --- | --- | --- |
| **稳定（Stable）** | `peon-burrow-protocol`、`peon-burrow-core`、`peon-burrow-ipc-types`、`peon-burrow-ipc`、`peon-burrow-service`、`peon-burrow-update` | 公开 API 遵守 [SemVer](https://semver.org/)；破坏性改动只在 **major**（`0.x` 期间为 **minor**）发布，且必须有迁移说明 |
| **产品（Product）** | `peon-burrow`（lib + bin `burrow`） | **命令行行为**稳定（`burrow run|service|status|doctor|update|version` 的语义、退出码、配置文件字段）；**lib API 不承诺稳定**（它是产品内部结构，公开只为测试与复用） |
| **内部（Internal）** | `peon-burrow-testkit`、`examples/` | 不发布、不承诺；随仓库演进 |

> 一句话：**要依赖就依赖稳定层的六个 crate**；需要 CLI 就 `cargo install peon-burrow`。

---

## 2. SemVer 具体规则

| 改动 | 版本位 |
| --- | --- |
| 稳定层删/改公开项、改语义、改错误类型 | `0.x`：**minor**；`≥1.0`：**major** |
| 稳定层新增公开项、新增 feature（默认关） | `0.x`：**patch**；`≥1.0`：**minor** |
| 仅内部/产品层变化、文档、性能 | **patch** |
| 协议形状变化（`protocol` 的报文、关闭码语义） | **minor 起** + 升 `WATCH_PROTOCOL_VERSION`（见 § 5） |

**弃用流程**：先用 `#[deprecated(since = "...", note = "...")]` 标记，**至少跨一个小版本**，
下一个 minor 才允许删除稳定层的项。

---

## 3. MSRV 政策

- MSRV 写在 `Cargo.toml` 的 `rust-version`（**不放 `rust-toolchain.toml`** —— 那会让 CI 的 MSRV job 失效）；
- 提高 MSRV 属于 **minor** 变更，写进 release notes；
- CI 有独立 job 用 `rust-version` 构建一遍（含 `--all-targets`）；
- 依赖下限要钉住（形如 `clap = "~4.5.61"`）：**Cargo 无法表达「取满足 MSRV 的最老版本」**，
  所以只能靠钉版本 + MSRV job 兜底。

---

## 4. Feature 政策

| 规则 | 说明 |
| --- | --- |
| **默认 feature 只增不减** | 关掉一个默认 feature = 让别人的构建行为静默变化 |
| 新功能默认**关** | 想用的人显式开 |
| `peon-burrow-core` 的 `imap-watch` **默认关** | 库用户拿到的是「纯字节隧道」，不会被 IMAP 逻辑牵连 |
| `peon-burrow`（CLI）的 `imap-watch` **默认开** | 否则 `burrow` 起来之后 mail-peon 那条链路会静默失效（这是产品行为，不是库行为） |
| feature 之间不许有循环、不许隐式开启别的 crate 的默认 feature | 用 `default-features = false` + 显式列 feature |

---

## 5. 协议版本

| 常量 | 位置 | 管什么 | 兼容规则 |
| --- | --- | --- | --- |
| `WATCH_PROTOCOL_VERSION` | `peon-burrow-protocol` | 扩展 ↔ 中继（`__watch` 报文） | 缺字段视为 `1`（向后兼容）；变更加一，且必须同时改 `mail-peon` 扩展 |
| `IPC_PROTOCOL_VERSION` | `peon-burrow-ipc-types` | GUI/脚本 ↔ 运行中的中继 | 客户端报更高的 `v` → `protocol-too-new`，由 GUI 提示升级中继 |

协议改动的完整流程见 [`ai-docs/design/wire-protocol.md`](./ai-docs/design/wire-protocol.md)。

---

## 6. 发布不可逆（发布前必读）

| 事实 | 后果 |
| --- | --- |
| crates.io 上「crate + 版本」**不可复用** | 发错了不能重发同一个版本号，只能发下一个 |
| yank 只是下架，**不能撤回**已下载 | 依赖过坏版本的构建仍然会成功 |
| 一旦发布，稳定层 API 就进入 SemVer 承诺 | 发布前必须过 `cargo-semver-checks` 与 `--dry-run` |

清单与流程见 [`ai-docs/release.md`](./ai-docs/release.md) 与
[`adr-0009`](./ai-docs/decisions/adr-0009-crates-io-publishing.md)。

---

## 7. 许可证

**MIT only**（与作者其它项目一致）。所有 crate 都是 `license = "MIT"`，
仓库根 `LICENSE` 为唯一文本；不用 `MIT OR Apache-2.0` 双许可（避免双份文件与 SPDX 配置的额外维护）。
