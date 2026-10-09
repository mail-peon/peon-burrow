# ADR-0008 · library-first 布局：crate 边界与稳定性分层

- **状态**：已采纳（2026-10，第二轮布局评审）
- **影响面**：workspace 结构、公开 API、feature 默认值、示例与文档
- **取代**：第一轮评审里的「`peon-burrow-app` 组装层」与「伞 crate `peon-burrow`」两条
- **相关**：[`../modules.md`](../modules.md)、[`../STABILITY.md`](../../STABILITY.md)、[`adr-0009`](./adr-0009-crates-io-publishing.md)

---

## 背景

第一轮布局评审把本项目当成「mail-peon 插件的配套 daemon」来设计，于是长出两样东西：

1. `peon-burrow-app`：一个只为「把各层拼成这个产品」而存在的 crate；
2. 伞 crate `peon-burrow`：给库用户 `cargo add peon-burrow` 用，而 CLI 放在 `peon-burrow-cli`。

但项目定位是：**一个标准的开源库**（`WebSocket ↔ TCP/TLS` 中继 + 本地服务托管 + 自更新工具集），
`mail-peon` 扩展只是**接入方之一**；同时 CLI 必须能独立使用，且用户安装的就是
`cargo install peon-burrow` → 得到 `burrow`。

这两个前提直接判掉了上面的设计：

- 「`peon-burrow` 是伞 crate、CLI 在别的 crate」→ 与「装 `peon-burrow` 得到 `burrow`」冲突
  （`cargo install` 只装**被安装 crate 自己声明、源文件在自己目录里**的 bin；跨包引用源文件也不被 `cargo publish` 允许）；
- 「`peon-burrow-app`」→ 一个没有对外价值、只为拼装而生的 crate，既不是库也不是产品。

---

## 决策

### 1. 抽 `peon-burrow-protocol` 作为最底层的稳定契约

线上协议（目标解析、`__watch` 报文、关闭码、`WATCH_PROTOCOL_VERSION`）**独立成 crate**，
且**不依赖任何 IO**（不许出现 tokio / rustls / interprocess）。

理由：别的语言实现客户端/服务端时，只需要这一份类型与文档；把它留在 `core` 里，
「只要协议」的接入方就得连 TLS/运行时一起拖进来。

### 2. `core` 只依赖 `protocol`，并提供两个显式接入点

```rust
pub trait Policy: Send + Sync { /* 访问控制可替换 */ }
pub struct TlsConfig { /* roots / verifier 可注入 */ }
RelayServer::start_with(RelayOptions, Arc<dyn Policy>, TlsConfig)
```

`core` **不依赖** `ipc-types`：控制面状态形状属于「产品怎么报告」，由产品层做
`From<&RelayState> for ProcessStatus` 映射（`modules.md § 7`）。这样稳定层之间不互相污染。

### 3. `imap-watch` 是 **feature**，`core` 默认关

| 构建 | 行为 |
| --- | --- |
| `core` 默认 | 纯字节隧道；`__watch` 请求以 `1008` 明确拒绝（不是静默当透传） |
| `core` + `imap-watch` | 启用 IMAP `IDLE` 监听 |
| `peon-burrow`（CLI）默认 | **开**（`default = ["imap-watch"]`）—— 产品行为，不是库行为 |

理由：库用户多数只想要隧道；而我们的 CLI 必须默认具备邮件推送能力（否则 mail-peon 链路静默失效）。

### 4. `service` 与 `update` 保持「与中继无关」

两者都不依赖 `core`/`protocol`，输入都是显式值（`InstallOptions` / `UpdateContext`）。

理由：**「给自家 daemon 做服务安装与自更新」是通用需求** —— 这两个 crate 因此可以直接被别的项目复用，
也是本项目作为「工具集」对外价值最高的部分。

### 5. 一个 `peon-burrow` = lib + `[[bin]] burrow`；不要 `-app` / `-cli` / 伞 crate

