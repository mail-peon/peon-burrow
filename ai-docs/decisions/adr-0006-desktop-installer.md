# ADR-0006 · 桌面端形态：Tauri 2 服务安装器（不做自更新）

- **状态**：已采纳（**接口由本 ADR 定义，实现在 `peon-hall` 仓库**）
- **影响面**：core 必须提供的子命令（`service install/uninstall/start/stop/autostart`）、
  控制面命令集、sidecar 打包契约、发布流程
- **相关**：[`adr-0001`](./adr-0001-two-repos.md)、[`adr-0003`](./adr-0003-service-model.md)、
  [`design/control-plane-ipc.md`](../design/control-plane-ipc.md)、姊妹仓库 `peon-hall/ai-docs/`

---

## 背景

用户需求原文：

> 2. 要写用户界面，简易的皆可，可以使用 Tauri，类似一个服务安装器，
>    上面显示当前服务安装状态、启动状态，有几个操作，安装/卸载服务、启动/停止服务、开启/关闭开机启动。
> 6. 桌面端是免安装工具，不需要检查更新。

早期方案（`mail-peon/ai-docs/decisions/relay-deployment.md § 2.2`）建议的是
`native-dialog` 弹一个「只剩两个按钮」的窗口 + `windows-service` 注册。现在需求更具体了：
**要显示状态**，所以两个按钮的原生对话框不够用。

## 问题

| # | 问题 |
| --- | --- |
| Q1 | 用什么做 UI？（native-dialog / Tauri / egui / 无 GUI 的 CLI） |
| Q2 | GUI 与服务的职责边界在哪？服务注册谁做？ |
| Q3 | 免提权 GUI 怎么完成需要提权的操作？ |
| Q4 | 内嵌的 core 二进制怎么打包、怎么保证版本一致？ |
| Q5 | 桌面端要不要自更新？ |

## 决策

### 1. Q1：Tauri 2（`tauri` 2.12.2）

| 备选 | 否决理由 |
| --- | --- |
| `native-dialog` | 无法承载「状态展示 + 多个操作 + 诊断结果」；界面稍微一改就要换框架 |
| `egui` / `iced`（纯 Rust GUI） | 学习与迭代成本高；中文字体、列表、可复制文本这些基础体验都要自己搭 |
| **Tauri 2** ✅ | 前端用 Web 技术（迭代快、能复用既有经验）；Rust 侧能直接调 `peon-burrow-ipc` 与 spawn sidecar；体积与启动可接受 |
| 不做 GUI，只用 CLI | 目标用户（普通用户）不会用终端 —— 需求 2 明确要求界面 |

**代价要如实记**：三平台分别依赖 WebView2（Windows）/ WKWebView（macOS）/ WebKitGTK（Linux），
Linux 构建要装 `libwebkit2gtk-4.1-dev` 等系统包（见 [`adr-0007`](./adr-0007-release-pipeline.md) 的 CI 依赖）。

### 2. Q2：GUI 只是 driver，**服务注册逻辑唯一实现在 core**

```
GUI  ──(控制面 peon-burrow-ipc)──▶  运行中的服务        读状态 / 停止 / 重启 / 触发更新
GUI  ──(提权 spawn sidecar)──▶  core 的 service 子命令   安装 / 卸载 / 自启开关
```

| 事情 | 谁做 |
| --- | --- |
| 显示「已安装 / 未安装 / 自启开关」 | GUI 读**服务管理器**（SCM / launchd / systemd / 任务计划程序） |
| 显示「正在运行 / 端口 / 版本 / 连接数」 | GUI 读**控制面**（权威），发现文件只作缓存 |
| 安装 / 卸载 / 切自启 | core 的 `service …` 子命令（唯一实现） |
| 启动 / 停止 / 重启服务 | 走控制面让**服务自己**停/重启；只有「未运行时要启动」才由 `service start` 做 |

> ⚠️ 为什么 GUI 不自己写注册表 / plist / unit：这些细节（SCM 的 failure actions、
> launchd 的 `KeepAlive`、systemd 的 `Restart=`）是**中继自己**必须知道的（它要能自更新、
> 要能自我重启）。写两份必然漂移，而漂移的表现是「服务崩了不回来」这种极难归因的故障。

### 3. Q3：提权 = 以管理员身份 spawn 一个**同样的 core 二进制**

Tauri 应用**无法**在非提权状态下安装服务（`CreateServiceW` / `sc.exe` 都会 access denied），
而 Tauri 的 `WindowsConfig` **没有 elevation 字段**（那是 exe manifest 的事，不是运行时 API）。

采用的标准做法：

| 平台 | 做法 |
| --- | --- |
| Windows | sidecar 里带一个**已嵌入 `requestedExecutionLevel level="requireAdministrator"` manifest** 的辅助可执行文件（同一个 core 的二进制的第二份构建，或 core 自带 `--elevate` 自我的入口）；非提权 GUI spawn 它 → Windows 自动弹 UAC |
| macOS | `osascript -e 'do shell script "…" with administrator privileges'` |
| Linux | `pkexec` |

**用户级自启（默认路径）不需要提权**，所以安装器的主按钮路径是零 UAC 的（[`adr-0003`](./adr-0003-service-model.md)）。

