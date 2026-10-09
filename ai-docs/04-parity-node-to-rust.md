# 04 · TS 中继 → Rust 逐条对照（parity）

> **本文是重写时唯一必须逐条对照的清单。**
> 左边是现状（`mail-peon/scripts/imap-relay.ts`，1901 行 / 73 KB，单文件），
> 右边是 Rust 版的落点，以及**允许偏离 / 必须一致**的判定。
>
> 判定用三档：
> - **必须一致**：扩展侧能观察到，或涉及安全属性 —— 偏离 = 破坏性变更
> - **可以不同**：内部实现细节
> - **应该不同**：TS 版的做法在服务形态下是错的（本文档给出理由）

源文件：`../../../mail-peon/scripts/imap-relay.ts`（行号以此文件为准）
回归测试：`../../../mail-peon/scripts/imap-relay.test.ts`（9 条断言，见 § 6）

---

## 1. TS 版是什么（角色表）

| 角色 | 说明 | Rust 版对应 |
| --- | --- | --- |
| WebSocket 服务器 | 单端口，两种模式共用（`ws` 库） | `peon-burrow-core::server`（`tokio-tungstenite`） |
| 隧道 | `WebSocket ↔ TCP/TLS` 字节透传，双向背压 | `peon-burrow-core::tunnel` |
| TLS 客户端 | implicit TLS（993），SNI，默认校验证书链 | `peon-burrow-core::tls`（`tokio-rustls`） |
| watch 协议机 | 替扩展挂 `IDLE`，推「有新邮件了」 | `peon-burrow-core::watch` |
| 访问控制 | token + host 白名单（支持 `*`）+ loopback 拒绝 | `peon-burrow-core::policy` |
| 配置读取 | CLI `--flag` > `ENV` > 默认值，模块级常量 | `peon-burrow-config`（+ TOML 文件层） |
| 端口占用处理 | `netstat`/`lsof` 找 PID → 终端 `[Y/n]` → `taskkill` | `burrow` CLI（**服务态不做这件事**，见 G3） |
| 交互控制台 | TTY 下 `q` / `r` | 仅 `run --foreground` 且 TTY |
| 日志 | stdout，`[imap-relay] ISO8601 msg` | `tracing` + 文件滚动（+ 控制面可读） |
| 崩溃兜底 | `uncaughtException` / `unhandledRejection` 吞掉 | Rust 无此问题；改为「服务层自动重启」+ 连接级错误隔离 |
| 服务化 / GUI / 自更新 | **不存在** | 本项目的全部新增内容 |

---

## 2. 对外契约（**必须一致**，违反即破坏扩展）

| # | 契约 | TS 位置 |
| --- | --- | --- |
| W1 | 端点：`ws://<host>:<port>/`，**一个端口两种模式** | `L405` |
| W2 | 透传目标：路径 `/host:port`，或查询 `?host=&port=` | `L1393-1407` |
| W3 | `tls` 缺省为真，判定式 `tls !== '0'`（**只有字面 `0` 才是明文**） | `L1417` |
| W4 | 端口缺省：`tls ? 993 : 143` | `L1419-1420` |
| W5 | `token` 从 query 取，与配置的 token 比对，不匹配 → **关闭码 1008** | `L574-578, L1431` |
| W6 | 白名单支持 `*` 通配（`*.gmail.com`），大小写不敏感 | `L1440-1459` |
| W7 | 第一帧为**文本且 `__watch === 1`** → watch 模式；否则透传 | `L325-331, L602-651` |
| W8 | watch 请求字段：`__watch` / `accountId` / `host` / `port` / `tls` / `user` / `pass` / `token` | `L59-68`（扩展侧）, `L264-274` |
| W9 | watch 推送（文本帧）：`{"type":"mail","accountId":…,"exists":N}`、`{"type":"state","state":"watching","exists":N}`、`{"type":"state","state":"reconnecting","retryInMs":N}`、`{"type":"state","state":"error"\|"failed","error":"…"}` | `L308-312, L1289-1298` |
| W10 | 关闭码：`1008` 策略拒绝 / `1011` watch 启动失败 / `1001` 重启中 | `L482, L630, L1508` |
| W11 | 关闭原因 **≤ 120 字节**、UTF-8 安全截断 | `L1525-1530` |
| W12 | 透传方向：任一方向关闭 → 关另一边 | `L696-712` |
| W13 | 只推「EXISTS 变大」，推送**不含邮件内容** | `L1115-1128` |
| W14 | 致命错误（密码错、白名单拒绝）→ `state:"failed"` 且**服务端停止重连** | `L981-985` |

