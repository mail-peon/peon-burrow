# 设计 · 扩展 ↔ 中继 线上协议（**冻结**）

> ⚠️ **这份文档描述的是契约，不是实现。** 冻结的含义：
> 改动这里的任何一项，都必须**同时**改 `mail-peon` 扩展（`src/adapters/mail/transport/relay.ts`、
> `transport/watch.ts`、`providers/imap/index.ts`），并当作**破坏性变更**发版。
>
> 实现的权威来源是 TS 版：`../../../../mail-peon/scripts/imap-relay.ts`。
> Rust 版必须与之逐字兼容，除非本文明确标注「计划扩展」。

---

## 1. 端点

```
ws://<relayHost>:<relayPort>/<path>[?query]
```

| 项 | 值 |
| --- | --- |
| 传输 | 明文 WebSocket（`ws://`）。**本地回环场景没有 TLS**，也不要加：中继与扩展在同一台机器上，加 `wss` 只会引入证书管理问题 |
| 监听 | 默认 `127.0.0.1`，默认端口见 [`port-and-discovery.md`](./port-and-discovery.md) |
| 两种模式 | 共用同一个端点，靠**第一帧**区分（§ 3） |
| 每连接 | 一条 WebSocket ↔ 一条 TCP/TLS，**永不复用**（IMAP 有状态） |

---

## 2. 透传模式

### 2.1 URL 形态

扩展侧 `buildRelayUrl()`（`mail-peon/src/adapters/mail/transport/relay.ts:30-46`）产出两种：

| 形态 | 例子 | 何时产生 |
| --- | --- | --- |
| 路径 | `ws://127.0.0.1:41316/imap.qq.com:993?tls=1` | 用户填的中继地址没有路径（`/`） |
| 查询 | `ws://relay.local/tunnel?host=imap.qq.com&port=993&tls=1` | 用户填的中继地址已带路径（如自建反代挂在子路径下） |

解析规则（`imap-relay.ts:1393-1432`）：

| 参数 | 规则 |
| --- | --- |
| `host` | 先看 `?host=`；没有则从路径解析 `/<host>:<port>`（路径支持 IPv6 方括号写法 `[::1]:993`） |
| `port` | 同上；缺省或非法（≤0 / >65535 / 非整数）时按 `tls ? 993 : 143` |
| `tls` | `?tls=0` → 明文；**其它任何值（含缺省）都算 TLS** |
| `token` | `?token=`，与中继配置的 `RELAY_TOKEN` 比对 |
| **都没有** | 抛 `NoTargetError`：**不算错误**，留着等第一帧（可能是 watch） |

### 2.2 拒绝规则（全部以 `1008` 关闭，且**不发任何数据字节**）

| 条件 | 关闭原因（人类可读，≤120 字节） |
| --- | --- |
| `tls=0` 且 `port=993` | `993 端口是 implicit TLS，必须开启 TLS（143 + STARTTLS 本版本不支持）` |
| 目标是 loopback 且不在 `ALLOWED_HOSTS` 里 | `拒绝连接本机地址（请显式加进 ALLOWED_HOSTS）` |
| `RELAY_TOKEN` 已设且不匹配 | `invalid token` |
| `ALLOWED_HOSTS` 非空且 host 不在其中 | `host not allowed` |
| 第一帧不是 watch 且 URL 里没有目标 | `缺少目标 host（用 /host:port 或 ?host=&port=）` |

> ⚠️ 这些检查必须在 `connection` 事件里**同步**完成。被拒的连接一个字节都不会发，
> 任何「等第一帧再决定」的写法都会让它永远挂着 = **静默放行**。

### 2.3 透传时序（不可变）

1. WebSocket 升级完成；
2. **立刻**建 TCP/TLS（不等第一帧）—— 有些客户端一个字节都不发，只等连接结果；
3. 第一帧到达：**二进制** → 按原样写入 socket；
4. 之后所有帧原样双向转发；
5. 任一方向关闭/出错/超时（默认 15 分钟空闲） → 关另一边。

