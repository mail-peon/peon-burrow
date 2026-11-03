use super::scm::inspect_sc_exit_code;
use super::*;
use crate::host::{Autostart, InstallOptions, ServiceError, ServiceHost};
use crate::runner::{CommandSpec, FakeRunner, Mutation};

fn sample_opts() -> InstallOptions {
    let mut opts = InstallOptions::new(
        "peon-burrow",
        r"C:\Users\me\AppData\Local\Programs\peon-burrow\burrow.exe",
    );
    opts.args = vec!["run".to_owned()];
    opts
}

/// 系统级安装的选项：命令行**必须**带 `--service <名字>`
/// （`prepare_install` 会校验，见 `require_windows_service_flag`）。
fn system_opts() -> InstallOptions {
    let mut opts = sample_opts();
    opts.level = ServiceLevel::System;
    opts.autostart = Autostart::Boot;
    opts.args = vec!["run".to_owned(), "--service".to_owned(), opts.name.clone()];
    opts
}

/// `schtasks /Query /XML` 真正返回的那种形状（节选，够覆盖我们要读的字段）。
fn task_xml_response() -> String {
    task_xml(&sample_opts())
}

/// `reg query … /v FailureActions` 的**真机**输出（在装好系统服务的机器上导出）。
///
/// 解码：reset=86400 秒，3 个动作都是 `(restart, 5000ms)`。
/// 不用 `sc qfailure` 的文字：它会随系统语言变（同一台机器上出现过中文的 `成功`）。
const FAILURE_FIXTURE: &str = r"HKEY_LOCAL_MACHINE\SYSTEM\CurrentControlSet\Services\peon-burrow
FailureActions    REG_BINARY    8051010000000000000000000300000014000000010000008813000001000000881300000100000088130000
";

/// 拼一个 `reg query` 形状的夹具（只给 hex，省得每次手写整行）。
fn reg_binary_fixture(hex: &str) -> String {
    format!(
        "HKEY_LOCAL_MACHINE\\SYSTEM\\CurrentControlSet\\Services\\peon-burrow\n    FailureActions    REG_BINARY    {hex}\n"
    )
}

fn assert_sc_options_are_separate(commands: &[CommandSpec]) {
    const OPTIONS: [&str; 12] = [
        "type=",
        "start=",
        "error=",
        "binPath=",
        "group=",
        "tag=",
        "depend=",
        "obj=",
        "DisplayName=",
        "password=",
        "reset=",
        "actions=",
    ];
    for command in commands {
        if command.program != "sc" {
            continue;
        }
        for arg in &command.args {
            for option in OPTIONS {
                if let Some(rest) = arg.strip_prefix(option) {
                    assert!(
                        rest.is_empty(),
                        "`{option}` 必须与它的值分成两个参数，现在是 {arg:?}（{}）",
                        command.purpose
                    );
                }
            }
        }
    }
}

/// 取这次测试里跑过的所有命令。
fn commands_of(runner: &FakeRunner) -> Vec<CommandSpec> {
    runner
        .mutations()
        .all()
        .into_iter()
        .filter_map(|mutation| match mutation {
            Mutation::Run(spec) => Some(spec),
            _ => None,
        })
        .collect()
}

mod tasks;

mod command;

mod user;

mod system;