完整定义与「扩展侧如何消费」见 [`design/wire-protocol.md`](./design/wire-protocol.md)。

---

## 3. 行为细节逐条对照

### A. 配置与启动

| # | TS 行为（位置） | Rust 落点 | 判定 |
| --- | --- | --- | --- |
| A1 | `readOption`：CLI `--flag` > `ENV` > 默认值（`L82-87`） | `peon-burrow-config`：TOML 文件 + CLI + ENV 三层；**ENV 变量名保持兼容** | 必须一致（ENV/CLI 名） |
| A2 | 端口非 0–65535 → stderr + `exit 1`（`L100-103`） | 配置校验失败 → 退出码 `2`（配置错误），并写状态文件 | 可以不同（退出码语义见 G6） |
| A3 | 默认 `HOST=127.0.0.1`（`L106`） | 同 | 必须一致 |
| A4 | `RELAY_TOKEN` 默认空（`L109`） | 同；服务模式下若用户曾设过则从配置文件读 | 必须一致 |
| A5 | `ALLOWED_HOSTS`：逗号分隔、trim、转小写、去掉空项（`L116-119`） | 同；TOML 里用数组，CLI 仍接受逗号串 | 必须一致 |
| A6 | `TLS_REJECT_UNAUTHORIZED` 默认 **开**（`L127`） | 同，且**不做**「默认放宽」的任何妥协 | 必须一致（安全属性） |
| A7 | `RELAY_TRACE=1` 打印原始字节（`L140`） | 保留，但服务态需显式二次确认，且日志里标记 | 可以不同（默认值必须仍是关） |
| A8 | 常量：`IDLE_TIMEOUT_MS = 15min`（`L162`）、`WATCH_REIDLE_MS = 25min`（`L151`）、退避 `[2,5,15,30,60,120,300]s`（`L159`）、`HIGH_WATER_BYTES = 16MiB`（`L224`） | `peon-burrow-config` 里的默认可配项；**数值默认不变** | 必须一致（数值） |
| A9 | 启动横幅 + 4 条安全告警（token/白名单/非 loopback/自签证书）（`L1343-1358`） | 日志同样的告警；GUI 状态页也展示 | 可以不同（形式） |
| A10 | 日志格式 `[imap-relay] <ISO8601> <msg>`（`L1857-1859`） | `tracing` 默认格式；保留 `[relay]` 前缀便于用户对照旧文档 | 可以不同 |

### B. WebSocket 服务生命周期

| # | TS 行为（位置） | Rust 落点 | 判定 |
| --- | --- | --- | --- |
| B1 | 「工厂 + 显式 start/stop」而不是模块顶层 `listen`，为了能重启（`L340-354`） | `RelayServer::start/stop`，同上 | 必须一致（能力） |
| B2 | 「是否在跑」用 `listening` 标志，**不能**用 `server !== null`（失败的 start 会留残留对象）（`L363-376, L441-452`） | `State<Idle|Running>` 枚举，从类型上排除这个 bug 类 | 应该不同（用类型消灭） |
| B3 | `start()` 幂等；先清掉上次失败残留（`L382-398`） | 同 | 必须一致 |
| B4 | `listening`/`error` 两个 handler 用 `settled` 去重（`L411-439`） | `tokio::select!` 或 `TcpListener::bind().await` —— 天然一次 | 可以不同 |
| B5 | 运行期 server error 只记日志，进程不退（`L456-459`） | 同（连接级错误隔离，见 I6） | 必须一致 |
| B6 | `stop()`：**先主动断开所有连接**再 `close()`，2 秒兜底（`L464-493`） | 同；用 `CancellationToken` + `JoinSet`，`close()` 带超时 | 必须一致（否则 IDLE 卡住关停） |
| B7 | `url()` 返回给用户填的地址（`L1274`） | 发现文件 + 控制面 + `doctor` 都要给同一个值 | 可以不同 |

### C. 建连、策略与第一帧分流

