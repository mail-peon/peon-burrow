# ADR-0010 · 落地时与设计文档不一致的地方

- 状态：**已生效**（实现完成到 S13）
- 日期：2026-10-09
- 影响文档：`02-tech-stack.md`、`modules.md`、`design/config-schema.md`、`design/update-flow.md`、
  `design/wire-protocol.md`、`04-parity-node-to-rust.md`、`design/cli.md`

---

## 为什么写这一份

设计文档是**先写**的，实现时出现三类偏差：

1. **更简单的做法** —— 文档里的方案在 Rust 里是多余的；
2. **依赖政策的硬约束** —— 某些 crate 会拉进 C 工具链，与「只用 ring」冲突；
3. **契约必须更严** —— 跨仓库的东西写松了，线上会静默失效。

⚠️ 这份 ADR **优先于**上面文档里被点名的段落。逐条给「文档怎么写 / 实际怎么做 / 为什么」。

---

## 1. 协议层：`resolve_target(&str)` 而不是 `&Uri`；`RelayTarget` 多一个 `token`

- 文档：`pub fn resolve_target(uri: &Uri) -> TargetResolve;`（`modules.md § 1`）
- 实际：`pub fn resolve_target(raw: &str) -> TargetResolve;`
- 理由：`peon-burrow-protocol` 的定位是**零 IO、零重依赖**（要让别的语言的客户端照着抄）。
  为了一个 `&Uri` 引进 `http` crate 不划算；查询解析与百分号解码自己写只有约 20 行，
  且有 13 条单测覆盖。`token` 放进目标信息，是为了让策略层不必**再解析一遍** URL。

## 2. 背压：靠 `await` 传播，删掉水位阈值

- 文档：TCP→WebSocket 方向 16 MiB 在途上限 + 迟滞（`modules.md § 2`、parity `D4`）
- 实际：每个方向一个任务、每步 `await`；内存上界 = 一个读缓冲（16 KiB）。
  配置项 `backpressure_high_water_mib` **保留但已无作用**，出现时给一行警告。
- 理由：TS 版必须手工计数，是因为 Node 的 stream 会无限缓冲；Rust 里
  `read().await` → `send().await` 天然把背压传回去。手工水位只会引入一个**可能算错**的状态机。
- 影响：`design/config-schema.md § 2` 那一行标注为「已弃用，写了会警告」。

## 3. 控制面传输：tokio 自带命名管道 / Unix socket，不用 `interprocess`

- 文档：`interprocess`（`02-tech-stack.md § 2`）
- 实际：`tokio::net::windows::named_pipe` + `tokio::net::UnixListener`/`UnixStream`，
  藏在 `IoStream` trait 后面
- 理由：少一个依赖；平台分支只有两个 `cfg`；传输本来就在 trait 后面，将来换实现不动上层。
- ⚠️ 踩过的坑（值得写下来）：**Windows 命名管道的名字必须是 `\\.\pipe\<名字>` 全名**，
  传短名得到的是 `os error 123`（文件名语法不正确）——一个和「连不上」毫无相似度的错误。
  现在 `ipc::transport` 会统一补齐前缀，并有单测。

## 4. 自更新：不引 `reqwest`，自写最小 HTTPS GET

- 文档：`self_update` + `reqwest`（`02-tech-stack.md § 2`）
- 实际：`tokio` + `tokio-rustls` + `rustls(ring)` + `webpki-roots`，四道闸自己实现
- 理由：**`reqwest 0.13` 的 `rustls` feature 会拉 `aws-lc-rs`**，它需要 C 工具链
  （Windows 上要 cmake/nasm）——与「ring 是唯一 provider」的政策冲突。
  自写的 GET 约 250 行，而且顺带得到一个可注入的 `Fetcher`（测试完全不碰网络）。
- 影响：`02-tech-stack.md` 的依赖表要划掉 `reqwest` / `self_update`。

## 5. TLS 信任根：先用 `webpki-roots`，系统信任库待办

- 文档：`rustls-platform-verifier`（系统信任库）
- 实际：`webpki-roots`（Mozilla 根）+ `TlsConfig::extra_roots` 注入私有 CA
- 理由：`rustls-platform-verifier` 当前会引入 `aws-lc-rs`（同第 4 条）。
- 待办：等它支持 ring-only、或接受多一个 provider 时切过去。企业私有 CA 现在走注入，
  有示例（`examples/private_ca.rs`）与端到端测试覆盖。

## 6. `service` / `update` **不依赖引擎**

- 文档：依赖图里有 `core → service`、`core → update`（`modules.md § 1`）
- 实际：两者只依赖 `ipc-types`（`service` 还依赖 `sysinfo`），**完全独立**，任何本地工具都能复用
- 理由：它们和「中继」没有关系（输入是 `InstallOptions` / `UpdateContext`）。
  独立之后依赖图是两条互不相干的支路，`cargo tree` 一眼可验（CI 里有 L3 守卫）。
- 影响：`modules.md § 1` 的依赖图要改；`UpdateContext` 因此**没有** `restart: RestartStrategy`
  —— 重启是调用方（产品层）的事：`apply()` 只返回 `Applied`。

