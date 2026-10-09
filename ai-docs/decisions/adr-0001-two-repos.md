# ADR-0001 · 两个独立仓库（同一父目录）

- **状态**：已采纳
- **影响面**：仓库边界、tag 命名、跨仓库类型共享（crates.io 版本依赖）、桌面端的 sidecar 绑定、文档引用方式
- **相关**：[`adr-0006`](./adr-0006-desktop-installer.md)、[`05-release-and-versioning.md`](../05-release-and-versioning.md)

---

## 背景

要交付两个**独立发版**的产品：

| | 核心 | 桌面端 |
| --- | --- | --- |
| 是什么 | `burrow`：隧道 + watch + 服务宿主 + 控制面 + 自更新 | Tauri 2 安装器 GUI |
| 语言 | Rust | Rust + 前端 |
| CI | 三平台二进制 + `latest.json` | 三平台安装包 |
| 版本节奏 | 跟协议/内核 bug 走，可能很频繁 | 跟 UI 走，慢 |

**用户已明确要求：两者各自是独立 git 仓库。** 本地把它们并排放在同一个父目录里方便开发，
但**父目录本身不是仓库**，也不放任何文件（没有 README、没有 `.gitignore`、没有共享脚本）。

```
D:\Projects\peon\      ← 只是一个容器目录，不是仓库、没有 .git
├── peon-burrow\                              ← git 仓库 A
│   ├── .git\
│   ├── Cargo.toml                     ← workspace 根
│   ├── crates\…
│   ├── ai-docs\
│   └── .github\workflows\
└── peon-hall\                           ← git 仓库 B
    ├── .git\
    ├── package.json
    ├── src\
    ├── src-tauri\
    ├── ai-docs\
    └── .github\workflows\
```

## 问题

拆成两个仓库后，原本「一个仓库」自然解决的几件事要重新安排：

| # | 问题 | 不处理的后果 |
| --- | --- | --- |
| Q1 | 控制面协议类型怎么共享（GUI 与 CLI 必须同源） | 手写两份镜像类型 → 字段一改就漂移，症状是「GUI 显示状态全空」 |
| Q2 | 桌面端怎么拿到 core 二进制（`externalBin`） | 各自编译 → 安装包里的 core 与发布的 core 版本不一致，没法复现 |
| Q3 | tag / 版本号怎么排 | 两边都打 `v0.1.0`，出问题时不知道用户装的是哪个 |
| Q4 | 文档怎么跨仓库引用 | 链接写死在一边，另一边重命名后全部烂掉 |

## 决策

### 1. 仓库划分与命名

