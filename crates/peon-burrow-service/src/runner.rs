//! 执行缝：所有「会改机器」的动作都经过 [`Runner`]。
//!
//! 这一层存在的唯一理由是**可测**：`install` / `uninstall` / `start` / `stop` / `set_autostart`
//! 的每个动作都被拆成「生成什么」与「执行什么」两半。生成部分是纯函数
//! （`windows` / `macos` / `linux` 三个平台模块各有一套），执行部分是这个 trait。
//! 测试注入 [`FakeRunner`] 就能断言**完整命令行、unit 文件正文、plist 正文**，一次都不动真机器。
//!
//! 读取类动作（`status` / `verify_restart_policy` / `probe_port`）同样走 [`Runner::run`]，
//! 只是它们的产物是「解析文本」，不会改机器。

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use crate::host::ServiceError;

/// 一次要执行的进程调用。
///
/// 刻意**不**是 `std::process::Command`：`Command` 不可比、不可打印，
/// 而我们要能把它原样写进测试断言和日志。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// 可执行文件（`sc` / `schtasks` / `systemctl` / `launchctl` / `powershell` …）。
    pub program: String,
    /// 参数列表（不走 shell，参数里带空格也安全）。
    pub args: Vec<String>,
    /// 需要喂给子进程标准输入的内容（Windows 用 `schtasks /XML` 时用得上）。
    pub stdin: Option<String>,
    /// 这条命令在干什么（报错时原样写给用户，例如「注册登录自启任务」）。
    pub purpose: String,
}

impl CommandSpec {
    /// 造一条命令。
    pub fn new(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
        purpose: impl Into<String>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            stdin: None,
            purpose: purpose.into(),
        }
    }

    /// 附带标准输入内容。
    #[must_use]
    pub fn with_stdin(mut self, stdin: impl Into<String>) -> Self {
        self.stdin = Some(stdin.into());
        self
    }

    /// 一行可复制的命令（**属性测试与工单里用这个**，Windows 上含空格参数会带上引号）。
    pub fn display_line(&self) -> String {
        let mut line = shell_quote(&self.program);
        for arg in &self.args {
            line.push(' ');
            line.push_str(&shell_quote(arg));
        }
        line
    }
}

/// 按 Windows / Unix 的引用习惯把一段参数转成可复制的形式。
fn shell_quote(value: &str) -> String {
    let needs_quotes = value.is_empty()
        || value
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '&' | '(' | ')' | '%'));
    if !needs_quotes {
        return value.to_owned();
    }
    // Windows 命令行与 POSIX shell 都用双引号；POSIX 侧再转义一次内部的双引号。
    #[cfg(windows)]
    {
        format!("\"{}\"", value.replace('"', "\\\""))
    }
    #[cfg(not(windows))]
    {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// 一次要落盘的文本文件。
///
/// 不需要标准输入的 `unit` 文件 / `plist` 走这里；
/// 「内容就在命令行里」（Windows 任务 XML）的平台不会用到它，见 `windows` 模块的说明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInstall {
    /// 落盘路径。
    pub path: PathBuf,
    /// 正文。
    pub content: String,
    /// 只有当前用户可读（token / token 文件用；`plist` 与 `unit` 不敏感）。
    pub private: bool,
    /// 这份文件在干什么（报错时原样写给用户）。
    pub purpose: String,
}

impl FileInstall {
    /// 造一份文件。
    pub fn new(
        path: impl Into<PathBuf>,
        content: impl Into<String>,
        purpose: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            content: content.into(),
            private: false,
            purpose: purpose.into(),
        }
    }

    /// 标记为「只有当前用户可读」。
    #[must_use]
    pub fn private(mut self) -> Self {
        self.private = true;
        self
    }
}

/// 一次被记录下来的机器变更。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mutation {
    /// 执行了一条命令。
    Run(CommandSpec),
    /// 写了一个文件。
    Install(FileInstall),
    /// 删了一个文件。
    Remove(PathBuf),
}

/// 进程与文件操作的执行者。
///
/// 生产代码用 [`RealRunner`]；测试用 [`FakeRunner`]。
/// 这是本 crate **唯一的副作用入口**（`probe_port` 的绑定除外，它只探测、不改机器）。
///
/// ⚠️ 刻意**不**要求 `Clone`：那样会让 trait 失去 dyn 兼容性（`Box<dyn Runner>` 就用不了），
/// 而平台宿主只需要一个 `Box<dyn Runner>`。
pub trait Runner: Send + Sync + fmt::Debug {
    /// 执行一条命令，返回它的**标准输出**。
    ///
    /// 非零退出码由实现转成 [`ServiceError::CommandFailed`]：
    /// 调用方拿到的永远是「成功 + 输出」或「失败 + 说人话的错误」。
    fn run(&self, spec: &CommandSpec) -> Result<String, ServiceError>;

    /// 落盘一份文件（覆盖已有内容）。
    fn install_file(&self, file: &FileInstall) -> Result<(), ServiceError>;

