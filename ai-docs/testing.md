# 测试策略

> 目标：**「Rust 版行为和 TS 版一致」这件事必须是可测的**，而不是靠人肉比较。
> 参照实现与它的 9 条断言在 `mail-peon/scripts/imap-relay.test.ts`。

---

## 1. 分层

| 层 | 位置 | 跑在哪 | 归谁 |
| --- | --- | --- | --- |
| 纯函数单测 | 各 crate 的 `#[cfg(test)]` | 三平台 CI | 快、必过 |
| 集成（隧道/watch/控制面/更新） | `crates/*/tests/` | 三平台 CI | 真 TCP/TLS，但全在本机 |
| 服务层只读测试 | `crates/peon-burrow-service/tests/` | 三平台 CI | 只测查询与自检 |
| 端到端手动 checklist | 文档 | 真机（Windows 为主） | 发版前 |

---

## 2. 从 TS 测试搬过来的东西

`imap-relay.test.ts` 的 9 条断言（清单见
[`04-parity-node-to-rust.md § 6.1`](./04-parity-node-to-rust.md)）搬成
`crates/peon-burrow-core/tests/tunnel_e2e.rs`，**断言文字保持原样**，便于跨版本对照。

| TS 的做法 | Rust 的改法 | 为什么 |
| --- | --- | --- |
| `spawn` 子进程 + `sleep(1200)` 等中继起来 | **同进程** `RelayServer::start(RelayOptions{ port: 0 })` | 不用 sleep、不抢端口、失败直接是测试失败而不是「连接被拒」 |
| 固定端口 `18899/18896/18897/18898` | 全部 `port: 0`，从 `local_addr()` 取 | CI 并行跑时不会互撞（TS 版固定端口是 flaky 的常见来源） |
| `openssl req -x509 …` 生成自签证书（缺 openssl 就跳过断言） | dev-dependency `rcgen` 现场生成 | 不依赖外部命令，**TLS 断言永远会跑**（而不是「跳过」） |
| 用 `spawn` 起第二个「严格证书校验」的中继 | 同进程起第二个实例，不同 `tls_reject_unauthorized` | 证明 `RelayOptions` 是值语义（parity G10） |
| runScript.ts（esno 绕行） | 不需要 | Rust 直接跑 |

> ⚠️ 「缺工具就跳过断言」在 TS 里是合理的妥协，但**第 6 条（默认拒绝自签证书）是安全属性**，
> 跳过它等于安全回归无人看守。Rust 版用 `rcgen` 把它变成必跑项。

---

## 3. 集成测试的基础设施

> 这些工具**统一放在 `peon-burrow-testkit`**（独立 dev-only crate，`publish = false`）：
> `core` / `ipc` / `app` 三处都要用，写在某一个 crate 的 `tests/common/` 里等于另外两处用不到（只能复制）。
> `Paths` 一律用 `Paths::for_test(tempdir)` 注入（L4）—— 测试**不许**碰真实用户目录。

```
crates/peon-burrow-core/tests/
├── common/
│   ├── echo.rs        # 明文 TCP 回声 + TLS 回声（rcgen 自成证书）
│   ├── mock_imap.rs   # 最小 IMAP 服务器：脚本化响应（见下）
│   └── harness.rs     # 起中继（port 0）+ 等就绪 + 连接辅助 + 断言 helper
├── tunnel_e2e.rs      # parity § 6.1 的 9 条
├── policy_e2e.rs      # 关闭码 1008 的三类场景 + 「一个字节都不发」
├── watch_e2e.rs       # parity § 6.2 的 10–12
└── limits_e2e.rs      # max_connections、120 字节截断、双实例并存
```

### 3.1 `mock_imap.rs` 要能演出的剧本

| 剧本 | 用来测 |
| --- | --- |
| `* OK [CAPABILITY IMAP4rev1] ready` → 对 `LOGIN` 回 `A0001 OK` → 对 `SELECT INBOX` 回 `* 3 EXISTS` + `A0002 OK` → 对 `IDLE` 回 `+ idling` → 推 `* 4 EXISTS` | 正常推送（parity 6.2 #10） |
| 问候语用 `* PREAUTH` | 未标记问候的两种形态 |
| **不发问候语**，直接对 `LOGIN` 回 `A0001 OK` | 回归「状态机永远停在 greeting」那个坑（#11） |
| `A0001 NO [AUTHENTICATIONFAILED]` | 致命错误 → `state:"failed"` 且不再重连（#12） |
| 中途 `* BYE` / 直接关连接 | 重连与退避 |
| 只回半行（`* 12 EX` 然后 50ms 后 `ISTS`） | 行缓冲（parity E17） |
| 对 `SELECT` 回 `A0002 NO` | 「无法打开收件箱」致命路径 |

剧本用**脚本数组**表示（`Vec<Step>`：`Expect`/`Send`/`Close`/`Delay`），
而不是写一堆 if —— 这样每个测试只声明「我要的对话」。

### 3.2 等就绪，不要 sleep

```rust
// ✅ 起完就能用：start() 返回时已经在监听
let relay = RelayServer::start(opts).await?;
let addr = relay.local_addr();

// ⚠️ 只有在测试「宿主进程」时才需要等：用轮询控制面 ping 或发现文件
async fn wait_ready(paths: &Paths, timeout: Duration) -> Result<()>;
```

