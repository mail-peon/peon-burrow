//! 装日志：stderr（前台）+ 落盘（服务态），带**按大小滚动**。
//!
//! 为什么自己写滚动而不用 `tracing_appender::rolling`：它只能按**时间**滚
//! （`Rotation::DAILY` 等），而配置里承诺的是「单文件 `log_max_mib`、保留 `log_keep_files` 个」
//! （`ai-docs/design/logging-and-diagnostics.md § 5`）。`ai-docs/02-tech-stack.md § 5`
//! 也把「自己写一个 rolling 封装」列为预期方案。
//!
//! ⚠️ 从前这两个配置项**只被解析**（`config.rs` 有默认值 10 / 5），日志却是不滚动的单文件：
//! 用户以为配了上限，实际会一直涨。

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tracing_subscriber::Layer as _;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::LogLevel;

/// 临时打开明文日志的全局开关（`burrow trace` 会翻它）。
static VERBOSE: std::sync::OnceLock<Arc<AtomicBool>> = std::sync::OnceLock::new();

/// 取（必要时创建）这个开关。
pub fn verbose_flag() -> Arc<AtomicBool> {
    Arc::clone(VERBOSE.get_or_init(|| Arc::new(AtomicBool::new(false))))
}

/// 服务态日志的落盘目标。
#[derive(Debug, Clone, Copy)]
pub struct LogFile<'a> {
    /// 日志文件路径。
    pub path: &'a Path,
    /// 单个文件上限（MiB；`0` = 不按大小滚）。
    pub max_mib: u64,
    /// 保留几个文件（含当前那个；`0` 与 `1` 都当 1）。
    pub keep: u32,
}

/// 装日志。
///
/// trace 的开关是**运行期可翻**的（`burrow trace` 会翻它），所以过滤器读一个原子量，
/// 而不是在启动时把级别写死。
///
/// `file` 给了就**同时**往文件写一份（服务态：没有终端，stderr 等于扔掉）。
pub fn init_logging(level: LogLevel, trace: bool, file: Option<LogFile<'_>>) {
    let verbose = verbose_flag();
    verbose.store(trace, Ordering::Relaxed);

    let max = match level {
        LogLevel::Error => tracing::Level::ERROR,
        LogLevel::Warn => tracing::Level::WARN,
        LogLevel::Info => tracing::Level::INFO,
        LogLevel::Debug => tracing::Level::DEBUG,
        LogLevel::Trace => tracing::Level::TRACE,
    };

    let filter = tracing_subscriber::filter::filter_fn(move |metadata| {
        *metadata.level() <= max || verbose.load(Ordering::Relaxed)
    });

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(filter.clone());

    let Some(target) = file else {
        // 已经初始化过（测试里会多次调用）就忽略
        let _ = tracing_subscriber::registry().with(stderr_layer).try_init();
        return;
    };

    // 落盘这份建不起来**不该**让中继起不来（`logging-and-diagnostics.md § 5` 验收 #4）：
    // 降级成「只写 stderr」并记一条 error —— stderr 那份先装好，这条 error 才有人看得见。
    let parent = target
        .path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    let Some(parent) = parent else {
        let _ = tracing_subscriber::registry().with(stderr_layer).try_init();
        tracing::error!(
            path = %target.path.display(),
            "日志文件没有父目录，落盘那份用不了：降级为只写 stderr，中继继续跑"
        );
        return;
    };
    if let Err(error) = std::fs::create_dir_all(parent) {
        let _ = tracing_subscriber::registry().with(stderr_layer).try_init();
        tracing::error!(
            path = %parent.display(),
            %error,
            "建不了日志目录：降级为只写 stderr，中继继续跑"
        );
        return;
    }

    let writer = match RotatingWriter::open(target) {
        Ok(writer) => writer,
        Err(error) => {
            let _ = tracing_subscriber::registry().with(stderr_layer).try_init();
            tracing::error!(
                path = %target.path.display(),
                %error,
                "日志文件打不开：降级为只写 stderr，中继继续跑"
            );
            return;
        }
    };

    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(writer)
        .with_filter(filter);

    // 已经初始化过（测试里会多次调用）就忽略
    let _ = tracing_subscriber::registry()
        .with(stderr_layer)
        .with(file_layer)
        .try_init();
}