| # | TS 行为（位置） | Rust 落点 | 判定 |
| --- | --- | --- | --- |
| C1 | 立刻建 TCP/TLS，**不等第一帧**（`L666-694`，理由见 `L505-518`） | 同。Rust 里用 `tokio::spawn` 后立即 `connect`，**不要**写成 `while let Some(msg) = ws.next()` 先读帧 | 必须一致 |
| C2 | 策略检查（token/白名单/`tls=0`+993）在 `connection` 事件里**同步**完成（`L573-584`） | 同：在拿到升级请求后、`spawn` 之前完成 | 必须一致（安全） |
| C3 | 被拒连接 `close(1008, reason)`，不写任何字节（`L1505-1517`） | `Message::Close(Some(CloseFrame{ code: Policy, reason }))` | 必须一致 |
| C4 | `NoTargetError` 与其它解析错误**分开处理**：前者留着等第一帧，后者立刻拒（`L551-562`） | `enum TargetResolve { Target(T), NoTarget, Rejected(Reason) }` | 必须一致 |
| C5 | `tls=0` + 993 → 立刻拒绝（`L1422-1424`） | 同，且文案保留「143 + STARTTLS 本版本不支持」 | 必须一致 |
| C6 | loopback 目标且不在白名单 → 拒绝（防把中继当 SSRF 跳板）（`L1428-1429`） | 同 | 必须一致（安全） |
| C7 | 分流判据：`__watch === 1`，别的字段一概不看（`L325-331`） | 同（serde 反序列化到 `WatchRequestRaw`，只判这一个字段） | 必须一致 |
| C8 | 判定为 watch → **先关掉刚建的 TCP** 再进 watch（`L617-634`） | 同。代价是服务器侧留一条「连上就断」的记录，可接受 | 必须一致 |
| C9 | 透传路径要把**第一帧补投**（`L650, L884-890`） | 同 | 必须一致（否则第一个 IMAP 命令丢） |
| C10 | 后续帧的 `on('message')` **必须在分流之后**才注册（`L806-819`） | 同（否则第一帧被处理两次：回声测试会收到 84 字节而期望 42） | 必须一致 |
| C11 | 无目标且非 watch → `close(1008)`，文案含 `用 /host:port 或 ?host=&port=`（`L645-648`） | 同文案 | 必须一致 |
| C12 | **不用** `ws.pause()/resume()` 做分流（会与透传背压抢状态）（`L529-531`） | 同（Rust 里对应 `StreamExt::next` 的取消安全：不要在两个地方同时 `next()`） | 必须一致（设计约束） |
| C13 | watch 启动失败 → `state:"failed"` + `close(1011)`（`L626-633`） | 同 | 必须一致 |

### D. 透传（隧道）

| # | TS 行为（位置） | Rust 落点 | 判定 |
| --- | --- | --- | --- |
| D1 | TLS 用 `tls.connect({ host, port, servername, rejectUnauthorized })`；`servername` 是 SNI，**IP 字面量不传**（`L685-692`, `L1475-1477`） | `tokio-rustls` + `ServerName`；IP 字面量用 `ServerName::IpAddress` | 必须一致 |
| D2 | `socket.setTimeout(IDLE_TIMEOUT_MS)`，超时 → 关连接（`L694, L863-865`） | `tokio::time::timeout` 包住读方向；watch 方向**禁用**（见 E16） | 必须一致（数值） |
| D3 | `shutdown()` 用 `closed` 标志去重，避免两边互相触发重复日志（`L696-712`） | 同 | 可以不同（日志去重，行为一致） |
| D4 | TCP→WS 背压：在途字节计数 + `HIGH_WATER (16MiB)` 暂停 + **半阈值**恢复（`L714-752`） | 同：`in_flight: usize` + `Notify`/`watch` 通道；`split()` 后两个方向各自任务 | 必须一致（阈值与迟滞） |
| D5 | WS→TCP：二进制帧原样写；文本帧按 UTF-8 编码后写（`L780-782`） | 同：`Message::Binary` 直写，`Message::Text` → `bytes()` | 必须一致 |
| D6 | `write()` 返回 false → `ws.pause()`；`drain` → `ws.resume()`（`L796-799, L827-832`） | 背压：`SinkExt::feed/send` 的 `await` **本身就是背压** —— 不要额外造暂停逻辑，也不要无界 `mpsc` | 应该不同（Rust 用 await 背压） |
| D7 | 建连失败通过 `onConnectError` 上报；判据是 `socket.connecting`（`L857-858`） | 区分「connect 阶段失败」与「连接后失败」（`connect().await` 的 `Err` vs 读循环里的 `Err`） | 必须一致（能力） |
| D8 | 自签证书错误给出人话提示（`L843-847`） | `rustls` 错误映射到同样的提示文案 | 必须一致（文案可参照） |
| D9 | tcp `error`/`timeout`/`close`、ws `close`/`error` 全部走 `shutdown`（`L863-877`） | 同 | 必须一致 |