⚠️ 已知坑（写进 desktop 的实现注意）：从 `perMachine` / `both` 模式安装的 NSIS 安装包启动的应用
**可能继承提权状态**（[tauri-apps/tauri#9835](https://github.com/tauri-apps/tauri/issues/9835)）——
不要假设「已安装 = 未提权」。安装包默认用 `perUser`（与「用户级自启」一致）。

### 4. Q4：sidecar 打包契约

```jsonc
// desktop/src-tauri/tauri.conf.json
"bundle": {
  "externalBin": ["binaries/burrow"],
  "createUpdaterArtifacts": false,          // Q5：不需要更新产物
  "targets": ["nsis", "msi", "dmg", "appimage", "deb"]
}
```

| 契约 | 细节 |
| --- | --- |
| 文件名 | 必须带 target triple 后缀：`binaries/burrow-x86_64-pc-windows-msvc.exe` |
| triple 从哪来 | `rustc --print host-tuple`（Rust ≥ 1.84） |
| 从哪拿二进制 | `gh release download <core tag> --repo mail-peon/peon-burrow`（见 [`adr-0001 § 决策 4`](./adr-0001-two-repos.md)） |
| 版本钉死 | peon-hall 仓库提交 `core-version.txt`；CI 可用输入 `core_ref` 覆盖 |
| 调用 | Rust：`app.shell().sidecar("burrow")`（**只用 basename**，不是配置里的路径） |
| 权限 | capability 里要 `shell:allow-execute` / `allow-spawn`，条目写成 `{ "name": "binaries/burrow", "sidecar": true }`，带参数时还要给 `args` 校验数组 |
| 校验 | 下载后用 core release 的 `SHA256SUMS` 校验，并把 core 版本写进 release notes |

### 5. Q5：桌面端**不做自更新**（需求 6）

- 不配 `plugins.updater`、不设 `createUpdaterArtifacts`、不引入 `tauri-plugin-updater`；
- GUI 只显示 core 的版本 + 「有新版本」提示 + 「立即更新」按钮（后者通过控制面
  `updateApply` 请求 **peon-burrow 自己**更新，见 [`design/update-flow.md`](../design/update-flow.md)）；
- 用户要新 GUI → 重新下载安装包。免安装工具不存在「必须自动更新」的压力。

> 这也让桌面端 CI 少一大块（签名密钥、`latest.json` 生成、updater 端点配置）。参考项目里
> `clash-verge-rev` 那套 `TAURI_SIGNING_PRIVATE_KEY` + `includeUpdaterJson` 的做法**不抄**。

### 6. 界面规格（实现细节见 peon-hall 仓库的 `ai-docs/01-ui-and-states.md`）

状态卡片（两个数据源拼出来的**四种可区分组合**，TS 版完全无法区分）：

| 已注册 | 控制面可连 | 文案 | 主按钮 |
| --- | --- | --- | --- |
| ✅ | ✅ | 正在运行 · 端口 41316 · v0.1.0 | 停止 / 重启 |
| ✅ | ❌ | 已安装，未运行 | 启动 |
| ❌ | ✅ | 前台运行中（未安装为服务） | 安装服务 |
| ❌ | ❌ | 未安装 | 安装服务 |

操作：**安装服务 / 卸载服务 / 启动 / 停止 / 开机自启开关**（需求 2 的五个），
外加三个低成本高收益的：**复制扩展地址**（`ws://127.0.0.1:41316/`）、
**打开配置/日志目录**、**诊断（`doctor`）**。

## 后果

### 好的

- 服务注册逻辑只有一份（core），GUI 换框架也不影响服务行为；
- 界面能表达「已安装但没跑」「跑着但没装」这类**TS 版无法区分**的状态，排查成本大降；
- 免 UAC 的默认安装路径 + 可选系统服务，覆盖两类用户；
- 控制面的 `peon-burrow-ipc-types` 类型由 core 定义 → GUI 与 CLI 永远同源。

### 代价（如实记录）

| 代价 | 说明 |
| --- | --- |
| 三平台 GUI 依赖 | WebView2 / WKWebView / WebKitGTK；Linux CI 要装系统包 |
| Tauri 应用体积 | 安装包几十 MB 量级（其中还内嵌了 core 二进制） |
| 需要第二个可执行文件（提权辅助） | 构建与发版矩阵各多一份产物；或者用「同一二进制 + manifest」的方式（待定，见后续 1） |
| GUI 与服务跨用户（系统服务模式） | 控制面必须有 TCP 退路（[`design/control-plane-ipc.md § 2`](../design/control-plane-ipc.md)） |

### 否决的方案

| 方案 | 否决理由 |
| --- | --- |
| `native-dialog` 两按钮 | 承载不了状态显示（需求 2） |
| 纯 Rust GUI（egui/iced） | 迭代成本与中文体验 |
| GUI 自己实现服务注册 | 逻辑两份必然漂移（Q2） |
| 用 SCM 控制码启停 | 非提权 GUI 会 `ERROR_ACCESS_DENIED`（[`adr-0003 § 6`](./adr-0003-service-model.md)） |
| 桌面端也做自更新 | 需求明确不要；且会引入签名密钥与 updater 端点的维护成本 |

## 后续

1. **提权辅助的形态**：倾向在**产品 crate `peon-burrow` 里再加一个 `[[bin]]`**（如 `burrow-elevate`），
   带 `requireAdministrator` manifest —— 一个 crate 多个 bin，与 `cargo-bumpp` 的做法一致
   （[`adr-0008 § 5`](./adr-0008-library-first-layout.md)）；
2. 是否提供「服务未安装时也能临时前台运行」的按钮：倾向**提供**（一键 `run`），
   它让用户在没有服务权限的机器上也能用；
3. Linux 的 `pkexec` 在没有 polkit agent 的桌面上会失败 → 需要给出「用终端执行」的降级文案；
4. macOS 13+ 的 `SMAppService` 迁移（[`adr-0003` 后续 1](./adr-0003-service-model.md)）。
