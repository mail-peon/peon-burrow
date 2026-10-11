//! `burrow` 命令行入口：**只做三件事**（布局铁律 L5：bin ≤ 60 行）。
//!
//! 真正的逻辑在 `peon_burrow::run` —— 这样同样的行为可以在集成测试里直接调用。

use clap::Parser;

use peon_burrow::{Cli, run};

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();

    // Windows 服务形态：SCM 拉起的进程必须**先**起 dispatcher（它要占着主线程，阻塞到服务停止），
    // 否则 SCM 等不到报到 → `sc start` 报 1053。缺的就是这一步。
    #[cfg(windows)]
    if let Some(name) = cli.service.clone() {
        let code = peon_burrow::service::run_as_service(&name);
        return std::process::ExitCode::from(code.as_u8());
    }

    let code = run(cli);
    std::process::ExitCode::from(code.as_u8())
}