### E. watch（IDLE）

| # | TS 行为（位置） | Rust 落点 | 判定 |
| --- | --- | --- | --- |
| E1 | 校验 `host` / `port` / `user` / `pass` 必填，端口 1–65535（`L924-935`） | 同；缺失 → `throw` → 走 C13 的失败通道 | 必须一致 |
| E2 | `useTls = tls !== false && tls !== 0`（数字 `0` 与布尔 `false` 都算明文）（`L927`） | serde 里用 `Option<Value>` 或自定义反序列化，**两种写法都要认** | 必须一致（输入宽容） |
| E3 | watch 与透传**共用**同一套 token / 白名单检查（`L937-941`） | 同（`policy::check` 一处实现，两处调用） | 必须一致（安全） |
| E4 | `clientGone` 由 ws `close`/`error` 置位，重连循环据此退出（`L943-950`） | `CancellationToken`；ws 任务结束时取消（**不要**只靠 `ws.send` 失败发现） | 必须一致（能力） |
| E5 | 退避阶梯 `[2,5,15,30,60,120,300]s`，`attempt` 在成功一轮后归零（`L954-995`） | 同 | 必须一致 |
| E6 | 致命错误不重试：`登录失败\|无法打开收件箱\|invalid token\|host not allowed\|缺少有效的\|缺少 user`（`L1338-1340`） | 用**类型化错误**（`FatalError` vs `TransientError`）而不是正则匹配；文案保持 | 应该不同（类型取代字符串匹配） |
| E7 | 问候语是**未标记**的 `* OK [...]` / `* PREAUTH`；靠「第一条 `* ` 开头」推进（`L1145-1161`） | 状态机显式建模：`Greeting → Login → Select → Idle ⇄ Done` | 必须一致（逻辑） |
| E8 | tag 形如 `A0001`，`padStart(4,'0')`（`L1086`） | 同（IMAP 本身允许任意 tag，但日志可读性一致更好排错） | 可以不同 |
| E9 | `quote()`：转义 `\` 与 `"`，**拒绝 CR/LF**（防命令注入）（`L1313-1317`） | 同；CR/LF 直接判为致命错误 | 必须一致（安全） |
| E10 | `SELECT` 成功后基准 `notified = exists`（`L1198`） | 同 | 必须一致 |
| E11 | 只在 `exists > notified` 时推 `mail`；变小不推（`L1123-1128`） | 同 | 必须一致 |
| E12 | `+ …`（继续请求）忽略（`L1131-1134`） | 同 | 必须一致 |
| E13 | 带标记响应判定 `^A\d{4} (OK\|NO\|BAD)`（`L1164`） | 自己维护「哪些 tag 已发」的集合更稳（见 § 7 缺口 I-3） | 可以不同 |
| E14 | `DONE` 之后收到该 tag 的 OK → 重新 `IDLE`（`L1206-1221`） | 同 | 必须一致 |
| E15 | 25 分钟重挂一次 IDLE（RFC 2177 建议 ≤29 分钟）（`L1101-1106`） | 同，可配 | 必须一致（默认值） |
| E16 | watch 连接 `setTimeout(0)`：**禁用**空闲超时（IDLE 会长期静默）（`L1029`） | 同（否则会把健康连接踢掉） | 必须一致 |
| E17 | 行缓冲：TLS 分片会在任意位置切断一行（`L176-213`, `L1226-1238`） | `peon-burrow-core::line_buffer`，**必须**有单测（喂半行、喂 `* 12 EX` + `ISTS`） | 必须一致（逻辑） |
| E18 | `watchOnce` resolve = 服务器正常关连接（重连）；reject = 错误（可能致命）（`L1044-1058`） | `Result<Closed, WatchError>`，`WatchError::Fatal` / `::Transient` | 必须一致（语义） |
| E19 | 只有 TLS `secureConnect` / 非 TLS `connect` 之后才进入问候阶段（`L1255-1266`） | 同（`connect().await` 成功后再开始读） | 可以不同 |
| E20 | 推送只有一处序列化（`sendToClient`），且 `readyState !== OPEN` 时跳过（`L1289-1298`） | 同：`ClientMessage` 枚举 + 一个 `send` 封装 | 必须一致 |

