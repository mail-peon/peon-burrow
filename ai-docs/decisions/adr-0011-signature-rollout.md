# ADR-0011 · 签名算法拍板 + 首发降级路径

- **状态**：已采纳
- **影响面**：`peon-burrow-update`、`peon-burrow` 的 `UpdateContext` 装配、`release-pipeline` CI
- **承接**：[`adr-0005-self-update.md`](./adr-0005-self-update.md) § 3「Q3 三层校验」、§ 7「首版没有签名工具链时要降级」

---

## 背景

`adr-0005 § 3` 把签名称为「强烈建议项」，并写下了两条**未决**问题：

> § 7 后续 1. **签名工具链：minisign 还是 zipsign**（`self_update` 的 `signatures` feature 原生支持 zipsign）→ **落地前拍板**，倾向 zipsign（与 `self_update` 集成最顺）；
> § 7 后续 2. **公钥轮换策略**：清单里放多把公钥（`pubkeys: []`），首版先单把。

但**实际落地**（[`adr-0010-implementation-revisions.md`](./adr-0010-implementation-revisions.md)）做了两件大事：

1. **不引 `self_update` 也不引 `reqwest`**：自写薄 HTTPS GET（`update/src/download.rs`） + 自写签名 trait
   `SignatureVerifier`（`update/src/verify.rs`）。理由见 `adr-0010 § 5`（避免 C 工具链 / 不绑发行方式）。
2. **默认 `require_signature = true`、`fail closed`**：`UpdateContext::default()` 就是 `true`，
   调用方必须**显式**改 `false` 才能不验签（`update/src/lib.rs:221-237`）—— 这与 `adr-0005 § 3` 的建议一致。

这两个调整让签名工具链的拍板**变成了选择 verifier 注入什么**，而不是选择 lib 依赖。

## 问题

| # | 问题 |
|---|---|
| Q1 | 签名算法用 minisign 还是 zipsign（或自签 RSA / ed25519）？ |
| Q2 | 公钥怎么管？首版用单把还是直接支持轮换？ |
| Q3 | 签名工具链还没就绪时，**首发**应该走哪条路径？ |
| Q4 | 签名落地后，回滚 / 旧版不验证签名 = 让它自己跑？ |

## 决策

### 1. Q1：**zipsign**

| 项 | 选 / 不选 |
|---|---|
| **zipsign** | **✅ 采用** |
| minisign | ❌ |
| 自签 ed25519（裸 `ed25519-dalek`） | ❌ |
| 签名密钥：OpenPGP | ❌（CI 已用 GPG，但那是给 git commit 的，**不**复用给二进制 —— 密钥用途不能混） |

**选 zipsign 的理由**：

1. **格式简单**：`signature` 是 hex 字符串，签名流程 `zipsign --sign < secret.key < file > file.sig`，
   校验 `zipsign --verify < public.key file.sig < file`。对接 CI 一行命令。
2. **签名长度固定**：base64 编码后 ~136 字节（ed25519），放进 `latest.json` 不会让清单变臃肿。
3. **密钥轮换的硬性背书简单**：`pubkeys: []` 是数组，加 / 减公钥就是改清单 —— 与
   `minisign` 的多公钥模型一致，但 zipsign 的工具链（`cargo install zipsign`）一行装好。
4. **不走 minisign**：minisign 的密钥格式里塞了「密钥 ID」，CI 里要保留这个 ID 才能在更新里
   引用 —— 增加一个对密钥生命周期的耦合，我们想要的是「CI 只管 secret blob，路径只想公钥 blob」。

**实现路径**：

- `update/Cargo.toml` 加 `zipsign = "0"`（**不**放进 `peon-burrow-update` 的稳定层公开 API —— 仅 dev-dep，给 tests 用）
- `peon-burrow/src/update.rs` 装配 `UpdateContext` 时给一个 `ZipsImpl { public_keys: ctx.pubkeys.clone() }`
- `ZipsImpl::verify(asset, bytes)` 用 `zipsign::Verifier` 校验 `asset.signature`
- 选 zipsign 不引 `zipsign-cli`：CLI 是给 CI 用的，校验侧只需要库函数

### 3. Q3：**首发走「无签名 + release notes 警告」路径**

```
v0.1.0（首发）
├── CI：生成 SHA256SUMS，但**不**生成 *.sig
├── burrow：UpdateContext { require_signature: false, ..Default::default() }
├── 文档：release notes 明写「未签名升级通道，仅靠 sha256 + HTTPS 防中间人」
└── README：风险段落醒目

v0.2.0
├── CI：SHA256SUMS + zipsign 签名一起生成
├── burrow：require_signature = true 默认开
└── 旧 v0.1.0 升级到 v0.2.0 时签名二进制本身就是自己，**自签仍验签通过**
   （首版不接受 v0.1.0 → v0.2.0 的升级，但 v0.2.0 → v0.3.0 验签链路正常）
```

**为什么不首版就上签名**：

| 原因 | 说明 |
|---|---|
| 工具链未就绪 | zipsign 私钥要进 CI secrets、CI 要装 `zipsign` 命令、清单格式加 `signature` 字段 —— 一套三件事 |
| **首版不应该因为「签名的链」调试期出部署事故** | 升级失败 = 用户邮箱连不上 = 真实伤害。把「签名机制」与「首发稳定性」解耦 |
| 与 crates.io 首发惯例一致 | `cargo` 自带 `cargo install` 不验签名；只有「升级中继这种可以默默改二进制」的场景才必须签 |

