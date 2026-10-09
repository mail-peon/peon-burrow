# 发版 runbook（core）

> 规则见 [`05-release-and-versioning.md`](./05-release-and-versioning.md)，
> 流水线设计见 [`decisions/adr-0007-release-pipeline.md`](./decisions/adr-0007-release-pipeline.md)。
> 这篇只讲**照着敲什么**。

---

## 1. 本地准备（一次性）

```bash
# 发版工具：本地改版本 + 打 tag（与作者其它仓库一致）
cargo install cargo-bumpp

# 确认当前状态
git switch main && git pull
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
```

---

## 2. 常规发版（stable）

```bash
# 1) 改版本 + 提交 + 打 tag + 推 tag（cargo-bumpp 一次做完）
cargo bumpp conventional        # 按 Conventional Commits 决定级别
#   或显式指定： cargo bumpp minor
#
# 它做的事：改 [workspace.package] version → 更新 Cargo.lock →
#          提交 "chore: release v0.2.0" → 打 tag v0.2.0 → push（含 tag）

# 2) 等 CI（唯一触发点是 tag）
gh run watch

# 3) 核对产物
gh release view v0.2.0 --json assets --jq '.assets[].name'
```

期望的资产（与 [`01-architecture.md § 7.1`](./01-architecture.md) 一致）：

```
peon-burrow-x86_64-pc-windows-msvc.zip
peon-burrow-aarch64-pc-windows-msvc.zip      # 若该 runner 可用
peon-burrow-x86_64-unknown-linux-gnu.tar.gz
peon-burrow-aarch64-unknown-linux-gnu.tar.gz # 若该 runner 可用
peon-burrow-x86_64-apple-darwin.tar.gz
peon-burrow-aarch64-apple-darwin.tar.gz
SHA256SUMS
latest.json
```

```bash
# 4) 校验清单与资产一致（防「清单里写了不存在的文件」）
gh release download v0.2.0 --pattern 'latest.json' --clobber
gh release download v0.2.0 --pattern 'SHA256SUMS' --clobber
#    核对 latest.json 里每个 assets[].sha256 == SHA256SUMS 里对应行
```

---

## 3. 发版前 checklist

| # | 检查 | 为什么 |
| --- | --- | --- |
| 1 | `Checks` 三平台全绿（含 MSRV job） | 基线 |
| 2 | **端口复核**：IANA CSV 里没有 `41316`（tcp/udp）；`netsh int ipv4 show excludedportrange protocol=tcp` 不含它 | OS/Hyper-V 的保留区间会变（[`adr-0004`](./decisions/adr-0004-port-default.md)） |
| 3 | `latest.json` 里的 `version` 与 tag 一致 | 自更新靠它比较版本 |
| 4 | `latest.json` 里的 `channel` 正确（stable/beta） | 渠道串了会让 beta 清单被 stable 用户拿到 |
| 5 | 签名有效（把公钥贴进一个临时文件，`minisign -V` 验一遍） | 更新通道的安全性 |
| 6 | 归档解压后**只有两个条目**（二进制 + LICENSE）且在根 | 用户手动解压体验、`cargo-binstall` 兼容 |
| 7 | Windows 归档里的 exe 双击能跑 `version` | 冒烟（架构/依赖问题在这一步暴露） |
| 8 | 兼容性矩阵（[`05 § 4`](./05-release-and-versioning.md)）是否需要加行 | 桌面端要对齐 core |
| 9 | `mail-peon` 那边的默认端口是否已切换（P5 之后） | 决定文档里写哪个地址 |

端口复核一键脚本（Windows）：

```powershell
$csv = (Invoke-WebRequest 'https://www.iana.org/assignments/service-names-port-numbers/service-names-port-numbers.csv').Content
if (($csv -split "`n") -match '^[^,]*,41316,') { '❌ IANA 已分配' } else { '✅ IANA 未分配' }
netsh int ipv4 show excludedportrange protocol=tcp
```

---

## 4. beta 发版（预发布）

```bash
cargo bumpp --preid rc minor      # → v0.3.0-rc.1
# 推 tag 后在 GitHub 上把该 release 标为 Pre-release（或让 CI 依据 tag 里的 '-' 自动标）
```

- 预发布**不会**进入 `releases/latest` → stable 用户不受影响；
- `beta-latest` 滚动 tag 上的 `latest.json` 需要覆盖更新（CI 的 `manifest` job 负责）；
- 演练重点：把一台机器切到 `beta` → 更新 → 再切回 `stable` → 确认**不降级**且有日志说明。

---

## 5. 出问题怎么办

| 情况 | 处置 |
| --- | --- |
| CI 在某个平台挂了 | **不要移动 tag**。修好代码 → 从 Actions 页面**重跑失败的 job**（`gh run rerun <id> --failed`） |
| 需要带上新 commit 重发同一个版本 | 先确认没人装过；然后 `cargo bumpp --retag`（重建并强推 tag）。⚠️ 已装用户的自更新会看到同样的版本号但不同的 sha256 → **能更新成功但语义混乱**，只在「发出去 5 分钟内」用 |
| `latest.json` 传错了 | 手动重新生成并 `gh release upload <tag> latest.json --clobber`；或重跑 `manifest` job |
| 资产缺失 | 重跑 `assets` job（`gh release upload --clobber` 幂等） |
| 发现严重 bug 需要撤回 | **不要删 release**（已装用户的自更新会 404）。发一个更高的补丁版本；必要时把 `latest.json` 指回上一个版本 —— ⚠️ 但 `peon-burrow-update` **不降级**，所以「指回旧版本」只能阻止新用户装坏版本，救不了已更新的用户 → 正解是尽快发补丁 |

---

## 6. 发版后

| # | 动作 |
| --- | --- |
| 1 | 在干净 Windows 机器上装一次（用户级），重启，收一封验证码邮件 |
| 2 | 跑一次 `update apply` 演练（从上一个版本升到本次） |
| 3 | 更新 [`05-release-and-versioning.md § 4`](./05-release-and-versioning.md) 的兼容性矩阵（如需） |
| 4 | 若本次改了 `peon-burrow-ipc`，在 `peon-hall` 仓库升级 git 依赖的 tag 并在那边发版 |
| 5 | 若本次改了协议，更新 [`design/wire-protocol.md`](./design/wire-protocol.md) 并在 `mail-peon` 仓库提对应改动（**协议变更必须两边一起发**） |
