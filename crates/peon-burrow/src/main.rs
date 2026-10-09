//! `burrow` 命令行入口：**只做三件事**（布局铁律 L5：bin ≤ 60 行）。
//!
//! 真正的逻辑在 `peon_burrow::run` —— 这样同样的行为可以在集成测试里直接调用。

use clap::Parser;

use peon_burrow::{Cli, run};

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let code = run(cli);
    std::process::ExitCode::from(code.as_u8())
}
