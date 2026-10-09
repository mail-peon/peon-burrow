//! 第 4 道闸：**冒烟测试**。
//!
//! ```text
//! <staging>/<asset> version --json   →   {"version":"0.2.0"}
//! ```
//!
//! 输出里的版本必须**等于清单里的版本**，否则拒绝替换。
//!
//! 为什么值得多花这几百毫秒（`ai-docs/design/update-flow.md § 4 ④`）：
//! Windows 上「文件替换成功但新二进制起不来」是最难排查的一类故障 —— 而且此时
//! **旧版本已经被挤掉了**。先跑一次 `version` 能把「架构不对 / 缺 DLL / 被截断 /
//! 根本不是我们的程序」挡在替换之前。
//!
//! 实现细节：`std::process::Command`（同步）跑在 `spawn_blocking` 里，避免阻塞异步运行时；
//! stdout/stderr 重定向到**文件**（不是管道）以免子进程写满管道缓冲区时死锁；
//! 超时后 `kill`，所以它不会把一次更新挂死。

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use semver::Version;
use serde::Deserialize;

use crate::error::{IoContext, UpdateError};

/// 冒烟测试的超时：超过就当它不可用（**拒绝替换**）。
pub const SMOKE_TEST_TIMEOUT: Duration = Duration::from_secs(10);

/// 轮询子进程的间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// 最多读多少输出（防止一个疯掉的二进制把日志刷爆）。
const OUTPUT_LIMIT: u64 = 8 * 1024;

/// 错误文案里最多带多少字符的输出。
const SNIPPET_CHARS: usize = 400;

/// 跑冒烟测试：`binary version --json` 必须成功、且自报版本等于 `expected`。
///
/// # Errors
///
/// - [`UpdateError::SmokeTestUnrunnable`]：起不来（不是可执行文件 / 权限不足 / 任务 panic）；
/// - [`UpdateError::SmokeTestFailed`]：非零退出；
/// - [`UpdateError::SmokeTestTimeout`]：超时被杀；
/// - [`UpdateError::SmokeTestVersionMismatch`]：跑起来了，但不是这个版本；
/// - [`UpdateError::Io`]：临时输出文件写不出来。
pub(crate) async fn smoke_test(binary: &Path, expected: &Version) -> Result<(), UpdateError> {
    let binary = binary.to_path_buf();
    let expected = expected.clone();
    let fail_path = binary.clone();

    tokio::task::spawn_blocking(move || run(&binary, &expected))
        .await
        .map_err(|join| UpdateError::SmokeTestUnrunnable {
            path: fail_path,
            reason: format!("冒烟测试任务异常结束：{join}"),
        })?
}

/// 同步实现（跑在 blocking 线程里）。
fn run(binary: &Path, expected: &Version) -> Result<(), UpdateError> {
    let path = binary.to_path_buf();
    let stem = binary.file_name().map_or_else(
        || "staged".to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let dir = binary
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let stdout_path = dir.join(format!("{stem}.smoke.stdout"));
    let stderr_path = dir.join(format!("{stem}.smoke.stderr"));

    let stdout = File::create(&stdout_path).with_path(&stdout_path)?;
    let stderr = File::create(&stderr_path).with_path(&stderr_path)?;

    let mut child = Command::new(binary)
        .arg("version")
        .arg("--json")
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .map_err(|error| UpdateError::SmokeTestUnrunnable {
            path: path.clone(),
            reason: error.to_string(),
        })?;

    let deadline = Instant::now() + SMOKE_TEST_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(UpdateError::SmokeTestTimeout {
                        path,
                        seconds: SMOKE_TEST_TIMEOUT.as_secs(),
                    });
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(error) => {
                let _ = child.kill();
                return Err(UpdateError::SmokeTestUnrunnable {
                    path,
                    reason: error.to_string(),
                });
            }
        }
    };

    let stdout_text = read_snippet(&stdout_path);
    let stderr_text = read_snippet(&stderr_path);

    if !status.success() {
        return Err(UpdateError::SmokeTestFailed {
            path,
            code: status.code().unwrap_or(-1),
            output: snippet(&format!("stdout: {stdout_text} | stderr: {stderr_text}")),
        });
    }

    let actual = parse_version(&stdout_text);
    let matches = actual
        .as_deref()
        .and_then(|value| Version::parse(value).ok())
        .is_some_and(|version| version == *expected);

    if !matches {
        return Err(UpdateError::SmokeTestVersionMismatch {
            expected: expected.to_string(),
            actual: actual.unwrap_or_else(|| "<输出里没有 version 字段>".to_string()),
            output: snippet(&stdout_text),
        });
    }

    tracing::debug!(event = "update.smoke", path = %binary.display(), version = %expected, "冒烟测试通过");
    Ok(())
}

/// 从 `<exe> version --json` 的 stdout 里取版本号。
///
/// 契约是 `{"version":"0.2.0"}`。为了容忍「运行时先打了几行日志」，这里取**最后一行**
/// 能解析出 `version` 字段的 JSON；版本号允许带 `v` 前缀（`v0.2.0`）。
fn parse_version(stdout: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct VersionOutput {
        version: String,
    }

    stdout.lines().rev().find_map(|line| {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        serde_json::from_str::<VersionOutput>(line)
            .ok()
            .map(|parsed| parsed.version.trim().trim_start_matches('v').to_string())
    })
}

/// 读最多 [`OUTPUT_LIMIT`] 字节的输出（文件不存在 / 读不动就当作空）。
fn read_snippet(path: &Path) -> String {
    let mut bytes = Vec::new();
    if let Ok(file) = File::open(path) {
        let _ = file.take(OUTPUT_LIMIT).read_to_end(&mut bytes);
    }
    snippet(&String::from_utf8_lossy(&bytes))
}

/// 错误文案里带的输出片段（截断过，避免日志被刷爆）。
fn snippet(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= SNIPPET_CHARS {
        return trimmed.to_string();
    }
    let cut: String = trimmed.chars().take(SNIPPET_CHARS).collect();
    format!("{cut}…（已截断）")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_contract_and_tolerates_extra_lines() {
        assert_eq!(
            parse_version(r#"{"version":"0.2.0"}"#).as_deref(),
            Some("0.2.0")
        );
        assert_eq!(
            parse_version("INFO 启动中\n{\"version\":\"v0.2.0\"}\n").as_deref(),
            Some("0.2.0")
        );
        assert_eq!(parse_version("not json").as_deref(), None);
        assert_eq!(parse_version(r#"{"other":1}"#).as_deref(), None);
    }

    #[test]
    fn snippets_are_truncated_on_char_boundaries() {
        let short = snippet("  好  ");
        assert_eq!(short, "好");
        let long = snippet(&"错".repeat(SNIPPET_CHARS + 10));
        assert!(long.ends_with("…（已截断）"));
        assert_eq!(long.chars().count(), SNIPPET_CHARS + 6);
    }

    #[tokio::test]
    async fn a_missing_binary_is_unrunnable_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let err = smoke_test(&dir.path().join("nope"), &Version::new(0, 2, 0))
            .await
            .expect_err("不存在的文件跑不起来");
        assert!(
            matches!(err, UpdateError::SmokeTestUnrunnable { .. }),
            "{err:?}"
        );
    }
}
