# 实现顺序（按依赖排的文件清单）

> 目的：**先定布局、再写代码**。这份清单回答「第一步动哪个文件、写完怎么验证」。
>
> 依据：两轮布局评审（甲/乙/丙/丁/戊 19 条 + library-first 重构）已全部落进文档 ——
> 铁律见 [`modules.md`](./modules.md) 开头，稳定性分级见 [`../STABILITY.md`](../STABILITY.md)，
> 行为清单见 [`04-parity-node-to-rust.md`](./04-parity-node-to-rust.md)。

---

## 0. 铁律速查

| # | 铁律 | 检查 |
| --- | --- | --- |
| L1 | 稳定层不许依赖产品层 | `modules.md § 12` 的守卫脚本 |
| L2 | `ipc-types` 只依赖 `serde`；`ipc` 才有传输依赖 | `cargo tree -p peon-burrow-ipc-types` |
| L3 | 默认值只在 `core` 的 `RelayOptions::default()` | `modules.md § 2` |
| L4 | `Paths` / 时钟 / 进程探测一律注入 | grep `ProjectDirs` 只命中 `peon-burrow/src/config/paths.rs` |
| L5 | bin 只有 40 行，逻辑在同 crate 的 lib | `wc -l crates/peon-burrow/src/main.rs` |

---

## 1. 阶段表

| 步 | 内容 | 产出 | 验证 |
| --- | --- | --- | --- |
| S0 | workspace 骨架（8 个包 + `examples/`） | `Cargo.toml` ×9 | `cargo metadata --no-deps` |
| S1 | **`protocol`**：协议类型与规则 | `crates/peon-burrow-protocol/src/*` | `cargo test -p peon-burrow-protocol` |
| S2 | `core`：纯函数层 | `error.rs`、`policy.rs`、`close`（来自 protocol） | `cargo test -p peon-burrow-core --lib` |
| S3 | `core`：传输 | `transport.rs` | 单测：SNI 规则、自签证书拒绝 |
| S4 | `core`：隧道与状态 | `dispatch.rs`、`tunnel.rs`、`state.rs` | parity § 6.1 第 1–5 条 |
| S5 | `core`：watch（feature 门控） | `watch.rs`、`line_buffer.rs` | parity § 6.2 第 10–12 条 |
| S6 | `testkit` | 4 个模块 | 被 S4/S5 的测试用起来 |
| S7 | `ipc-types` + `ipc` | 类型 / 传输 + 客户端 | control-plane § 9 的 1–3 |
| S8 | `service` | `host.rs` + 三平台模块 + `probe.rs` | 只读路径测试 + 手动清单 |
| S9 | `update` | `manifest.rs`、`verify.rs`、`apply.rs` | update-flow § 8 的 1–7、10、14 |
| S10 | `peon-burrow` 产品层 | `config/`、`doctor/`、`control.rs`、`exit.rs`、`run.rs` | 命令映射 / doctor / 退出码单测 |
| S11 | bin + 前台跑通 | `crates/peon-burrow/src/main.rs` | 真邮箱收到一封信；`wc -l` ≤ 60 |
| S12 | `examples/` | 7 个示例 | `cargo run --example embed_relay` 等 |
| S13 | CI | `ci.yaml`（含守卫）+ `assets.yaml` + `release.yaml`（含 publish） | 打 `v0.0.1-rc.1` 出全部产物 |
| S14 | 首次发 crates.io | 按 § 8 顺序 | `cargo install peon-burrow` 装上 `burrow` |

**S1–S5 是「协议对齐 TS 版」阶段**，顺序不能反（[`adr-0002`](./decisions/adr-0002-rust-rewrite-scope.md)）。

---

## 2. S0：workspace 骨架

```
Cargo.toml                      # [workspace] resolver="3", edition 2024, rust-version = MSRV
                                # + [workspace.package]（version/license/edition/rust-version/repository）
crates/
├── peon-burrow-protocol/       # S1  ← 先写它（core 依赖它，且它是稳定契约）
├── peon-burrow-core/           # S2–S5
├── peon-burrow-ipc-types/      # S7
├── peon-burrow-ipc/            # S7
├── peon-burrow-service/        # S8
├── peon-burrow-update/         # S9
├── peon-burrow/                # S10–S11（lib + [[bin]] burrow）
├── peon-burrow-testkit/        # S6（publish = false）
└── peon-burrow-examples/       # S12（publish = false，`examples/` 里的 7 个文件）
```

**统一元数据放 `[workspace.package]`**，各 crate 用 `version.workspace = true` 等继承；
`LICENSE` 用 MIT（[`../STABILITY.md`](../STABILITY.md)）。

⚠️ **不写 `rust-toolchain.toml`**（会让 CI 的 MSRV job 失效），MSRV 只写在 `Cargo.toml` 的 `rust-version`。

**验收**：`cargo metadata --no-deps` 列出 9 个包；`cargo clippy --all-targets` 干净（此时全是空壳）。

---

## 3. S1：`protocol` 先写（稳定契约）

| 文件 | 内容 | 验证 |
| --- | --- | --- |
| `lib.rs` | `WATCH_PROTOCOL_VERSION`、再导出 | `cargo doc -p peon-burrow-protocol` |
| `target.rs` | `resolve_target(&Uri) -> TargetResolve`（路径 `/host:port` 与查询 `?host=&port=`）、`tls` 判定、缺省端口 `tls?993:143` | parity W2–W4、C4–C6 |
| `policy.rs` | `is_host_allowed`（`*` 通配、大小写不敏感）、loopback 判定 | parity W6、C6 |
| `watch.rs` | `WatchRequest` / `ClientMessage`（serde 形状 = 冻结协议） | parity W8–W9、E2（`tls` 容忍 `0`/`false`） |
| `close.rs` | 关闭码常量 + `truncate_close_reason`（≤120 字节、UTF-8 安全） | parity W10–W11、F1 |

