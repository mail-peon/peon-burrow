# ADR-0009 · 发布到 crates.io：发哪些、按什么顺序、怎么防翻车

- **状态**：已采纳（2026-10）
- **影响面**：CI（新增 publish job）、版本纪律、每个 crate 的元数据、桌面端的依赖方式
- **取代**：[`adr-0007 § 决策 8`](./adr-0007-release-pipeline.md)「**不发布到 crates.io**」那条
- **相关**：[`../../STABILITY.md`](../../STABILITY.md)、[`../release.md`](../release.md)、[`adr-0008`](./adr-0008-library-first-layout.md)

---

## 背景

本项目定位是**标准开源库**：库要被别人依赖、CLI 要能 `cargo install` 装上，
所以必须发布到 crates.io（而不是只发 GitHub Release 资产）。

第一轮布局时写的是「不发布」——理由是「二进制产品不是给第三方用的库」。
那个前提已经不成立（见 [`adr-0008`](./adr-0008-library-first-layout.md)），本条取代它。

## 决策

### 1. 发布 7 个 crate，2 个不发

| crate | 发布 | 说明 |
| --- | --- | --- |
| `peon-burrow-protocol` | ✅ | 稳定契约，别的语言实现客户端时的参考实现 |
| `peon-burrow-core` | ✅ | 引擎，可嵌入 |
| `peon-burrow-ipc-types` | ✅ | 控制面类型 |
| `peon-burrow-ipc` | ✅ | 控制面传输 + 客户端 |
| `peon-burrow-service` | ✅ | 通用「装成服务」工具 |
| `peon-burrow-update` | ✅ | 通用自更新工具 |
| `peon-burrow` | ✅ | **产品 crate**：lib + `[[bin]] burrow` → 用户装这个 |
| `peon-burrow-testkit` | ❌ `publish = false` | 发布它等于承诺一套测试 API |
| `peon-burrow-examples` | ❌ `publish = false` | 只是示例 |

### 2. 发布顺序（依赖倒序）

```
protocol → core → ipc-types → ipc → service → update → peon-burrow
```

**首次发布必须按这个顺序手动走一遍**（registry 上还没有依赖版本时，后面的 crate 无法通过校验）。
之后交给 `harbor` 编排（它的 preflight 会算顺序、也会在「版本已在 registry 上」时视为完成 —— 重跑安全）。

```yaml
- uses: cargo-bins/cargo-binstall@v1.25.1
- name: Install the release tool
  run: cargo binstall harbor --version 0.1.4 --no-confirm --disable-strategies compile
- name: Check the release before starting it
  env: { CARGO_REGISTRY_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }} }
  run: cargo harbor check --plan "$GITHUB_REF_NAME"
- name: Publish
  env: { CARGO_REGISTRY_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }} }
  run: cargo harbor publish
```

沿用作者其它仓库的约定：token **只走 `env:`**（不进 argv）、**不加 `environment:`**、
**publish job 不挂 rust-cache**（缓存过 `target/package` 会引发清理报错）。

### 3. 每个 crate 的元数据（crates.io 上看得见的部分）

| 项 | 值/做法 |
| --- | --- |
| `license` | `MIT`（[`../../STABILITY.md § 7`](../../STABILITY.md)） |
| `repository` / `homepage` | `https://github.com/mail-peon/peon-burrow`（放 `[workspace.package]` 继承） |
| `readme` | **每个 crate 自己一份 `README.md`**（crates.io 渲染它，不能只靠仓库根） |
| `keywords` / `categories` | 各 crate 按定位给（`relay` / `websocket` / `tcp` / `imap` / `service` / `self-update` / `command-line-utilities` / `network-programming`） |
| `rust-version` | MSRV，继承 `[workspace.package]` |
| `docs.rs` | `[package.metadata.docs.rs] all-features = true`（让文档覆盖 feature 分支） |
| `[package.metadata.binstall]` | `peon-burrow` 必须有 `bin-dir = "{ bin }{ binary-ext }"`（crate 名 ≠ bin 名） |

