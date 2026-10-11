//! 把 `icons/icon.ico` 嵌进 `burrow.exe`。
//!
//! 为什么需要它：不嵌的话，任务管理器 / 服务列表（`services.msc`）/ 快捷方式里的
//! `burrow.exe` 是**通用 exe 图标**——一个要装成服务长期跑的东西，看起来像没做完。
//!
//! 图标来源：`.github/images/burrow.png`（README 的概念图，300×300 透明 PNG）。
//! 重做 `icons/icon.ico`（多尺寸 16/24/32/48/64/128/256）的办法写在
//! `ai-docs/02-tech-stack.md` 的依赖表下面，别手改二进制。
//!
//! ⚠️ 两道闸，缺一个 CI 就红：
//!
//! 1. **只在目标平台是 Windows 时才嵌**（`CARGO_CFG_TARGET_OS`）。CI 会在 Linux 上跑
//!    `cargo clippy --workspace --all-targets --target x86_64-pc-windows-msvc`，
//!    那台机器上没有 `rc.exe`，无条件嵌会让整个 job 挂掉；
//! 2. **只在构建机也是 Windows 时才有真实现**（`#[cfg(windows)]`，build script 是按
//!    **宿主**编译的）。交叉构建出 Windows 产物时不嵌图标，而不是去猜工具链在哪。
//!
//! 失败只警告不报错：图标是装饰，嵌不进去（裁剪过的构建环境里没有 `rc.exe`）不该让
//! 用户的 `cargo build` 直接失败。

fn main() {
    println!("cargo:rerun-if-changed=icons/icon.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_icon();
    }
}

/// 真实现：交给 `winresource` 找 `rc.exe` 并编译 `.rc`。
///
/// ⚠️ 它**自己会在 Windows SDK 里找 `rc.exe`**，不要求 `rc.exe` 在 PATH 里
/// （实测：PowerShell 里 `Get-Command rc.exe` 找不到，照样能嵌）。
#[cfg(windows)]
fn embed_icon() {
    let mut resource = winresource::WindowsResource::new();
    resource.set_icon("icons/icon.ico");
    if let Err(error) = resource.compile() {
        println!("cargo:warning=嵌入 exe 图标失败（不影响功能，exe 会用系统默认图标）：{error}");
    }
}

/// 构建机不是 Windows（交叉构建）：不嵌图标。
#[cfg(not(windows))]
fn embed_icon() {}
