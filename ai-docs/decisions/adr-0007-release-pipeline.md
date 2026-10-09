# ADR-0007 · 发版流水线：tag 触发、三平台矩阵、静态清单

- **状态**：已采纳
- **影响面**：`.github/workflows/`、打包脚本、`latest.json`、与桌面端仓库的衔接
- **相关**：[`05-release-and-versioning.md`](../05-release-and-versioning.md)、[`release.md`](../release.md)、[`adr-0005`](./adr-0005-self-update.md)、[`adr-0001`](./adr-0001-two-repos.md)

---

## 背景

作者其它 Rust 仓库（`cargo-bumpp`、`harbor`、`bmux-cli`、`crate-plugin-kit`）已经有一套稳定的约定。
本仓库**沿用**它，只在必要处扩展（我们要额外产出**自更新清单**，并且有一个**姊妹仓库**要取产物）。

已确认的既有约定（来自对这些仓库的实际检查）：

| 约定 | 内容 |
| --- | --- |
| workflow 文件名 | `ci.yaml`（`Checks`）、`release.yaml`（`Release`）、`assets.yaml`（`Assets`，可复用） |
| `ci.yaml` 触发 | **只有 `workflow_call`**（没有 `push`/`pull_request`/`workflow_dispatch`） |
| `release.yaml` 触发 | **只有** `push: tags: ["v*.*.*"]` |
| 权限 | 顶层 `permissions: contents: read`；只在需要写 release 的 job 上提到 `contents: write` |
| 顺序 | `checks` → (`assets`, `publish`) 用 `needs:`，结构上保证「先检查后发布」 |
| 工具链 | `rustup toolchain install stable --profile minimal`；**没有 `rust-toolchain.toml`**；MSRV 由独立 job 从 `Cargo.toml` 的 `rust-version` 读 |
| 缓存 | `Swatinem/rust-cache@v2`（`cargo package` 相关 job 例外） |
| 矩阵 | **一个 native runner 一个 target**，不用 `cross` / `zigbuild`；macOS **分架构，不做 universal** |
| 上传 | `gh release create … \|\| true` + `gh release upload "$tag" dist/* --clobber`（`gh` CLI，无第三方 action） |
| 校验和 | 独立 job 生成**一个** `SHA256SUMS`，上传后再**确认它在 release 上** |
| 归档 | `{crate}-{target}.tar.gz` / `.zip`（**不带版本号**，二进制 + `LICENSE` 在归档根） |
| CHANGELOG | **没有**（提交信息用 Conventional Commits；Release notes 由 tag/PR 组成） |
| 版本 / tag | 本地 `cargo bumpp <level>`，提交 `chore: release v{version}`，推 tag 是唯一发版触发 |
| 发布到 crates.io | 用 `harbor` 编排（**要发布**，7 个 crate；见 [`adr-0009`](./adr-0009-crates-io-publishing.md)） |
| `workflow_dispatch` | 不用；发布失败就重跑 job（tag 不变） |

## 决策

### 1. 触发：`push: tags: ["v*.*.*"]`

因为两个项目已经是**两个仓库**（[`adr-0001`](./adr-0001-two-repos.md)），
**不再需要 `core-v*` / `desktop-v*` 前缀** —— 每个仓库各自用 `v*.*.*`，与作者其它仓库一致。

### 2. 三个 workflow

```
.github/workflows/
├── ci.yaml        # name: Checks      on: workflow_call only      （测试矩阵 + MSRV）
├── release.yaml   # name: Release     on: push tags v*.*.*        （checks → assets → manifest）
└── assets.yaml    # name: Assets      on: workflow_call only      （矩阵构建 + 上传 + SHA256SUMS）
```

`release.yaml` 的结构（**顺序是结构性的，不是靠时序**）：

```yaml
name: Release

on:
  push:
    # 唯一触发点：`v<major>.<minor>.<patch>` tag。
    tags: [ "v*.*.*" ]

permissions:
  contents: read

jobs:
  checks:
    name: Checks
    uses: ./.github/workflows/ci.yaml

  assets:
    name: Archive
    needs: [ checks ]
    uses: ./.github/workflows/assets.yaml
    permissions:
      contents: write       # 被调用方只能收窄，不能放宽

  manifest:
    name: Update manifest
    needs: [ assets ]
    runs-on: ubuntu-latest
    permissions:
      contents: write
```

### 3. 矩阵：一个 native runner 一个 target