### F. 健壮性与运维

| # | TS 行为（位置） | Rust 落点 | 判定 |
| --- | --- | --- | --- |
| F1 | 关闭原因按**字节**截断到 120，且不切坏 UTF-8（`L1525-1530`） | 同（`&str` 按 `char_indices` 截断到 ≤120 字节）；**必须有单测**（中文 + emoji） | 必须一致（I5） |
| F2 | `reject` 里 `close` 抛异常 → `terminate`（`L1505-1517`） | 同（关闭失败直接 drop 连接） | 必须一致（能力） |
| F3 | `uncaughtException` / `unhandledRejection` 吞掉、进程不退（`L1538-1543`） | Rust 里没有对应物；改为「连接级隔离」+ **服务层崩溃自动重启**（ADR-0003）。⚠️ 不要引入 `panic = "abort"` + 全局 `catch_unwind` 去模仿它 | 应该不同 |
| F4 | 端口被占用 → `netstat -ano` / `lsof` 找 PID（`L1560-1588`） | `sysinfo` 或平台 API；给 GUI 显示「谁占着」 | 可以不同（实现），能力保留 |
| F5 | 只杀**具体 PID**，绝不按名字杀（`L1599-1610`） | 同。且**服务态永不杀**，只在 CLI 前台带 `--force-port` 时杀 | 应该不同（见 G3） |
| F6 | `[Y/n]` 用**同步**读，非 TTY 直接返回 false（`L1628-1676`） | 前台模式可保留交互；服务态无 TTY，走 G3 | 应该不同 |
| F7 | 杀掉占用者后等 300 ms 再 `listen`（`L1725`） | 同（内核回收监听套接字有延迟） | 必须一致（逻辑） |
| F8 | 控制台 `q` / `r` 仅 TTY 下启用（`L1783-1816`） | 仅 `run --foreground`；服务态不注册任何 stdin 逻辑 | 应该不同（服务态） |
| F9 | 退出时**主动**关服务与 readline，否则进程不结束（`L1840-1844`） | 同：`CancellationToken` + `JoinSet::shutdown`，超时兜底退出 | 必须一致（能力） |
| F10 | `SIGINT` / `SIGTERM` 走同一套清理（`L1847-1852`） | `tokio::signal`；Windows 下还要处理 **SCM 的 Stop 控制码** | 必须一致（能力）+ 新增 SCM |
| F11 | trace：只打文本行（≤20 行），二进制正文只报长度（`L1873-1893`） | 同（避免把终端刷爆） | 可以不同（形式），语义保留 |
| F12 | 入口包在 `main()` 里（CJS 不支持顶层 await）（`L1746-1758`） | Rust 无此限制。**这一条不要照抄**，但要知道为什么存在：当年照文档敲 `pnpm relay:port` 是错的 | 应该不同 |

---

## 4. 应该不同的地方（汇总理由）