**三个仓库都在 GitHub Org [`mail-peon`](https://github.com/mail-peon) 下** ——
扩展仓库也已迁过去：`mail-peon/mail-peon`（本地 checkout 仍是 `D:\Projects\mail-peon`）。

| 本地目录 | GitHub 仓库 | 说明 |
| --- | --- | --- |
| `peon-burrow/` | `mail-peon/peon-burrow` | 本仓库（`peon-burrow-*` crates + `burrow` bin） |
| `peon-hall/` | `mail-peon/peon-hall` | Tauri 2 安装器 |

两个仓库都**不包含**对方：没有 submodule、没有 `path = "../peon-burrow/…"` 这种只在本地成立的依赖。

### 2. tag：各自用 `v<major>.<minor>.<patch>`，不要前缀

拆成两个仓库后，**不再需要 `core-v*` / `desktop-v*` 前缀**（那是单仓方案为区分两条线才需要的）。
两边各自沿用作者其它 Rust 项目的约定：`v*.*.*`，由 `cargo bumpp` 在本地打。

tag 同时承担第二个职责：**它是桌面端钉 core 版本的锚点**（见决策 4）。

### 3. Q1：控制面类型用 **git 依赖 + tag 钉版**

> 📦 **契约是 `peon-burrow-ipc-types`**（纯类型，只依赖 `serde`）；`peon-burrow-ipc` 是**传输**
> （本地 socket + 客户端）。桌面端需要客户端，所以两个都依赖；`core`/`service` 只依赖类型层 ——
> 这样它们不会被拖进 `interprocess`（布局评审 甲1）。
> **首次发布到 crates.io 之后**，桌面端改用版本依赖（`peon-burrow-ipc = "0.1"`），
> git 依赖只是发布前的过渡（[`adr-0009 § 6`](./adr-0009-crates-io-publishing.md)）。

桌面端 `src-tauri/Cargo.toml`：

```toml
[dependencies]
# 控制面协议的唯一实现。钉到 tag，而不是 main：
# ① 安装器与某个 core 版本的协议必须匹配；② 构建可复现。
peon-burrow-ipc = { git = "https://github.com/mail-peon/peon-burrow", tag = "v0.1.0", package = "peon-burrow-ipc" }
```

- **提交 `Cargo.lock`**：git 依赖必须锁到具体 commit，否则每次构建拉到的都是 tag 当时的样子（tag 被强推过就更糟）。
- 本地开发时，如果两个仓库并排存在，可以临时改成路径依赖（`path = "../../peon-burrow/crates/peon-burrow-ipc"`），
  **但不许提交**（提交后 CI 与别人的 checkout 都会找不到路径）。这一点写进桌面端 README 的「本地联调」小节。
- peon-burrow 侧的纪律：**`peon-burrow-ipc-types` 的破坏性改动必须伴随 core 的 minor/major 版本变化**，
  因为它是对外契约（对桌面端而言）。

### 4. Q2：桌面端从 core 的 **Release 资产**取二进制

桌面端 CI 的步骤：

1. 读 `core-version.txt`（仓库内提交的文件，形如 `v0.1.0`；CI 可用输入 `core_ref` 覆盖）；
2. `gh release download v0.1.0 --repo mail-peon/peon-burrow --pattern 'peon-burrow-<triple>.*'`；
3. 解压 → 改名为 Tauri 要求的 `burrow-<triple>[.exe]` → 放进 `src-tauri/binaries/`；
4. 用 `SHA256SUMS` 校验，并把 `version` 打进 release notes。

**先发 core，再发 desktop。** 若某次桌面端只想改 UI，不必发 core —— 沿用 `core-version.txt` 里的版本即可。

### 5. Q3：版本各不相同，靠文档与 release notes 对齐

- core 的版本 = `burrow` 的版本（crate 版本即产品版本）；
- desktop 的版本 = Tauri 应用的版本；
- 桌面端 release notes **必须**写明「本次捆绑 core vX.Y.Z」；
- 兼容性矩阵（哪个 desktop 捆哪个 core）维护在 [`05-release-and-versioning.md § 4`](../05-release-and-versioning.md)。
- 🔎 不引入「两个版本号必须相等」这种约束：它们本来就会不同步，强行对齐只会制造无意义的发版。

### 6. Q4：文档引用

| 场景 | 写法 |
| --- | --- |
| 指向另一仓库的文档 | 用**仓库名 + 路径**的纯文本 + 本地相对链接：``见 peon-hall 仓库的 `ai-docs/01-ui-and-states.md`（本地：[`../../../peon-hall/ai-docs/01-ui-and-states.md`](../../../peon-hall/ai-docs/01-ui-and-states.md)）`` |
| 指向 `mail-peon` 扩展仓库 | 同上（本地路径 `../../../mail-peon/…`） |

本地并排 checkout 时相对链接可用；在 GitHub 上跨仓库链接会失效，所以**纯文本那半句是必须的**。
不要写 GitHub 绝对 URL —— 仓库改名/迁移后没人会回来改。

## 后果

### 好的

- 各自 CI 独立：改一行 GUI 不会触发 core 的三平台矩阵，反之亦然；
- 权限边界清晰（core 的 release 资产是公开只读的，桌面端只需要读它，不需要跨仓库 token）；
- 版本语义清楚：用户装的是「desktop v0.1.0（含 core v0.1.0）」；
- 各自的 ADR / 决策独立演进，互相不阻塞。

### 代价（如实记录）

| 代价 | 缓解 |
| --- | --- |
| **跨仓库的原子改动做不到**（改 `peon-burrow-ipc-types` 字段 + 改 GUI 调用，要两个 PR） | ① 桌面端在 core 发版**之后**才升级依赖；② 破坏性改动走 core 的 minor 版本，桌面端按需跟进 |
| git 依赖需要网络与 tag 存在 | CI 天然有网；本地首次构建也需要网（可接受，与 cargo 的其它依赖一样） |
| 桌面端构建慢一步（先下 core 产物） | 下载的是编译好的归档（几 MB），比现场 `cargo build` 快得多 |
| 文档跨仓库引用不再「点一下就跳」 | 见决策 6 的双写法 |
| 两个仓库要各自维护 LICENSE / README / CI | 内容不同、量很小 |

### 否决的方案

| 方案 | 否决理由 |
| --- | --- |
| 单仓双项目 | **用户已明确要求两个仓库**；且单仓里桌面端的构建会拖累 core 的 CI 矩阵 |
| ~~把契约发布到 crates.io~~ | **已改**：项目定位是标准库，7 个 crate 都要发布（[`adr-0009`](./adr-0009-crates-io-publishing.md)） |
| 在桌面端复制一份控制面类型 | 漂移不可检（JSON 字段写错只在运行时暴露）；违背「契约单点定义」 |
| git submodule | 心智负担大，且 `peon-burrow` 同时是「被依赖的库」与「被下载的产物源」两种角色，submodule 只解决其中一个 |
| 桌面端现场编译 core | 安装包里的 core 与官方发布的 core 不再是同一份产物，出问题无法复现 |

## 后续

1. 类型共享已由 crates.io 版本依赖解决（[`adr-0009`](./adr-0009-crates-io-publishing.md)）；若出现第三个消费方，再评估是否需要第三个仓库；
2. 桌面端是否需要自动拉取「最新 core」而不是钉 `core-version.txt`：**不需要**，钉版更可复现；
3. `mail-peon` 扩展仓库的联动（默认端口、文案）见 [`design/wire-protocol.md § 6`](../design/wire-protocol.md)。