**为什么 `v0.1.0` 的 risk 仍然可控**：

- 中继**不**自动降级到旧版本（`adr-0005` 否决的方案）—— 升级失败 = 留下旧版 = 用户邮箱照常收信
- 签名升级链路是**新功能**，与中继本身功能解耦 —— 首版验证 v0.1.0 → v0.1.x（同样无签名）的链路即可
- 中继必须经过的「闸 4」是「staging 二进制冒烟测试」（`update/src/smoke.rs`）：`staging/<exe> version --json` 不返回正确结果 = 拒绝替换 → 即使下载到了坏二进制也不会进生产

### 4. Q4：旧版不验证签名 = 让它自己跑

| 情况 | 行为 |
|---|---|
| v0.1.0（无签名）发现 v0.2.0（已签名） | v0.1.0 的 `require_signature = false` → 下载 + sha256 通过就升级 |
| v0.2.0（已签名）发现 v0.3.0（已签名） | 验签生效 → 正常升级 |
| v0.2.0 发现签名验证失败 | **`UpdateError::SignatureInvalid`**，不替换原二进制 |

### 5. 公钥轮换策略

`pubkeys: Vec<String>` 数组，**首版单把**。轮换步骤：
1. CI 加新私钥 → 用新私钥签 v0.4.0+ → 清单里 **同时**含新旧两把公钥
2. 用户升到 v0.4.0 → 清单里有两把公钥都能验证它（虽然它只需要一把）
3. v0.5.0 CI 加**吊销列表**字段 → 旧公钥从清单移除
4. v0.6.0+ 只能验新公钥

**首版不实现吊销**——单把公钥足以应对「私钥丢了」之外的常见情况。

## 后果

### 好的

- 首版不被签名工具链绑定，能**快发**（S14 验收清单可以聚焦在「中继核心稳定性」而不是「签名链调试」）；
- 默认 `require_signature = true`（已落地）让 `fail closed` 是「忘了装 verifier」而非「忘了配 require」；
- zipsign 是单文件 CLI，CI 集成一行命令，不会引入额外工作流；
- v0.2.0 加签名是**纯增量**：清单加字段 + CI 加命令 + 默认开，`peon-burrow` 的 `update.rs` 多 ~30 行 `ZipsImpl`。

### 代价（如实记录）

| 代价 | 说明 |
|---|---|
| **首版有窗口期** | v0.1.0 ~ v0.2.0 之间，升级通道靠 sha256 + HTTPS。这是「信任 GitHub + 信任镜像站」的窗口期 |
| **签名密钥要单独管理** | 不能复用 git commit 的 GPG 密钥（用途不同）；需要单独的 ed25519 私钥进 CI secrets |
| zipsign 私钥丢了 = 无法发可信更新 | **必须**备份到 1Password / Bitwarden 等专用地方，且与 `repo` 的 admin 权限**分人**保管 |
| v0.2.0 升级首发 | v0.1.0 → v0.2.0 的升级**会变成「无签名升级到带签名」** —— 这是安全的，但要在 release notes 里说清 |

### 否决的方案

| 方案 | 否决理由 |
|---|---|
| 首版就用 `require_signature = true` + zipsign | 工具链未就绪 + CI secrets 还没配 + 调试期出部署事故的概率高 |
| 用 minisign | 密钥 ID 引入额外生命周期耦合；与 zipsign 比工具链没有明显优势 |
| 复用 git commit 的 GPG key | 用途语义不同（commit 签名 ≠ 二进制签名）；GPG key 暴露在所有开发者本机，安全范围更糟 |
| 「v0.2.0 之前不开放给用户用」 | v0.1.0 已经能跑、用户群已经在用；推迟 = 用户手动重装的痛苦 |
| `cargo update`-style 强推升级 | `adr-0005` 已否决（用户不会理解「昨天好好的，今天回退了」） |

## 后续

| # | 事项 | 何时做 |
|---|---|---|
| 1 | zipsign 私钥生成 + CI secrets 注入 | v0.2.0 之前（`S14`） |
| 2 | CI 在 `assets.yaml` 加 zipsign 签名步骤 | v0.2.0 之前 |
| 3 | `update.rs` 装配 `ZipsImpl` + `pubkeys` 从配置读 | v0.2.0 之前 |
| 4 | `verifier` 缺失时给开发者清晰报错（已经有，但 release notes 抄一遍） | v0.2.0 发版时 |
| 5 | 吊销列表字段 + 工具 | v0.5.0+（不在首发门槛里） |

---

## 显式状态

**S14 验收清单 + 首签项**：

- [x] zipsign 算法拍板（本 ADR）
- [x] 首发降级路径定下（v0.1.0 不签名 + release notes 警告）
- [x] 后续 v0.2.0 启用签名的路径定下
- [ ] 实施：zipsign 私钥生成 + CI 接入（v0.2.0）
- [ ] 实施：`require_signature` 默认值切换（v0.2.0）
- [ ] 真机 beta：用 v0.1.0 升 v0.2.0 演练一次签名链路（v0.2.0 验收）