| 主题 | TS 版 | Rust 版 | 为什么必须改 |
| --- | --- | --- | --- |
| **G1 单文件** | 1901 行一个文件 | 6 个 crate（[`01-architecture.md § 2.1`](./01-architecture.md)） | 服务注册、自更新、控制面都要各自的测试与依赖边界 |
| **G2 配置** | CLI + ENV，模块级常量 | 再加 TOML 文件层；ENV 名兼容 | 服务不由用户手动启动，配置必须能落盘；模块级常量导致**一个进程只能跑一个实例** |
| **G3 端口占用** | 找到 PID → 交互问 → `taskkill /F` | 用户级自启：**不杀、不换端口**；先判断占用者是不是自己的旧实例（控制面握手），是则视为「已在运行」；不是则**明确失败**并给出「谁占着 + 怎么办」。前台 CLI 才提供 `--force-port` | ① 服务没有 TTY，交互式提问等于挂死；② 一个后台服务**不该**有权限去杀任意进程；③ 扩展只认固定默认端口，**静默换端口 = 用户永远连不上**（比失败更糟） |
| **G4 日志** | stdout | 文件 + 滚动 + 控制面可读；stdout 仅前台模式 | 服务态没有终端；排查必须能回看历史 |
| **G5 交互控制台** | 有 | 仅前台 | 服务态 stdin 是关闭的（TS 版自己都写了这个坑，见 `L1777-1782`） |
| **G6 退出码** | 0 / 1 | 0 正常、1 运行失败、2 配置错误、3 端口被占用、4 自更新后需重启 | GUI 与脚本要能区分「配置错」和「端口冲突」 |
| **G7 崩溃** | 吞异常继续跑 | 连接级隔离 + 服务层自动重启（SCM recovery / `Restart=always` / `KeepAlive`） | 吞异常会让进程处于「假活着」状态；重启是操作系统本来就提供的能力 |
| **G8 发现** | 无 | 写发现文件（端口 / PID / 版本 / 启动时间） | GUI、`doctor`、用户排查都要它 |
| **G9 自更新** | 无 | `peon-burrow-update` | 免安装工具没有包管理器帮忙升级 |
| **G10 可测性** | `spawn` 子进程 + `sleep(1200)` | `#[tokio::test]` 里直接起实例（`RelayOptions` 注入 + `port: 0`） | 测试不该靠 sleep；端口 0 让内核分配，避免测试互相抢端口 |

---

## 5. 环境变量与 CLI 兼容表

服务化以后，**旧文档里的命令仍然要能用**（否则用户照旧文档敲会一脸茫然）：

| 旧（TS 版） | 新（Rust 版） | 说明 |
| --- | --- | --- |
| `pnpm relay` | `burrow run --foreground` | TS 版需要 Node，Rust 版不需要 |
| `pnpm relay --port 8788` | `burrow run --port 41316` | 同上 |
| `PORT=8788` | `PORT=8788`（兼容）/ `--port` / `relay.toml` | ENV 名不变 |
| `HOST` / `RELAY_TOKEN` / `ALLOWED_HOSTS` / `TLS_REJECT_UNAUTHORIZED` / `RELAY_TRACE` | 同名 ENV 仍然生效 | 优先级：CLI > ENV > 配置文件 > 默认 |
| `pnpm relay:kill` | `burrow doctor --port` / `--force-port` | 见 G3 |

---

## 6. 回归验收：TS 的 9 条断言必须在 Rust 里全部成立

来源 `../../../mail-peon/scripts/imap-relay.test.ts`（`pnpm relay:test`）。
Rust 版把它们搬成 `crates/peon-burrow-core/tests/tunnel_e2e.rs`，
**断言文字保留**（便于跨版本对照），并新增 § 6.2。

### 6.1 逐条搬运（必须全绿）

| # | 断言 | TS 位置 |
| --- | --- | --- |
| 1 | WebSocket 建连成功 | `L134` |
| 2 | 字节原样回传（含 CRLF 与二进制） | `L144` |
| 3 | 256 KB 大块不丢字节 | `L154` |
| 4 | 关闭 WebSocket 后中继仍存活（不崩） | `L158` |
| 5 | `tls=1` 时中继用 TLS 连服务器（握手成功并回传 IMAP 命令） | `L176-180` |
| 6 | 默认拒绝自签证书（`TLS_REJECT_UNAUTHORIZED` 默认开） | `L229-233` |
| 7 | `tls=0` + 993 被拒（关闭码 1008） | `L243-247` |
| 8 | `ALLOWED_HOSTS` 之外的 host 被拒（1008） | `L252-254` |
| 9 | 多次策略拒绝之后中继仍存活 | `L261` |

> ⚠️ 第 6 条守的是**安全属性**，不是功能：默认值写反时功能测试照样全绿
> （连接成功），所以它必须单独存在。

### 6.2 Rust 版新增（TS 版没有覆盖到的）