| runner | target | 说明 |
| --- | --- | --- |
| `windows-latest` | `x86_64-pc-windows-msvc` | 一等公民 |
| `ubuntu-latest` | `x86_64-unknown-linux-gnu` | 用 gnu 而不是 musl：本仓库不是「必须静态」的单文件分发，glibc 版足够，且省掉一整套工具链 |
| `macos-15-intel` | `x86_64-apple-darwin` | ⚠️ `macos-13` 已退役，`macos-15-intel` 是它的替代（作者仓库注释里写明了） |
| `macos-15` | `aarch64-apple-darwin` | Apple silicon 镜像 |
| `windows-11-arm` | `aarch64-pc-windows-msvc` | **待核**：GitHub 的 ARM Windows runner 可用性 |
| `ubuntu-24.04-arm` | `aarch64-unknown-linux-gnu` | **待核**：同上 |

```yaml
    strategy:
      fail-fast: false
      matrix:
        include:
          - runner: windows-latest
            target: x86_64-pc-windows-msvc
          - runner: ubuntu-latest
            target: x86_64-unknown-linux-gnu
          - runner: macos-15-intel
            target: x86_64-apple-darwin
          - runner: macos-15
            target: aarch64-apple-darwin
    runs-on: ${{ matrix.runner }}
```

**arm64 的处理原则**：如果 ARM runner 不可用，**不引入 `cross` / `zigbuild`**（会打破
「native runner per target」这条约定，也带来链接器的隐藏差异），而是：
① 首版只发 x64（Windows/Linux）+ 双架构 macOS；② 在 README 里写明架构支持矩阵；
③ 等 runner 可用后再加两行矩阵。

> ⚠️ `rustls` **不支持 32 位 macOS**。不要因为「看起来应该支持」就加 `i686-apple-darwin` 进矩阵。

### 4. 构建与归档脚本（可本地复现）

```
scripts/
├── package-release.sh     # Unix：cargo build --release --locked --target <triple> → dist/<crate>-<triple>.tar.gz
├── package-release.ps1    # Windows：同上 → dist/<crate>-<triple>.zip
└── gen-manifest.sh        # 生成 latest.json（新增，作者生态没有）
```

要点（抄 `harbor` 的做法）：

- `--locked`：同一个 tag 谁构建都得到同一套依赖；
- 归档里**只放两个条目**：二进制（改名成 `burrow[.exe]`）+ `LICENSE`，放在**归档根**；
- 暂存目录用 `mktemp -d` / 临时目录，避免把目录结构带进归档；
- CI 里额外校验脚本本身可解析：`bash -n` + PowerShell 的 `[scriptblock]::Create`。

### 5. 上传：`gh` CLI，容忍竞态

```yaml
      - name: Upload it to the tag
        shell: bash
        env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        run: |
          set -euo pipefail
          tag="${{ github.ref_name }}"
          # 每个 matrix leg 都会尝试创建 release：已存在、以及抢输竞态都不是失败
          gh release view "$tag" >/dev/null 2>&1 \
            || gh release create "$tag" --title "$tag" --notes "Release $tag" \
            || true
          gh release upload "$tag" dist/* --clobber
```

### 6. `SHA256SUMS` + **上传后确认**

沿用作者仓库的写法：独立 job → 下载全部资产（**bare asset names**，这样
`sha256sum -c SHA256SUMS` 在用户下载目录里直接可用）→ 生成 → 上传 → **确认它在 release 上**。

⚠️ 「确认」这一步不是仪式：`bmux-cli` 的注释里写着，`gh` 在**没有 checkout 的 job** 里
必须显式给 `env: GH_REPO: ${{ github.repository }}`，否则报 `fatal: not a git repository`
—— 而这种错误**在 tag 已经推出去之后**才被发现。

### 7. `latest.json`（本仓库新增的一步）

```
manifest job:
  gh release download "$tag" --pattern 'peon-burrow-*' --clobber
  → 对每个资产算 sha256 + size
  → 生成 latest.json（schema/version/channel/releasedAt/notesUrl/assets[]）
  → （可选）minisign/zipsign 签名
  → gh release upload "$tag" latest.json --clobber
```

渠道规则：

| 渠道 | 资产放哪 |
| --- | --- |
| `stable` | 正常 release（`latest` 重定向会指向它，前提是**不发预发布**） |
| `beta` | 预发布 release + 一个滚动 tag `beta-latest`，`latest.json` 也放一份在那里 |