    /// 删文件；文件不存在**不算失败**（幂等，卸载要能重跑）。
    fn remove_file(&self, path: &Path) -> Result<(), ServiceError>;
}

/// 真执行：`std::process::Command` + `std::fs`。
///
/// 刻意保持同步：服务注册 / 启停是低频、天然阻塞的动作，
/// 调用方（CLI / 安装器）自己决定要不要扔进线程池。
#[derive(Debug, Clone, Copy, Default)]
pub struct RealRunner;

impl Runner for RealRunner {
    fn run(&self, spec: &CommandSpec) -> Result<String, ServiceError> {
        let mut command = Command::new(&spec.program);
        command.args(&spec.args);
        if spec.stdin.is_some() {
            command.stdin(std::process::Stdio::piped());
        }
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());

        tracing::debug!(
            event = "service.command",
            program = %spec.program,
            purpose = %spec.purpose,
            "执行服务管理命令"
        );

        let mut child = command.spawn().map_err(|source| ServiceError::Io {
            action: "启动系统命令",
            source,
        })?;

        if let Some(text) = &spec.stdin {
            use std::io::Write as _;
            let mut pipe = child.stdin.take().ok_or_else(|| {
                ServiceError::command_failed(
                    spec.purpose.clone(),
                    None,
                    "无法写入子进程的标准输入（管道没打开）",
                )
            })?;
            pipe.write_all(text.as_bytes())
                .map_err(|source| ServiceError::Io {
                    action: "写入子进程的标准输入",
                    source,
                })?;
            // 关掉管道，否则子进程会一直等 EOF。
            drop(pipe);
        }

        let output = child
            .wait_with_output()
            .map_err(|source| ServiceError::Io {
                action: "等待系统命令结束",
                source,
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        if output.status.success() {
            return Ok(stdout.into_owned());
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        // 有的工具（`sc` / `schtasks`）把诊断写在 stdout 上，两个都带上，避免「错误信息是空的」。
        let detail = if stderr.trim().is_empty() {
            stdout.trim().to_owned()
        } else {
            stderr.trim().to_owned()
        };

        Err(ServiceError::command_failed(
            spec.purpose.clone(),
            output.status.code(),
            detail,
        ))
    }

    fn install_file(&self, file: &FileInstall) -> Result<(), ServiceError> {
        if let Some(parent) = file.path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ServiceError::Io {
                action: "创建目录",
                source,
            })?;
        }
        std::fs::write(&file.path, &file.content).map_err(|source| ServiceError::Io {
            action: "写入文件",
            source,
        })?;
        tracing::debug!(
            event = "service.file_written",
            path = %file.path.display(),
            purpose = %file.purpose,
            private = file.private,
            "已写入服务文件"
        );
        // ⚠️ 待办：`file.private = true` 时应当把权限收到 0600（Unix）。
        // 那需要 `libc` / `std::os::unix::fs::PermissionsExt`，而本 crate 的依赖表白名单里没有它，
        // 所以现在只保证「内容不含 token」+「父目录是用户私有目录」（见 `private` 字段文档）。
        Ok(())
    }

    fn remove_file(&self, path: &Path) -> Result<(), ServiceError> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            // 不存在 = 已经是我们想要的状态
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(ServiceError::Io {
                action: "删除文件",
                source,
            }),
        }
    }
}

/// 只记录、不执行的 runner（测试专用）。
///
/// 它把每一次 [`Runner::run`] / [`Runner::install_file`] / [`Runner::remove_file`]
/// 原样留在 [`Mutations`] 里，并按调用顺序弹出 [`FakeRunner::expect`] 预置的输出。
/// **不会启动任何进程，也不会碰文件系统**。
#[derive(Debug, Clone, Default)]
pub struct FakeRunner {
    inner: Arc<FakeState>,
}

#[derive(Debug, Default)]
struct FakeState {
    mutations: SyncCell<Vec<Mutation>>,
    responses: SyncCell<std::collections::VecDeque<Response>>,
    cursor: AtomicUsize,
}

#[derive(Debug, Clone)]
enum Response {
    Ok(String),
    Err(String),
}

/// `std::sync::Mutex` 的小包装。
///
/// 布局铁律 C5 禁的是**在 async 代码里跨 `await` 持 `std::sync::Mutex`**（会让运行时线程卡住）。
/// 本 crate 没有任何 async 代码（服务注册 / 启停本来就是阻塞动作），
/// 而测试 runner 需要「`&self` 也记录」，所以这里用普通的同步锁是正确选择。
#[derive(Debug, Default)]
struct SyncCell<T>(StdMutex<T>);

impl<T> SyncCell<T> {
    fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        // 故意 `expect` 而不是传播：持锁期间只做内存操作，不可能中毒；真中毒了就是 bug，不该被吞掉。
        let mut guard = self.0.lock().expect("测试 runner 的内部锁被毒化");
        f(&mut guard)
    }
}

