//! 第 5 道工序：**替换正在运行的自己**。
//!
//! Windows **不允许删除**正在运行的可执行文件，但**允许重命名**它。所以流程是：
//!
//! ```text
//! 1) burrow.exe            → burrow.exe.old-<pid>   （rename，运行中的也允许）
//! 2) 新二进制               → burrow.exe             （copy，权限位一起带过去）
//! 3) 下次启动时顺手删掉 .old-*（尽力而为，删不掉就留着）
//! ```
//!
//! 两条必须记住的事实（`ai-docs/design/update-flow.md § 5`、`adr-0005 § 4`）：
//!
//! 1. [`crate::apply`] 返回成功后，**内存里跑的仍然是旧代码** —— 调用方必须立刻退出，
//!    否则用户以为更新了其实没有；
//! 2. 安装目录里任何**被加载的 DLL / 资源**都会阻止这一步重命名 ——
//!    被更新的程序应当是**单一自包含 exe**（这也是我们不用 OpenSSL 的原因之一）。
//!
//! 本模块**自己实现**（不依赖 `self-replace`）：逻辑就这三十行，而且失败时的回滚语义
//! 必须由我们自己说清楚 —— 任何一步失败都要把原文件放回去。

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{IoContext, UpdateError};

/// 被替换下来的旧文件的后缀标记：`<name>.old-<pid>`。
pub(crate) const OLD_SUFFIX: &str = ".old-";

/// 把 `target` 换成 `new_binary` 的内容，返回被重命名下来的旧文件路径（备份）。
///
/// 「原子-ish」的含义：
///
/// - 先 `rename` 旧文件 → 目标路径在**任何时刻都至少有一个可用的文件**（要么旧的、要么新的）；
/// - 复制新文件失败 → **立刻回滚**（把备份 rename 回去），原二进制继续可用；
/// - 回滚也失败 → 返回 [`UpdateError::RollbackFailed`]，里面同时带上两个原因和备份路径，
///   绝不静默。
///
/// 备份**不会被本函数删除**：运行中的进程自己就是那个备份（Windows 删不掉），
/// 留给 [`cleanup_stale_backups`] 在下次启动时顺手清。
///
/// # Errors
///
/// [`UpdateError::Io`]（rename 失败：文件锁 / 权限不足）、[`UpdateError::Replace`]（复制失败，已回滚）、
/// [`UpdateError::RollbackFailed`]（复制失败且回滚失败）。
pub fn replace_binary(target: &Path, new_binary: &Path) -> Result<PathBuf, UpdateError> {
    let file_name = target
        .file_name()
        .ok_or_else(|| UpdateError::CannotDetermineBinary {
            reason: format!("{} 不是一个文件路径", target.display()),
        })?
        .to_string_lossy()
        .into_owned();

    let backup = target.with_file_name(format!("{file_name}{OLD_SUFFIX}{}", std::process::id()));

    // 1) 把旧文件重命名到一边（Windows 上允许对正在运行的 exe 做这件事）
    if target.exists() {
        if backup.exists() {
            // 上一次留下的同名备份：先清掉，否则 rename 会失败
            fs::remove_file(&backup).with_path(&backup)?;
        }
        fs::rename(target, &backup).with_path(target)?;
    }

    // 2) 把新二进制复制到目标位置（`fs::copy` 会一起带上权限位，Unix 上的可执行位靠它）
    if let Err(source) = fs::copy(new_binary, target) {
        // 3) 失败了就把旧的放回去 —— 这一步是「永远保留原二进制」的兑现
        return match restore(&backup, target) {
            Ok(()) => Err(UpdateError::Replace {
                target: target.to_path_buf(),
                source,
            }),
            Err(rollback) => Err(UpdateError::RollbackFailed {
                target: target.to_path_buf(),
                backup,
                replace_error: source.to_string(),
                source: rollback,
            }),
        };
    }

    Ok(backup)
}

/// 把备份放回原位（回滚）。备份不存在（原本就没有旧文件）时视作成功。
fn restore(backup: &Path, target: &Path) -> std::io::Result<()> {
    if !backup.exists() {
        return Ok(());
    }
    // Windows 上 `fs::rename` 用 MOVEFILE_REPLACE_EXISTING，可以直接盖掉半成品；
    // 万一不行（跨卷 / 被占用），退化成「复制 + 删备份」。
    match fs::rename(backup, target) {
        Ok(()) => Ok(()),
        Err(_) => {
            fs::copy(backup, target)?;
            fs::remove_file(backup)
        }
    }
}