## 7. `UpdateContext` 比文档多 4 个字段

`binary_name`（要替换的文件名，`None` = 从 `current_exe()` 推断）、
`verifier: Option<Arc<dyn SignatureVerifier>>`、`skip_smoke_test`（默认 `false` —— 冒烟测试是必过的闸）、
`fetcher: Option<Fetcher>`（打桩点）。另外 `UpdateContext` 派生了 `Clone`（字段都是 `Arc`/值类型），
因为控制面钩子与启动检查各要一份。

## 8. 清单契约（**跨仓库，最要紧的一条**）

`latest.json` 的形状以实现为准，两条硬约束：

1. **字段名是 camelCase**（`releasedAt` / `notesUrl`）——写成 snake_case 会让 `check()`
   直接 `missing field` 失败；
2. **`assets[].name` 必须指向裸二进制**（`burrow-<target>[.exe]`），**不能是 `.zip` / `.tar.gz`**
   —— `peon-burrow-update` 不解压，指向压缩包会被判为「不支持的资产格式」。

⚠️ 这两条漏掉的后果是「线上老版本的更新检查永远失败」，而本地测试完全看不见。所以：
`scripts/make-manifest.sh` 按此生成、CI 里有一条 grep 守卫、产品层有一条契约测试把生成的清单
喂回 `Manifest::parse`。归档仍然发布，只是**给人手动下载**。

## 9. staging 目录位置

文档写 `<data-dir>/update/…`；实际是 `<install_dir>/.peon-burrow-update/<version>/`
（`UpdateContext` 只知道 `install_dir`，不知道数据目录）。同一个目录内的 rename 才是原子操作，
这样反而更对。

## 10. 四道闸之外还有一道「格式预检」

顺序仍是 `size → sha256 → 签名 → 冒烟测试`；**压缩包预检插在第 2 与第 3 道之间**
（按文件名与魔数各判一次）。它不算闸门，只是尽早失败、省一次下载。

## 11. Windows 用户级自启用 `Register-ScheduledTask -Xml`（XML 走 stdin）

- 文档：`schtasks /Create /XML`
- 实际：PowerShell `Register-ScheduledTask -Xml`，XML 走 stdin
- 理由：`schtasks /XML` 的参数是**文件路径而不是 XML 正文**，用它就必须多落一个临时文件
  ——而 `service-lifecycle.md § 3` 整段都在讲为什么临时目录不可靠。
- 任务 XML 里仍然有 `<Hidden>true</Hidden>`（不闪黑框）、`<LogonTrigger>`、
  `<RestartOnFailure>PT1M × 3`、`LeastPrivilege`，且全文可从测试断言。

## 12. 服务库多了一层执行缝：`Runner` + `Plan` + `Mutations`

文档里没有这一层。实现时加了：**所有会改机器的动作都经过 `Runner`**，
测试用 `FakeRunner` 断言「生成了哪些命令行 / 写了什么文件内容」，
于是 56 个测试里**没有一个真的改机器**（只读探测除外）。
这是「测试不许动系统」这条硬要求的实现方式。

## 13. 产品层多两个模块

`paths.rs`（`Paths::discover()` 是唯一碰 `directories` 的地方，`for_test()` 给测试）
与 `update.rs`（配置 → `UpdateContext` 的**唯一**映射点 + 节流戳 + 控制面钩子）。
`run::dispatch_with(paths, config, cli)` 是注入版入口，集成测试因此不碰真实用户目录。

## 14. 端口占用者诊断

`doctor` 的端口检查接 `service::probe_port`，`-v` 时输出「被 <进程名>（PID）占着」。
文档只写了「给诊断」；具体到进程名是加值项 —— 用户真正想知道的是这个，而不是一个 errno。

## 15. `doctor` 的退出码

有任意一条 `FAIL` → 退出码 `2`（与配置错误一致，脚本能直接用）；只有 `WARN` → `0`。
「端口被占」是 `WARN` 而不是 `FAIL`：中继可能正在跑，那本来就是正常的。

---

## 对发布的影响

- 仍然发布 7 个 crate，顺序 `protocol → core → ipc-types → ipc → service → update → peon-burrow`；
- `service` 不是 `core` 的下游，所以 `core` 可以先发；
- `peon-burrow` 是唯一带 bin 的 crate（`burrow`），`Cargo.toml` 里带
  `[package.metadata.binstall] bin-dir`，否则 `cargo-binstall` 会找错产物名；
- 首次发版必须**人工按序**（不可逆），之后可以用 `release.yaml` 的 dry-run 兜底。

## 仍然缓做的（已写进各 crate 文档，不是遗漏）

真签名算法（minisign/zipsign，只留注入点）、解压（zip/tar → 明确报错）、
CONNECT 代理（明确报错）、重启辅助进程（`--apply-restart --wait-pid`）、
检查退避与断点续传、macOS 13+ `SMAppService`、launchd/unit 注入环境变量、
按 `allowed_users` 限制控制面（系统服务模式下）。