`tls=1` 时中继负责 TLS 握手，**`servername`（SNI）用目标主机名，IP 字面量不发 SNI**；
证书链校验默认**开启**（`TLS_REJECT_UNAUTHORIZED=0` 只用于自签证书的测试环境）。

---

## 3. 第一帧分流

```
第一帧是 文本帧  且  JSON.parse 后 __watch === 1   →  watch 模式
其它一切情况（二进制帧、非 JSON、JSON 但没有 __watch、__watch 是别的值、字符串/数字）→ 透传
```

- 判据**只有** `__watch === 1` 这一条，别的字段一概不看：目标与凭据在各自的层里校验，
  这样「字段暂时缺失」的连接能走到给出**具体**报错的那一层。
- 判定为 watch 时，中继会**先关掉刚建好的 TCP**（代价：邮件服务器上留一条「连上就断」的记录）。
- 判定为透传时，被分流扣下的**第一帧必须补投**。
- 顺序：先建 TCP → 再分流 → 再转发。任何「等消息 + `setImmediate`」的写法都是结构性竞态。

---

## 4. watch 模式

### 4.1 请求（客户端 → 中继，文本帧 JSON）

```json
{
  "__watch": 1,
  "accountId": "acc_xxx",
  "host": "imap.qq.com",
  "port": 993,
  "tls": true,
  "user": "you@qq.com",
  "pass": "授权码",
  "token": ""
}
```

| 字段 | 类型宽容度 | 说明 |
| --- | --- | --- |
| `__watch` | 必须 `=== 1` | 分流唯一判据 |
| `accountId` | 任意，缺省 `"unknown"` | 原样回显在推送里，扩展据此定位账号 |
| `host` | 必须非空 | |
| `port` | 必须 1–65535 的整数 | 缺省/非法 → `watch 请求缺少有效的 host / port` |
| `tls` | **`false` 与 `0` 都算明文**，其余算 TLS | 扩展发布尔；容忍数字写法 |
| `user` / `pass` | 必须非空 | 缺 → `watch 请求缺少 user / pass` |
| `token` | 与 `RELAY_TOKEN` 比对 | |

校验失败 → 关闭码 `1011`，并**先**推一条 `{"state":"failed","error":"…"}`。

### 4.2 响应（中继 → 客户端，**文本帧** JSON）

| 消息 | 何时 |
| --- | --- |
| `{"type":"state","state":"watching","exists":N}` | `SELECT INBOX` 成功、挂上 IDLE。`exists` 是**基准值** |
| `{"type":"mail","accountId":"…","exists":N}` | 服务器报的 `EXISTS` **比基准大**时。**不含邮件内容** |
| `{"type":"state","state":"reconnecting","retryInMs":N}` | 连接断了，正在退避重连。退避阶梯 `[2,5,15,30,60,120,300]` 秒 |
| `{"type":"state","state":"error","error":"…"}` | 本轮出错但还会重试（可观测用，扩展当前忽略） |
| `{"type":"state","state":"failed","error":"…"}` | **重试无用**（密码错、白名单拒绝）。中继**停止重连**，扩展也**不要重连**，提示用户改配置 |

⚠️ 扩展侧对帧形态是宽容的（`string` / `ArrayBuffer` / `ArrayBufferView` 都接），
但**中继必须继续发文本帧**：TS 版曾因扩展侧误设 `binaryType='arraybuffer'`
导致「中继推了、扩展一条没收到」，两边都不报错。

### 4.3 watch 的 IMAP 状态机

```
greeting ──(未标记的 * OK / * PREAUTH)──▶ login ──(A000n OK)──▶ select ──(A000n OK)──▶ idle
                                                                                      │  ▲
                                                          25 分钟 ──DONE──▶ done ──────┘
                                                                    (A000n OK)
```