/// 顺手清理 `dir` 里遗留的 `*.old-*`（**尽力而为**：删不掉就跳过，不报错）。
///
/// 返回成功删掉的个数。典型场景：更新完重启后，上一次运行的那个 exe 终于不再被占用，
/// 这时就能删掉；**失败是正常的**（那个文件可能正是当前正在运行的自己），所以只记 `debug`。
pub fn cleanup_stale_backups(dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };

    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.contains(OLD_SUFFIX) {
            continue;
        }
        match fs::remove_file(entry.path()) {
            Ok(()) => {
                removed += 1;
                tracing::debug!(event = "update.cleanup", file = %name, "清掉遗留的旧二进制");
            }
            Err(error) => {
                // 正在运行的那个删不掉是预期内的，不升级成 warn（否则每次启动都刷一行）
                tracing::debug!(event = "update.cleanup", file = %name, %error, "旧二进制删不掉（多半正在运行）");
            }
        }
    }
    removed
}

/// Unix：保证 staging 里的文件有可执行位（Windows 不需要，扩展名决定）。
///
/// 冒烟测试要真的**跑**这个文件，所以位必须在跑之前设好。
#[cfg(unix)]
pub(crate) fn ensure_executable(path: &Path) -> Result<(), UpdateError> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path).with_path(path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).with_path(path)
}

/// Windows / 其它平台：什么都不用做。
#[cfg(not(unix))]
pub(crate) fn ensure_executable(_path: &Path) -> Result<(), UpdateError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_swaps_content_and_keeps_the_old_file_as_a_backup() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("burrow.exe");
        let fresh = dir.path().join("burrow-new.exe");
        fs::write(&target, b"old code").unwrap();
        fs::write(&fresh, b"new code").unwrap();

        let backup = replace_binary(&target, &fresh).expect("替换应当成功");

        // 目标位置是**新**内容
        assert_eq!(fs::read(&target).unwrap(), b"new code");
        // 旧内容还在备份里（调用方/清理逻辑之后才处理它）
        assert_eq!(fs::read(&backup).unwrap(), b"old code");
        assert!(backup.exists());
        let backup_name = backup.file_name().unwrap().to_string_lossy().into_owned();
        assert!(backup_name.starts_with("burrow.exe.old-"), "{backup_name}");
        assert_eq!(backup.parent().unwrap(), dir.path());
    }

    #[test]
    fn replace_restores_the_original_when_the_new_binary_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("burrow.exe");
        let missing = dir.path().join("nope.exe");
        fs::write(&target, b"old code").unwrap();

        let err = replace_binary(&target, &missing).expect_err("复制失败必须报错");

        // 回滚路径：原文件原封不动，且没有留下垃圾备份
        assert!(matches!(err, UpdateError::Replace { .. }), "{err:?}");
        assert_eq!(fs::read(&target).unwrap(), b"old code");
        assert_eq!(cleanup_stale_backups(dir.path()), 0);
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(OLD_SUFFIX))
            .collect();
        assert!(leftovers.is_empty(), "回滚后不该留下 {leftovers:?}");
    }

    #[test]
    fn replace_creates_the_target_when_it_did_not_exist_yet() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("burrow.exe");
        let fresh = dir.path().join("burrow-new.exe");
        fs::write(&fresh, b"new code").unwrap();

        let backup = replace_binary(&target, &fresh).unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"new code");
        assert!(!backup.exists(), "没有旧文件时不该造一个备份出来");
    }

    #[test]
    fn cleanup_removes_stale_backups_only() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("burrow.exe"), b"live").unwrap();
        fs::write(dir.path().join("burrow.exe.old-1234"), b"dead").unwrap();
        fs::write(dir.path().join("burrow.exe.old-5678"), b"dead").unwrap();

        assert_eq!(cleanup_stale_backups(dir.path()), 2);
        assert!(dir.path().join("burrow.exe").exists(), "活着的文件不能动");
        assert_eq!(cleanup_stale_backups(dir.path()), 0);
    }

    #[test]
    fn cleanup_on_a_missing_dir_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(cleanup_stale_backups(&dir.path().join("nope")), 0);
    }
}