---

## 4. 三类容易写歪的测试

| 类型 | 反例（不要写） | 正例 |
| --- | --- | --- |
| 时序 | `sleep(400ms); assert!(received.len() == 1)` | 用「等到条件成立或超时」的 helper（`wait_for(|| …)`），超时才失败 |
| 大块数据 | 只发 1 KB | 256 KB（parity #3），并校验**逐字节相等** |
| 安全属性 | 「连接失败就算通过」 | 断言**关闭码是 1008**、且**服务器侧没收到任何字节**（parity #14） |

⚠️ 特别提醒：**不要给 flaky 测试加重试**。flaky 在本项目里通常意味着
「等就绪的方式不对」或「时序假设」，加重试只会把它藏起来。

---

## 5. 端到端手动 checklist（发版前）

自动化测不了的（真服务、真安装包、真邮箱、真更新），留在 checklist 里：

### 5.1 真邮箱（沿用 `mail-peon` 的验收清单）

见 `mail-peon/ai-docs/decisions/imap-testing.md § 4`（QQ 邮箱授权码、扩展里怎么填、
六条验收）。本仓库的补充项：

| # | 步骤 |
| --- | --- |
| 1 | 用 `run --foreground` 跑起来，扩展填 `ws://127.0.0.1:41316/`，收一封验证码邮件 |
| 2 | 关掉前台进程，改成服务安装，**重启机器**，不手动做任何事，再收一封 |
| 3 | 让服务崩一次（任务管理器杀进程）→ 1 分钟内自动恢复 |
| 4 | 停掉服务 → 扩展显示「reconnecting」而不是崩掉；启动后自动恢复 |

### 5.2 安装包与更新（P4/P5）

| # | 步骤 |
| --- | --- |
| 1 | 干净 Windows 机器：下载 core 归档 → 解压 → `service install` → 收信 |
| 2 | 发一个 `v0.x.y-1` 的 beta → 该机器自动更新 → 版本变化且收信恢复 |
| 3 | 装 desktop 安装包 → 五种操作与状态卡片都对 |
| 4 | 卸载服务 → 扩展失联 → 日志/配置按选择保留 |

---

## 6. 与 TS 版「对跑」

Rust 版完成后、删 TS 版之前，做一次**行为对拍**：

```
同一个 mock IMAP 服务器 + 同一个 echo 服务器
   ├─ TS 中继在 18899
   └─ Rust 中继在 0（打印实际端口）
对同一个用例脚本（连接、发同样的字节、关闭）比较：
   ① 客户端观察到的字节序列
   ② 中继侧的日志事件序列（TS 是文本日志，Rust 是 json 事件 → 用一个脚本比对语义）
   ③ 关闭码
```

这是「parity 文档写了 60+ 条，但实现还是可能漏」的最后一道网。
不需要全量覆盖，挑 § 3 表里判定为**必须一致**的条目。

---

## 7. CI 上的执行

| job | 内容 | 平台 |
| --- | --- | --- |
| `checks` | `cargo fmt --all --check`、`cargo clippy --all-targets --locked`、`cargo test --locked`、`cargo doc --no-deps` | ubuntu / windows / macos |
| `msrv` | 从 `Cargo.toml` 读 `rust-version`，装该版本后 `cargo build --all-targets` | ubuntu |

约定（沿用作者其它仓库）：

- `RUSTFLAGS: -D warnings` → warning 当错误；
- 不用 `rust-toolchain.toml`（会让 MSRV job 失效）；
- `cargo package` 相关的 job 用独立的 `CARGO_TARGET_DIR`，并在缓存前删掉 `target/package`；
- 测试**不允许访问外网**（自更新测试用本地 HTTP server）；
- 协议层测试（`crates/peon-burrow-protocol/tests/`）**不需要任何网络**：URL 两形态、关闭码、报文往返；
- `imap-watch` 的用例用 `#[cfg(feature = "imap-watch")]` 门控，CI 额外跑一遍
  `cargo test -p peon-burrow-core --no-default-features`（此时 `__watch` 必须被**明确拒绝**，而不是静默当透传）；
- `testkit` 与 `examples` 都是 `publish = false`；
- **两条布局守卫**（防结构漂移，抄 [`implementation-order.md § 9`](./implementation-order.md)）：
  `bin/main.rs ≤ 60 行`、`cargo tree -p peon-burrow-ipc-types` 里不出现 `tokio`/`interprocess`。

---

## 8. 验收

| # | 断言 |
| --- | --- |
| 1 | `cargo test` 在三平台全绿，且**不依赖 openssl / 网络 / 固定端口** |
| 2 | TLS 断言（parity #5/#6）在任何机器上都会执行（不再有「跳过」分支） |
| 3 | 单测可以在同进程里并存两个中继实例（证明无全局状态） |
| 4 | 每个 `pub` 项都有 doc 注释（`cargo doc -D warnings` 通过） |
| 5 | 服务层的只读测试在 CI 上不需要提权 |
| 6 | 自更新测试覆盖 update-flow § 8 的 1–7、10、14（8、9 属手动清单） |
| 7 | 没有 `sleep` 出现在测试的断言路径上（review 检查；`Delay` 只出现在 mock 剧本里） |