详见 [`design/update-flow.md § 2`](../design/update-flow.md)。

### 8. 发布到 crates.io（**已反转**）

第一轮写的是「不发布」，理由是「这是二进制产品，不是给第三方用的库」。
**该前提已不成立**：本项目定位为标准库，`service` / `update` 是可直接复用的工具，
桌面端也要从 crates.io 取类型。

现在发布 **7 个 crate**（`testkit` / `examples` 为 `publish = false`），顺序与门禁
（`harbor` 编排、`cargo-semver-checks`、`--dry-run`）见
[`adr-0009`](./adr-0009-crates-io-publishing.md)。本节的其余部分（tag 触发、三平台矩阵、资产上传、`SHA256SUMS`）不变。

### 9. 与桌面端仓库的衔接

**顺序：先推 core 的 tag → CI 出资产 → 再发 desktop。**

| 依赖点 | 说明 |
| --- | --- |
| `peon-burrow-ipc-types` 的 git 依赖 | desktop 的 `Cargo.toml` 钉在 core 的某个 **tag** 上 → 该 tag 必须先存在（推了 tag 就有） |
| sidecar 二进制 | desktop CI 用 `gh release download` 取（`gh` 对公开仓库不需要 token） |
| 版本记录 | peon-hall 仓库的 `core-version.txt` 由人工或脚本更新；CI 可用 `core_ref` 覆盖 |

⚠️ **一个容易踩的点**：`peon-burrow-ipc-types` 的破坏性改动必须先发 core（minor 版本），
desktop 再跟进 —— 反之会出现「desktop 引用了还不存在的 tag」。

## 后果

### 好的

- 与作者其它仓库完全一致的骨架 → 维护心智统一，排错经验可迁移；
- tag 是唯一发版入口 → 「谁发的、从哪个 commit 发的」永远可查；
- `gh` CLI + `--clobber` → 失败重跑天然幂等（不像 crates.io 那样不可重来）；
- `latest.json` 与 `SHA256SUMS` 都在 release 上 → 自更新与手动校验共用一份事实。

### 代价（如实记录）

| 代价 | 说明 |
| --- | --- |
| 没有 `workflow_dispatch` | 想「重新跑一遍」只能从 Actions 页面重跑 job（且 tag 不能移动 —— 移动了要 `cargo bumpp --retag` 强推，作者仓库文档里就是这么写的） |
| 手写 workflow 比 `cargo-dist` 啰嗦 | 换来的是与既有仓库一致 + 能给 `latest.json` 加签名 |
| 归档不带版本号 | 下载区里同名文件会被 `--clobber` 覆盖 → 靠 GitHub 的 `releases/download/<tag>/<asset>` 路径区分版本 |
| 没有 CHANGELOG 文件 | 沿用作者生态的做法（Release notes 由 tag/PR 组成）；用户想看变更要去 Releases 页 |

### 否决的方案

| 方案 | 否决理由 |
| --- | --- |
| `cargo-dist` 0.32.0 | 与既有仓库风格不一致；管不了「带签名的自更新清单」与「跨仓库取二进制」；且有一个未关闭的 Windows 临时目录清理 bug |
| `release-plz` / `cargo-release` | 都是版本/CHANGELOG 工具，不产三平台资产；`cargo-bumpp` 已覆盖本地打 tag |
| 一个 workflow 里做完 checks + 构建 + 上传 | 失败重跑会重做全部；分开后可以只重跑 `assets` |
| `actions/upload-release-asset` 之类第三方 action | `gh` CLI 已经够；少一个供应链依赖 |
| 用 `cross`/`zigbuild` 做交叉编译 | 打破「native runner per target」约定；链接器差异带来的隐藏问题不值得 |

## 后续

1. 确认 `windows-11-arm` / `ubuntu-24.04-arm` 的可用性与稳定性（首版可以先只发 4 个 target）；
2. ARM 版发布后，自更新清单要多两个 `target` 条目 —— `peon-burrow-update` 的 target 匹配逻辑要能处理「清单里有、本机不匹配」的情况（已设计，见 update-flow § 2.2）；
3. 代码签名（Windows OV/EV、macOS notarize）与 `latest.json` 签名是**两件事**：前者解决 SmartScreen/Gatekeeper，后者解决更新通道真实性。首版至少做后者；
4. `harbor` 已引入（`release.yaml` 的 `publish` job），发布顺序与门禁见 [`adr-0009`](./adr-0009-crates-io-publishing.md)。