/// 按大小滚动的日志写入器。
///
/// 滚动规则（与 `logging-and-diagnostics.md § 5` 对齐）：
/// 当前文件超过 `max_mib` 时改名为 `<名字>.1`，原来的 `.1` → `.2` …，
/// 超过 `keep` 个的删掉。只有 `keep` 个文件，磁盘占用上界 = `max_mib × keep`。
#[derive(Debug)]
struct RotatingWriter {
    path: PathBuf,
    max_bytes: u64,
    keep: usize,
    // C5-EXCEPTION: 这是**同步**的日志写入路径（`std::io::Write`），整条链上没有 `await`，
    // 锁只在一次 `write` 内持有；跨线程并发也只在这把锁上排队（写日志本来就该串行）。
    // 写全 `std::sync::Mutex` 是为了让 `ci.yaml` 的 C5 守卫**看得见**它（守卫按字面量匹配）。
    state: std::sync::Mutex<State>,
}

#[derive(Debug)]
struct State {
    /// 当前文件的句柄；滚动时先 `take()` 掉（Windows 上打开着的文件不能改名）。
    file: Option<std::fs::File>,
    size: u64,
}

impl RotatingWriter {
    fn open(target: LogFile<'_>) -> std::io::Result<Self> {
        let file = open_append(target.path)?;
        let size = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        Ok(Self {
            path: target.path.to_path_buf(),
            max_bytes: target.max_mib.saturating_mul(1024 * 1024),
            // 至少留当前这一个：keep = 0 的人想要的是「不要历史」，不是「不要日志」
            keep: usize::try_from(target.keep.max(1)).unwrap_or(usize::MAX),
            // C5-EXCEPTION: 同上面 `state` 字段的说明 —— 同步写入路径，锁只在一次 `write` 内持有
            state: std::sync::Mutex::new(State {
                file: Some(file),
                size,
            }),
        })
    }

    /// 滚动一次：`<path>` → `<path>.1` → `<path>.2` …
    fn rotate(&self, state: &mut State) -> std::io::Result<()> {
        // ① 先放掉句柄（Windows 上「正在使用的文件」不能改名）
        let _ = state.file.take();

        // ② 从最老的开始往后挪：序号最大的那个（`keep - 1`）先删掉 —— 它已经被挤出保留窗口，
        //    再往后挪会凭空多出一个 `.3`，磁盘占用就没有上界了（测试盯的就是这一条）
        if self.keep > 1 {
            let oldest = self.rotated(self.keep - 1);
            if oldest.exists() {
                let _ = std::fs::remove_file(&oldest);
            }
            for index in (1..self.keep - 1).rev() {
                let from = self.rotated(index);
                let to = self.rotated(index + 1);
                if from.exists() {
                    let _ = std::fs::rename(&from, &to);
                }
            }
            if self.path.exists() {
                let _ = std::fs::rename(&self.path, self.rotated(1));
            }
        } else if self.path.exists() {
            // keep=1：只要当前文件，旧内容直接丢掉（等价于截断）
            let _ = std::fs::remove_file(&self.path);
        }

        // ③ 重新打开当前文件
        state.file = Some(open_append(&self.path)?);
        state.size = 0;
        Ok(())
    }

    /// 第 `index` 个历史文件（`<path>.1` 是上一个）。
    fn rotated(&self, index: usize) -> PathBuf {
        let mut name = self.path.clone().into_os_string();
        name.push(format!(".{index}"));
        PathBuf::from(name)
    }

    /// 写入并在必要时先滚动（`&self`：内部那把锁保证一处一写）。
    fn write_rotating(&self, buf: &[u8]) -> std::io::Result<usize> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        // ① 超上限就滚。`size > 0` 这个条件保证「单条日志比上限还大」时不会死循环。
        if self.max_bytes > 0
            && state.size > 0
            && state.size.saturating_add(buf.len() as u64) > self.max_bytes
        {
            self.rotate(&mut state)?;
        }