```toml
[[bin]] name = "burrow"
[lib]  name = "peon_burrow"     # 产品逻辑（config / doctor / control / exit / run）
[package.metadata.binstall] bin-dir = "{ bin }{ binary-ext }"
```

- **用户视角**：`cargo install peon-burrow` / `cargo binstall peon-burrow` → `burrow` ✓
- **可测试性**：产品逻辑在同一 crate 的 lib 里，`crates/peon-burrow/tests/` 直接测（不牺牲「bin 测不了」那条约束）
- **可扩展**：将来 Windows 提权辅助之类的第二个命令，在同一 crate 再加一个 `[[bin]]`
  （作者的 `cargo-bumpp` 就是 `[lib]` + 两个 `[[bin]]` 的先例）

### 6. 配置（`relay.toml` / ENV / `Paths`）归产品层，不单独出 crate

配置来源是**产品**的事；稳定层只吃值。`service` / `update` 也不需要 `Config`（它们吃显式参数）。
少一个 crate，少一层版本同步。

### 7. `testkit` 不发布；`examples/` 进仓库

- `peon-burrow-testkit`：`publish = false`（发布它等于承诺一套测试 API）；
- `examples/`：**示例即接入文档**（embed / custom policy / plain tunnel / own service / self update / ipc client / testkit）。

### 8. 稳定性分三层，写成 `STABILITY.md`

稳定层（6 个）遵守 SemVer；产品层只承诺**命令行行为**；内部层无承诺。
详见 [`../../STABILITY.md`](../../STABILITY.md)。

---

## 后果

### 好的

- 「装 `peon-burrow` → 得到 `burrow`」这条 UX 与库的可组合性同时成立；
- 稳定层互不反向依赖，任何一层都能单独被外部消费（守卫脚本进 CI）；
- `service` / `update` 从「本项目的零件」变成「可以单独用的工具」；
- 没有只为拼装而生的 crate，`modules.md` 的边界更好解释。

### 代价（如实记录）

| 代价 | 说明 |
| --- | --- |
| crate 数量到 7（+1 内部） | 多 crate 意味着多次发布、多份 README/元数据；用 `harbor` 与 `[workspace.package]` 摊平 |
| `core` 关掉 `imap-watch` 时行为分裂 | 必须在协议层明确「本构建未启用」的拒绝语义（已写进 `modules.md § 2`），否则排查困难 |
| 产品层 lib 也会被发布 | 需在 `STABILITY.md` 明确「不承诺 API 稳定」，避免别人依赖它 |
| `protocol` 独立后，改协议要动两个 crate | 换来的是「只要协议的接入方」不被牵连 |

### 否决的方案

| 方案 | 否决理由 |
| --- | --- |
| 保留 `peon-burrow-app` | 为拼装而生的 crate，无对外价值；由 `peon-burrow` 的 lib 承担即可 |
| 伞 crate `peon-burrow` + `peon-burrow-cli` | 与「装 `peon-burrow` 得到 `burrow`」冲突 |
| `peon-burrow` 只做 bin、逻辑放 `-cli` | 多一层版本同步，且 crates.io 页面会是个空壳 |
| `config` 独立成 crate | 产品配置不是库；`service`/`update` 也不需要它 |
| `imap-watch` 默认开 | 库用户会被 IMAP 逻辑牵连（用户的明确要求是默认关） |
| 按平台拆 `service-*` | 公开 API 相同，拆开只得到三份版本号与 `cfg` 转发 |

---

## 后续

1. `burrow-elevate`（Windows 提权辅助）确认形态：倾向**同 crate 第二个 `[[bin]]`**；
2. 若将来出现「非 IMAP 的推送协议」（如 JMAP 推送），按同样方式做成 feature 或独立 crate；
3. `protocol` 是否要提供 **C/FFI 或 JSON Schema** 以方便非 Rust 接入方 → 等真有需求再说。
