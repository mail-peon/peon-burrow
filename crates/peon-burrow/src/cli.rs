//! 命令行定义（`clap`）。
//!
//! ⚠️ 子命令里**没有**「读凭据」「改端口」之外的魔法：CLI 只做用户能理解的事
//! （见 `ai-docs/design/cli.md`）。`burrow` 不带子命令时等价于 `burrow status`。

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::config::{CliOverrides, LogLevel};

/// WebSocket → TCP/TLS 中继，带服务安装与自更新。
#[derive(Debug, Clone, Parser)]
#[command(
    name = "burrow",
    version,
    about = "把浏览器扩展的 WebSocket 连接转成到邮件服务器的 TCP/TLS 连接",
    long_about = None,
    disable_help_subcommand = true
)]
pub struct Cli {
    /// 子命令（省略 = status）。
    #[command(subcommand)]
    pub command: Option<Command>,

    /// 配置文件路径。
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// 监听地址。
    #[arg(long, global = true, value_name = "HOST")]
    pub host: Option<String>,

    /// 监听端口。
    #[arg(long, global = true, value_name = "PORT")]
    pub port: Option<u16>,

    /// 访问 token。
    #[arg(long, global = true, value_name = "TOKEN")]
    pub token: Option<String>,

    /// 允许的邮件服务器（可重复，支持 `*` 通配）。
    #[arg(long = "allowed-host", global = true, value_name = "HOST")]
    pub allowed_hosts: Vec<String>,

    /// 日志级别。
    #[arg(long, global = true, value_name = "LEVEL")]
    pub log_level: Option<String>,

    /// 打开明文日志（含凭据，慎用）。
    #[arg(long, global = true)]
    pub trace: bool,

    /// 以 JSON 输出（脚本用）。
    #[arg(long, global = true)]
    pub json: bool,
}

impl Cli {
    /// 抽出配置覆盖项（`--trace` 是开关，只有给了才算覆盖）。
    pub fn overrides(&self) -> CliOverrides {
        CliOverrides {
            config: self.config.clone(),
            host: self.host.clone(),
            port: self.port,
            token: self.token.clone(),
            allowed_hosts: if self.allowed_hosts.is_empty() {
                None
            } else {
                Some(self.allowed_hosts.clone())
            },
            log_level: self.log_level.clone(),
            trace: self.trace.then_some(true),
            tls_reject_unauthorized: None,
        }
    }

    /// 日志级别的默认值（配置文件里没有时用）。
    pub fn log_level_or_default(&self) -> LogLevel {
        LogLevel::Info
    }
}

/// 子命令。
#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// 前台运行中继（Ctrl+C 退出）。
    Run {
        /// 关掉控制面（只在中继出问题时用）。
        #[arg(long)]
        no_control: bool,
    },

    /// 问运行中的中继要状态。
    Status,

    /// 让运行中的中继停下来。
    Stop,

    /// 重启运行中的中继。
    Restart,

    /// 自检：配置、目录、端口、安全项。
    Doctor {
        /// 输出更多细节。
        #[arg(short, long)]
        verbose: bool,
    },

    /// 打印版本与协议版本。
    Version,

    /// 安装/卸载/启停自启项。
    #[command(subcommand)]
    Service(ServiceCommand),

    /// 检查更新（默认行为）。
    Update {
        /// 显式要求「只检查」（与不带参数的默认行为一致，写出来更清楚）。
        #[arg(long, conflicts_with = "apply")]
        check: bool,

        /// 立刻检查，忽略节流。
        #[arg(long)]
        force: bool,

        /// 直接下载并替换（完成后需要重启服务）。
        #[arg(long)]
        apply: bool,
    },

    /// 打印扩展里要填的中继地址。
    Url,

    /// 生成一个随机 token。
    Token,

    /// 临时打开/关闭明文日志。
    Trace {
        /// 关闭。
        #[arg(long)]
        off: bool,

        /// 持续秒数（默认 60，上限 3600）。
        #[arg(long, default_value_t = 60)]
        seconds: u32,
    },
}

/// 服务相关子命令。
#[derive(Debug, Clone, Subcommand)]
pub enum ServiceCommand {
    /// 安装自启项。
    Install {
        /// 装成系统服务（需要一次提权）。
        #[arg(long)]
        system: bool,

        /// 不自启，只注册。
        #[arg(long)]
        no_autostart: bool,
    },

    /// 卸掉自启项。
    Uninstall,

    /// 启动。
    Start,

    /// 停止。
    Stop,

    /// 重启。
    Restart,

    /// 看注册状态。
    Status,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_tree_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn no_subcommand_means_status() {
        let cli = Cli::parse_from(["burrow"]);
        assert!(cli.command.is_none());
    }

    #[test]
    fn global_flags_work_after_the_subcommand() {
        let cli = Cli::parse_from(["burrow", "run", "--port", "41317", "--log-level", "debug"]);
        assert_eq!(cli.port, Some(41317));
        assert_eq!(cli.log_level.as_deref(), Some("debug"));
        assert!(matches!(cli.command, Some(Command::Run { .. })));
    }

    #[test]
    fn allowed_hosts_can_be_repeated() {
        let cli = Cli::parse_from([
            "burrow",
            "run",
            "--allowed-host",
            "imap.qq.com",
            "--allowed-host",
            "*.163.com",
        ]);
        let overrides = cli.overrides();
        assert_eq!(
            overrides.allowed_hosts,
            Some(vec!["imap.qq.com".to_owned(), "*.163.com".to_owned()])
        );
    }

    #[test]
    fn trace_is_only_an_override_when_given() {
        let cli = Cli::parse_from(["burrow", "run"]);
        assert_eq!(cli.overrides().trace, None);
        let cli = Cli::parse_from(["burrow", "run", "--trace"]);
        assert_eq!(cli.overrides().trace, Some(true));
    }

    #[test]
    fn service_subcommands_parse() {
        let cli = Cli::parse_from(["burrow", "service", "install", "--system"]);
        assert!(matches!(
            cli.command,
            Some(Command::Service(ServiceCommand::Install {
                system: true,
                no_autostart: false
            }))
        ));
        let cli = Cli::parse_from(["burrow", "trace", "--off"]);
        assert!(matches!(
            cli.command,
            Some(Command::Trace { off: true, .. })
        ));
    }
}