| 细节 | 值 | 为什么 |
| --- | --- | --- |
| 问候语 | **未标记**的 `* OK […]` / `* PREAUTH` | 它没有 tag，靠「等某个 tag」判断会永远停在 greeting（曾经的真 bug） |
| tag | `A0001`、`A0002`… | 便于日志对照 |
| 基准值 | `SELECT` 那一刻的 `EXISTS` | SELECT 之前就有的邮件不会被误推 |
| 推送条件 | `exists > notified`（**只在变大时**） | 变小 = 别处删了邮件，不是新邮件 |
| 重挂 IDLE | 25 分钟（RFC 2177 建议 ≤ 29 分钟） | 多数服务器 30 分钟断空闲连接 |
| 空闲超时 | **禁用** | IDLE 可以静默几十分钟，15 分钟的空闲超时会误杀 |
| `+ idling` | 忽略 | 只是「进入 IDLE」的继续请求 |
| 致命错误 | `登录失败` / `无法打开收件箱` / `invalid token` / `host not allowed` / `缺少有效的…` / `缺少 user…` | 密码错时快速重连会把账号锁掉（QQ 会直接拒连一段时间） |

---

## 5. 关闭码

| 码 | 含义 | 触发点 |
| --- | --- | --- |
| `1008` | 策略拒绝 | token / 白名单 / `tls=0`+993 / 无目标 |
| `1011` | watch 启动失败 | 请求字段非法等 |
| `1001` | 中继正在重启 | 用户在前台控制台敲 `r`，或服务收到重启指令 |
| `1000` / 无 | 正常关闭 | 客户端主动关、或对端正常关闭 |

**关闭原因上限 123 字节（WebSocket 规范），中继一律截断到 ≤ 120 字节且不切坏 UTF-8。**
（TS 版曾因超长原因导致 `ws.close()` 抛异常 → 整个进程崩 → 一个非法请求即可 DoS。）

### 5.1 计划扩展：版本协商（未实现）

| 方向 | 字段 | 兼容规则 |
| --- | --- | --- |
| 扩展 → 中继 | `protocol`（缺省视为 `1`）、`clientVersion` | 中继只认 `1` 与 `2`；更高 → `state:"failed"` 提示升级中继 |
| 中继 → 扩展 | `protocol`、`relayVersion`（附加在 `state:"watching"` 上） | 扩展忽略未知字段；据此提示「中继过新，请更新扩展」 |

**不新增关闭码语义**，复用现有 `failed` 通道。

---

## 6. 与 `mail-peon` 的联动清单（P6，跨仓库）

| # | 位置 | 现状 | 要改成 |
| --- | --- | --- | --- |
| L1 | `src/adapters/mail/providers/imap/index.ts:62-63` | `default` / `placeholder` = `ws://127.0.0.1:8787/` | 新默认端口（见 [`port-and-discovery.md`](./port-and-discovery.md)） |
| L2 | `src/options/pages/AccountsPage.vue:152,156` | 文案写 `pnpm relay` + `ws://127.0.0.1:8787/` | 改为「安装 peon-hall后自动可用」+ 新端口 |
| L3 | `src/adapters/mail/transport/relay.ts:56-59` | 错误文案「当前环境没有裸 TCP…」 | 与 L2 统一（产品名统一为「peon-hall / Relay」） |
| L4 | `src/adapters/mail/transport/watch.ts:78` | 注释里的示例地址 | 新端口 |
| L5 | `ai-docs/decisions/relay-deployment.md § 2.3` | 标着「待实现」的目标表 | 本文档落地后逐条勾掉 |
| L6 | `src/adapters/mail/__tests__/watch-client.spec.ts:35,63,183` | 测试里的 `8787` | 改端口（只是字面量，不影响断言） |

> ⚠️ 在 L1 落地之前，用户必须**手填**新端口地址。所以 Rust 版首版发出去时，
> 安装器要在界面上**显式给出这个地址**（可一键复制），不能指望用户猜。

---

## 7. 扩展**不会**用到的能力（可以自由演进）

以下都属于中继内部实现或本仓库新增能力，改它们不构成协议变更：

- 控制面（[`control-plane-ipc.md`](./control-plane-ipc.md)）的全部内容；
- 发现文件、日志路径、`doctor` 输出；
- 服务注册方式、自更新流程；
- `max_connections` 限制（超限时用 `1008` 拒绝，扩展侧只会看到「连不上」，
  但正常情况下远达不到上限）；
- 连接级的背压参数、超时参数（默认值不变即可）。