        // ② 写 + flush：服务态崩溃时最后几行不能丢（日志是唯一证据）
        let file = state
            .file
            .as_mut()
            .ok_or_else(|| std::io::Error::other("日志文件句柄缺失（滚动时被拿走了）"))?;
        let written = file.write(buf)?;
        file.flush()?;
        state.size = state.size.saturating_add(written as u64);
        Ok(written)
    }
}

/// 打开（必要时创建）日志文件并定位到末尾。
fn open_append(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

impl std::io::Write for RotatingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.write_rotating(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// 一次写入用的句柄：每行日志拿一次锁，写完就放。
#[derive(Debug)]
struct RotatingHandle<'a> {
    target: &'a RotatingWriter,
}

impl std::io::Write for RotatingHandle<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.target.write_rotating(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for RotatingWriter {
    type Writer = RotatingHandle<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        RotatingHandle { target: self }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(writer: &RotatingWriter, text: &str) {
        writer.write_rotating(text.as_bytes()).expect("write");
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    #[test]
    fn rotates_when_the_file_reaches_the_limit() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("peon-burrow.log");
        let writer = RotatingWriter::open(LogFile {
            path: &path,
            max_mib: 0, // 用字节上限测：下面手动改成 100 字节
            keep: 3,
        })
        .expect("open");
        // 直接构造一个「100 字节上限」的写入器（MiB 粒度对测试太粗）
        let writer = RotatingWriter {
            max_bytes: 100,
            ..writer
        };

        write(&writer, &"a".repeat(60)); // 60
        write(&writer, &"b".repeat(60)); // 60 → 超过 100 → 先滚动

        assert_eq!(read(&path), "b".repeat(60), "新内容写在当前文件里");
        let first = writer.rotated(1);
        assert_eq!(read(&first), "a".repeat(60), "旧内容滚到 .1");

        write(&writer, &"c".repeat(60)); // 再滚一次
        assert_eq!(read(&path), "c".repeat(60));
        assert_eq!(read(&writer.rotated(1)), "b".repeat(60));
        assert_eq!(read(&writer.rotated(2)), "a".repeat(60), "keep=3 → 留三个");

        write(&writer, &"d".repeat(60)); // 第三次滚动：最老的 a 被丢掉
        assert_eq!(read(&writer.rotated(2)), "b".repeat(60));
        assert!(
            !writer.rotated(3).exists(),
            "keep=3 时不该出现第 4 个文件（磁盘占用要有上界）"
        );
    }

    #[test]
    fn a_single_write_larger_than_the_limit_does_not_loop() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("peon-burrow.log");
        let writer = RotatingWriter {
            max_bytes: 10,
            ..RotatingWriter::open(LogFile {
                path: &path,
                max_mib: 0,
                keep: 3,
            })
            .expect("open")
        };

        write(&writer, &"x".repeat(500));
        assert_eq!(read(&path).len(), 500, "超大的一条照样写完，只是不滚动");
        assert!(!writer.rotated(1).exists(), "不该为了它滚出一个空文件");
    }

    #[test]
    fn keep_one_truncates_instead_of_keeping_history() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("peon-burrow.log");
        let writer = RotatingWriter {
            max_bytes: 10,
            ..RotatingWriter::open(LogFile {
                path: &path,
                max_mib: 0,
                keep: 1,
            })
            .expect("open")
        };

        write(&writer, "first-line\n");
        write(&writer, "second-line\n");
        assert_eq!(read(&path), "second-line\n");
        assert!(!writer.rotated(1).exists(), "keep=1 = 只要当前文件");
    }

    #[test]
    fn the_configured_limits_are_turned_into_bytes() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("peon-burrow.log");
        let writer = RotatingWriter::open(LogFile {
            path: &path,
            max_mib: 10,
            keep: 5,
        })
        .expect("open");
        assert_eq!(writer.max_bytes, 10 * 1024 * 1024);
        assert_eq!(writer.keep, 5);
    }
}
