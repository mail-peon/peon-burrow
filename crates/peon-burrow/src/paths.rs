//! 所有落盘路径的**唯一入口**。
//!
//! 布局铁律 L4：只有 [`Paths::discover`] 允许碰 `directories`，其它模块一律接收注入的
//! `Paths`；测试用 [`Paths::for_test`]。这条守的是「测试能并行、且不会写进用户真实的
//! `%APPDATA%`」。

use std::path::{Path, PathBuf};

use crate::exit::AppError;

/// 应用使用的全部路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    root: PathBuf,
    config_file: PathBuf,
}

impl Paths {
    /// 按平台惯例定位（**唯一**碰 `directories` 的地方）。
    ///
    /// | 平台 | 配置 | 数据 |
    /// | --- | --- | --- |
    /// | Windows | `%APPDATA%\peon-burrow\relay.toml` | `%LOCALAPPDATA%\peon-burrow\` |
    /// | macOS | `~/Library/Application Support/peon-burrow/` | 同上 |
    /// | Linux | `~/.config/peon-burrow/` | `~/.local/share/peon-burrow/` |
    pub fn discover() -> Result<Self, AppError> {
        let project = directories::ProjectDirs::from("", "", "peon-burrow").ok_or_else(|| {
            AppError::Config("无法确定用户配置目录（HOME / APPDATA 都读不到）".to_owned())
        })?;
        Ok(Self::from_roots(
            project.data_local_dir(),
            project.config_dir(),
        ))
    }

    /// 测试用：所有文件都落在一个临时目录下。
    pub fn for_test(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            config_file: root.join("relay.toml"),
            root,
        }
    }

    /// 用给定的数据目录与配置目录构造。
    pub fn from_roots(data_dir: impl Into<PathBuf>, config_dir: impl Into<PathBuf>) -> Self {
        let root = data_dir.into();
        Self {
            config_file: config_dir.into().join("relay.toml"),
            root,
        }
    }

    /// 配置文件（`relay.toml`）。
    pub fn config_file(&self) -> &Path {
        &self.config_file
    }

    /// 数据目录。
    pub fn data_dir(&self) -> &Path {
        &self.root
    }

    /// 控制面发现文件。
    pub fn control_file(&self) -> PathBuf {
        self.root.join("control.json")
    }

    /// 日志目录。
    pub fn log_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// 日志文件（服务态写这里；前台态写 stdout）。
    pub fn log_file(&self) -> PathBuf {
        self.log_dir().join("peon-burrow.log")
    }

    /// 自更新检查的节流戳（避免每次都打 GitHub）。
    pub fn update_stamp(&self) -> PathBuf {
        self.root.join("update-check.json")
    }

    /// 状态/诊断快照文件（GUI 与 `doctor` 都读它）。
    pub fn status_file(&self) -> PathBuf {
        self.root.join("status.json")
    }

    /// 确保目录存在。
    pub fn ensure_dirs(&self) -> Result<(), AppError> {
        std::fs::create_dir_all(&self.root)?;
        std::fs::create_dir_all(self.log_dir())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_paths_stay_inside_the_given_root() {
        let paths = Paths::for_test("C:\\tmp\\peon-burrow-test");
        assert!(paths.config_file().starts_with("C:\\tmp\\peon-burrow-test"));
        assert!(
            paths
                .control_file()
                .starts_with("C:\\tmp\\peon-burrow-test")
        );
        assert!(paths.log_file().starts_with("C:\\tmp\\peon-burrow-test"));
        assert!(paths.status_file().starts_with("C:\\tmp\\peon-burrow-test"));
    }

    #[test]
    fn the_config_file_is_named_relay_toml() {
        let paths = Paths::for_test("/tmp/x");
        assert_eq!(paths.config_file().file_name().unwrap(), "relay.toml");
    }

    #[test]
    fn ensure_dirs_creates_everything_it_promises() {
        let directory = tempfile::tempdir().expect("tempdir");
        let paths = Paths::for_test(directory.path().join("data"));
        paths.ensure_dirs().expect("ensure");
        assert!(paths.data_dir().is_dir());
        assert!(paths.log_dir().is_dir());
    }

    #[test]
    fn discover_does_not_panic() {
        // 真实环境里能不能定位是环境问题；这里只要求它别把异常变成 panic
        match Paths::discover() {
            Ok(paths) => assert!(paths.config_file().ends_with("relay.toml")),
            Err(error) => assert!(error.to_string().contains("配置目录")),
        }
    }
}