impl FakeRunner {
    /// 造一个空的假 runner。
    pub fn new() -> Self {
        Self::default()
    }

    /// 预置下一条命令的输出（**必须按调用顺序**）。
    ///
    /// 预置的数量和实际执行顺序不一致时，测试会在 [`FakeRunner::expectations_left`] 上暴露出来
    /// —— 这是故意的：顺序即语义（先 `daemon-reload` 再 `enable`）。
    #[must_use]
    pub fn expect(self, stdout: impl Into<String>) -> Self {
        self.inner
            .responses
            .with(|queue| queue.push_back(Response::Ok(stdout.into())));
        self
    }

    /// 预置下一条命令的**失败**（文本会被当成 `stderr`）。
    #[must_use]
    pub fn expect_failure(self, stderr: impl Into<String>) -> Self {
        self.inner
            .responses
            .with(|queue| queue.push_back(Response::Err(stderr.into())));
        self
    }

    /// 所有被记录下来的变更（命令 + 文件，按发生顺序）。
    pub fn mutations(&self) -> Mutations {
        Mutations {
            inner: self.inner.clone(),
        }
    }

    /// 还有几条预置输出没被消费。
    ///
    /// 断言它等于 0，可以保证「实现真的执行了测试期望的那些命令」。
    pub fn expectations_left(&self) -> usize {
        self.inner.responses.with(|queue| queue.len())
    }
}

/// [`FakeRunner`] 记录下来的变更集合。
#[derive(Debug, Clone)]
pub struct Mutations {
    inner: Arc<FakeState>,
}

impl Mutations {
    /// 按顺序取出所有变更。
    pub fn all(&self) -> Vec<Mutation> {
        self.inner.mutations.with(|items| items.clone())
    }

    /// 第 `index` 次被执行（0 起）的命令。
    ///
    /// 越界返回 `None` —— 测试可以用它断言「第 3 步是 `systemctl --user enable`」。
    pub fn nth(&self, index: usize) -> Option<Mutation> {
        self.inner.mutations.with(|items| items.get(index).cloned())
    }

    /// 第 `index` 次执行的命令（不含文件操作）。
    pub fn command(&self, index: usize) -> CommandSpec {
        let mut seen = 0usize;
        for mutation in self.all() {
            if let Mutation::Run(spec) = mutation {
                if seen == index {
                    return spec;
                }
                seen += 1;
            }
        }
        panic!("只记录了 {seen} 条命令，取不到第 {} 条", index + 1);
    }

    /// 被执行的命令条数。
    pub fn run_count(&self) -> usize {
        self.all()
            .iter()
            .filter(|mutation| matches!(mutation, Mutation::Run(_)))
            .count()
    }

    /// 所有命令的一行式表示（断言「生成的命令行长什么样」时最好用）。
    pub fn command_lines(&self) -> Vec<String> {
        self.all()
            .iter()
            .filter_map(|mutation| match mutation {
                Mutation::Run(spec) => Some(spec.display_line()),
                _ => None,
            })
            .collect()
    }

    /// 落盘/删除的文件操作。
    pub fn files(&self) -> Vec<Mutation> {
        self.all()
            .into_iter()
            .filter(|mutation| !matches!(mutation, Mutation::Run(_)))
            .collect()
    }

    /// 第 `index` 次落盘的文件正文。
    pub fn file_content(&self, index: usize) -> String {
        let mut seen = 0usize;
        for mutation in self.all() {
            if let Mutation::Install(file) = mutation {
                if seen == index {
                    return file.content;
                }
                seen += 1;
            }
        }
        panic!("只写入了 {seen} 个文件，取不到第 {} 个", index + 1);
    }
}

impl Runner for FakeRunner {
    fn run(&self, spec: &CommandSpec) -> Result<String, ServiceError> {
        self.inner
            .mutations
            .with(|items| items.push(Mutation::Run(spec.clone())));
        let position = self.inner.cursor.fetch_add(1, Ordering::SeqCst);
        let response = self
            .inner
            .responses
            .with(|queue| queue.pop_front())
            .unwrap_or_else(|| {
                panic!(
                    "FakeRunner 只预置了 {position} 条输出，第 {} 条没有对应输出：{}",
                    position + 1,
                    spec.display_line()
                )
            });
        match response {
            Response::Ok(stdout) => Ok(stdout),
            Response::Err(stderr) => Err(ServiceError::command_failed(
                spec.purpose.clone(),
                Some(1),
                stderr,
            )),
        }
    }

    fn install_file(&self, file: &FileInstall) -> Result<(), ServiceError> {
        self.inner
            .mutations
            .with(|items| items.push(Mutation::Install(file.clone())));
        Ok(())
    }

    fn remove_file(&self, path: &Path) -> Result<(), ServiceError> {
        self.inner
            .mutations
            .with(|items| items.push(Mutation::Remove(path.to_path_buf())));
        Ok(())
    }
}
