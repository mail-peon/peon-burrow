//! 配置：**文件 / 环境变量 / CLI 三层合并** + 组合规则校验。
//!
//! ```text
//! CLI 参数  >  环境变量  >  配置文件  >  内置默认值（peon-burrow-core）
//! ```
//!
//! ⚠️ 默认值不在这里（布局铁律 L3）：结构里每个字段都是 `Option`，缺省就落到
//! `RelayOptions::default()`。**改默认值只改 core 一处。**
//!
//! 环境变量名保持 TS 版的 `PORT` / `RELAY_TOKEN` 等（旧脚本与旧文档继续有效）。

mod merge;
mod model;
mod sources;
mod validation;

pub(crate) use model::FileConfig;
pub use model::{
    CliOverrides, Config, ControlSection, LogLevel, ServiceSection, UpdateChannel, UpdateSection,
};
pub use sources::EnvSource;

use crate::exit::AppError;
use crate::paths::Paths;
use sources::parse_file;
use std::path::PathBuf;

impl Config {
    /// 三层合并。
    pub fn load(paths: &Paths, env: &EnvSource, cli: &CliOverrides) -> Result<Self, AppError> {
        let mut warnings = Vec::new();

        let path = cli
            .config
            .clone()
            .or_else(|| env.get("RELAY_CONFIG").map(PathBuf::from))
            .unwrap_or_else(|| paths.config_file().to_path_buf());

        let file = if path.exists() {
            parse_file(&path, &mut warnings)?
        } else {
            FileConfig::default()
        };

        let config = Self::merge(file, env, cli, warnings)?;
        config.validate()?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests;
/// 给 crate 内其它模块的测试用（`doctor` 的测试要造配置）。
#[cfg(test)]
pub(crate) mod test_support {
    use super::FileConfig;

    /// 从 TOML 文本构造原始配置。
    pub(crate) fn file_config(text: &str) -> FileConfig {
        let table: toml::Table = toml::from_str(text).expect("测试里的 TOML 应当合法");
        table.try_into().expect("测试里的字段应当合法")
    }
}