### 4. 两道防翻车闸

| 闸 | 工具 | 时机 |
| --- | --- | --- |
| API 兼容性 | `cargo-semver-checks`（对稳定层 6 个 crate） | CI 的 `Checks` job |
| 打包可发布性 | `cargo publish --dry-run -p <crate> --locked` | `release.yaml` 的 `assets` 之后、`publish` 之前 |

`cargo-semver-checks` 把「破坏性改动必须 minor/major」从**纪律**变成**门禁** ——
crates.io 上版本不可复用、yank 不可撤销，所以这条不能靠自觉。

### 5. 标签、版本与产物

- tag 仍是唯一的发布触发点：`push: tags: ["v*.*.*"]`；
- 版本号唯一来源：`[workspace.package] version`（`cargo-bumpp` 改它，所有 crate 继承）；
- GitHub Release 资产**不变**（三平台归档 + `latest.json` + `SHA256SUMS`）：CLI 的安装有两条路，
  `cargo binstall peon-burrow`（走 crates.io 元数据 → 我们的 Release 资产）与手动下载归档；
- crates.io 与 GitHub Release **同一次 tag 一起发**：`release.yaml` 里 `publish` 与 `assets` 并列 `needs: [checks]`。

### 6. 桌面端（`peon-hall`）的依赖方式随之简化

| 阶段 | `peon-hall` 怎么依赖 |
| --- | --- |
| 首次发布前 | git 依赖 + tag 钉版（`{ git = "...", tag = "v0.1.0" }`） |
| 首次发布后 | **版本依赖**（`peon-burrow-ipc = "0.1"`），不再需要 git 依赖与 Cargo.lock 锁 commit |

⚠️ 二进制（sidecar）仍从 **GitHub Release** 取（`core-version.txt` 记 tag）——
crates.io 不提供预编译产物，两条线分工不变。

## 后果

### 好的

- 库的接入成本降到一行 `cargo add`；CLI 的安装降到 `cargo install peon-burrow`；
- `service` / `update` 这两个通用工具能被别的项目直接复用（这是本项目对外价值最高的部分）；
- 桌面端依赖从 git+tag 变成正常版本依赖，跨仓库绑定更简单；
- `cargo-semver-checks` 让「稳定层」的承诺可执行。

### 代价（如实记录）

| 代价 | 说明 |
| --- | --- |
| **发布不可逆** | 版本号不可复用、yank 不能撤回 → 发布前必须 `--dry-run` + semver-checks（决策 4） |
| 7 个 crate 要维护 README/元数据 | 用 `[workspace.package]` 继承公共字段，只写各自独有的 |
| 首次发布要手动按顺序走 | 之后 `harbor` 接管 |
| 稳定层 API 进入公开承诺 | 想清楚再 `pub`；`pub(crate)` 优先 |

### 否决的方案

| 方案 | 否决理由 |
| --- | --- |
| 不发布（第一轮的结论） | 前提变了：这是库项目，不是二进制产品的附赠 |
| 只发 `core` | `core` 依赖 `protocol`；且 `service`/`update` 的复用价值被埋掉 |
| 发布 `testkit` | 等于承诺一套测试 API |
| 双许可 `MIT OR Apache-2.0` | 作者其它仓库是 MIT；双许可多两份文件与 SPDX 配置 |
| 用 `cargo publish` 逐个手发（长期） | 顺序与重试要自己管；`harbor` 已有 preflight 与「已发布即视为完成」的语义 |

---

## 后续

1. **发布前逐个核 crate 名是否可用**（`peon-burrow` 已核过是空的；`peon-burrow-*` 其余待核）；
2. 若将来 `peon-burrow-protocol` 需要给非 Rust 接入方用，考虑产出 JSON Schema 或 IDL（不是现在的需求）；
3. 首个 `1.0` 的时机：稳定层 API 经过至少两个 minor 版本没有破坏性改动之后再定。