⚠️ 这一层**不许出现 tokio / rustls / interprocess**（守卫脚本会挡）。

---

## 4. S2–S5：`core`

| 步 | 先写什么 | 为什么这个顺序 |
| --- | --- | --- |
| S2 | `error.rs`（`Transient`/`Fatal`）、`policy.rs`（默认 `Policy` 实现） | 零 IO；`error` 被后面所有模块用 |
| S3 | `transport.rs` | 建连+TLS+证书错误的**唯一实现**（tunnel 与 watch 共用） |
| S4 | `dispatch.rs` → `tunnel.rs` → `state.rs` | 第一个真 IO；`state` 同时定下「状态 vs 日志」边界 |
| S5 | `watch.rs` + `line_buffer.rs`（`#[cfg(feature = "imap-watch")]`） | 纯状态机，前面都就绪后最后写 |

**每步验证**：`cargo test -p peon-burrow-core --lib`；S4/S5 再跑集成测试
（`cargo test -p peon-burrow-core --test tunnel_e2e`、`--features imap-watch --test watch_e2e`）。

⚠️ feature 关掉时也要能编、能跑：`cargo check -p peon-burrow-core --no-default-features`，
且此时 `__watch` 请求必须被明确拒绝（不是静默当透传）。

---

## 5. S6–S9：测试工具、控制面、服务、更新

| 步 | 关键点 |
| --- | --- |
| S6 | `testkit`：`rcgen` 自签证书（不依赖 openssl）、`Vec<Step>` 剧本、`port: 0` + **等就绪**（不是 sleep） |
| S7 | `ipc-types`（只 serde）→ `ipc`（本地 socket 优先、loopback TCP 退路、8 KiB 上限、token 校验 + 5 次失败锁 30 秒） |
| S8 | `service`：单一 trait + 三平台模块（**不拆 crate**）；`probe_port` 是端口探测唯一实现；`RestartStrategy` 在此定义 |
| S9 | `update`：输入 `UpdateContext`（**不自查环境**）；四道闸 size→sha256→签名→冒烟测试；成功返回 `Applied` |

**验收**：`cargo test -p peon-burrow-ipc`、`cargo test -p peon-burrow-update`（本地 HTTP server 伪造清单）。

---

## 6. S10–S11：产品层与 bin

| 步 | 关键点 |
| --- | --- |
| S10 | `config/`：TOML 字段全 `Option` + 组合规则（非 loopback ⇒ token+白名单）；`Paths::discover()` 是唯一碰 `directories` 的地方，`for_test()` 给测试<br>`doctor/`：检查项注册表（加检查 = 加 struct）<br>`control.rs`：`Request` 映射（纯函数）+ `From<&RelayState> for ProcessStatus`<br>`exit.rs`：`ExitCode` + `AppError`（唯一退出码来源）<br>`run.rs`：配置 → 日志 → 控制面 → `RelayServer` → 信号 |
| S11 | `main.rs` ≈ 40 行，只做 `Cli::parse` → `peon_burrow::run(cli)` → 退出码 |

```rust
// crates/peon-burrow/src/main.rs
fn main() -> std::process::ExitCode {
    let cli = peon_burrow::Cli::parse();
    let code = tokio::runtime::Builder::new_multi_thread().enable_all().build()
        .expect("runtime").block_on(peon_burrow::run(cli));
    std::process::ExitCode::from(code as u8)
}
```

**验收**：干净机器 `cargo run -p peon-burrow -- run --foreground`，扩展填 `ws://127.0.0.1:41316/`，
**收到一封真邮件**（QQ 邮箱）。

---

## 7. 与 parity 断言的对应

| parity 断言 | 哪一步满足 |
| --- | --- |
| § 6.1 第 1–5 条（往返 / 256KB / TLS / 存活） | S3 + S4 |
| § 6.1 第 6–9 条（默认拒绝自签证书 / 1008 / 策略后存活） | S1 + S3 |
| § 6.2 第 10–12 条（watch 状态机 / 未标记问候 / 致命不重试） | S5（`--features imap-watch`） |
| § 6.2 第 13–19 条（截断 / 策略不发字节 / 端口 / 控制面 / 双实例 / 关停） | S1、S7、S8、S10 |
| § 6.2 第 20 条（二进制被删后不静默失联） | S8 + S10（doctor） |

---

## 8. S14：发布到 crates.io 的顺序

```
peon-burrow-protocol
  → peon-burrow-core
      → peon-burrow-ipc-types
          → peon-burrow-ipc
  → peon-burrow-service
  → peon-burrow-update
      → peon-burrow            （最后：产品 crate，用户装这个）
```

- 用 `harbor` 编排（它会 preflight 依赖顺序）：`cargo harbor check --plan "$GITHUB_REF_NAME"` → `cargo harbor publish`；
- 每次发版先本地 `cargo publish --dry-run -p <crate> --locked` 过一遍；
- 首次发布要**按上面顺序手动发一遍**（registry 上还没有依赖版本时，后面的 crate 无法验证）；
- `testkit` 与 `examples` **不发**（`publish = false`）。

---

## 9. 每次提交前

```bash
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo test -p peon-burrow-core --features imap-watch --locked
cargo doc --no-deps
```

外加布局守卫（`modules.md § 12`，建议直接做成 CI 的一个 step）。