| # | 新增断言 | 为什么 |
| --- | --- | --- |
| 10 | watch 状态机全流程（mock IMAP 服务器）：问候 → LOGIN → SELECT → IDLE → `* 2 EXISTS` → 收到 `mail` 推送；基准值不误推 | TS 版靠真邮箱手测，回归价值最高的一块反而没自动覆盖 |
| 11 | 问候语未标记时也能推进（回归 `L1066-1077` 那个「永远停在 greeting」的坑） | 症状是「一条消息都收不到」，无报错 |
| 12 | 致命错误 → `state:"failed"` 且不再重连（密码错不锁账号） | 安全/账号保护 |
| 13 | 关闭原因 120 字节截断（中文 + emoji 不切坏） | TS 版曾因此崩进程 |
| 14 | 策略拒绝的连接**一个字节都不发**（不是「等消息」挂住） | 「静默放行」是最坏失败方式 |
| 15 | 端口被占用时的行为符合 G3（自己的旧实例 → 视为已在运行；别人的 → 退出码 3 + 诊断） | 服务态全靠它 |
| 16 | 控制面：无 token 拒绝、命令白名单、跨用户访问被拒 | 新增攻击面 |
| 17 | 自更新：校验和不匹配 → 拒绝替换且**保留原二进制** | 一个能看邮箱的进程不能被投毒 |
| 18 | `port: 0` 下两个实例可同时跑（证明无全局状态） | 可测性 |
| 19 | 停服时挂着的 IDLE 连接不会把关停卡住（≤ 超时时间） | 回归 `L473-479` |
| 20 | 二进制从「安装位置」被删/被换后服务仍能报错而不是静默失联 | 真实用户会手动删文件 |

---

## 7. 已知缺口（TS 版没有、Rust 版要么补要么明确不做）

| # | 缺口 | 决定 |
| --- | --- | --- |
| I-1 | **STARTTLS（143 端口）不支持**，明文 143 也只当明文用 | 保持不支持。用户要 993；补 STARTTLS 要改扩展与协议 |
| I-2 | 没有**连接数上限**：多账号 + 恶意客户端可以开无限 TCP/TLS | 补：`max_connections`（默认 32），超限 → `close(1008, "too many connections")`（新行为，扩展侧无需感知） |
| I-3 | tag 匹配用正则 `^A\d{4}`，且**不知道哪个 tag 属于哪条命令**（只有 `phase`） | 补：维护 `HashMap<Tag, Command>`。当前实现靠「同一时刻只有一条在途命令」这一点成立，但它是个隐性不变量 |
| I-4 | 没有 `LOGOUT`：断开靠 `socket.destroy()` | 补：正常停服/watch 结束时发 `LOGOUT` 再关（服务器侧日志更干净）；失败不影响功能 |
| I-5 | 只处理 `INBOX`，无其他文件夹 | 保持（扩展也不需要） |
| I-6 | watch 只认 `EXISTS` 变大，**不认 `EXPUNGE` / 序号压缩** | 保持。游标是 UID，序号变化不影响增量抓取 |
| I-7 | 无 IPv6 监听支持（`HOST=::1` 在 TS 里靠 `ws` 兜着） | 补：`HOST` 支持 IPv6 字面量，`url()` 输出要补方括号（TS 的 `displayHost()` 已有此逻辑，`L1483-1485`） |
| I-8 | 无 HTTP 健康检查端点 | 不做（控制面已覆盖「服务活着吗」这个问题，别再开一个端口） |
| I-9 | 版本协商 | **计划中**，见 [`01-architecture.md § 8`](./01-architecture.md) 与 wire-protocol § 5 |

---

## 8. 待决（写代码前要拍板）

1. **`RELAY_TRACE` 在服务态怎么开？** 候选：① 只允许配置文件 + 重启生效；② 控制面 `TraceOn { seconds }` 临时开 N 秒；③ 干脆删掉服务态入口，只留前台。
   倾向 ②（可审计、会自己关掉），但需要在文档里明说「开 trace 期间日志含明文凭据」。
2. **`max_connections` 默认值**：32 是否够多账号用户？（每账号 1 条 watch + 抓取时并发几条）
3. **是否保留「用户级自启」作为默认**，还是直接默认系统服务？见 [`adr-0003-service-model.md`](./decisions/adr-0003-service-model.md)。
4. **自更新是否要「重启服务」**：Windows 上替换正在运行的 exe 需要 rename-then-move 技巧，涉及权限；
   见 [`design/update-flow.md`](./design/update-flow.md)。
