use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 平台配置：缺省用系统配置目录，`--config` 可覆盖路径
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Config {
    #[serde(skip)]
    pub(crate) path: PathBuf,
}

impl Config {
    /// 加载配置；文件缺失时返回默认值
    ///
    /// # Errors
    ///
    /// 读取或解析失败时返回错误
    pub(crate) fn load(explicit: Option<&Path>) -> Result<Self> {
        let path = match explicit {
            Some(path) => path.to_path_buf(),
            None => default_config_path()?,
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self { path });
            }
            Err(err) => return Err(err).context(format!("读取配置 {}", path.display())),
        };
        let mut cfg: Self =
            toml::from_str(&text).context(format!("解析配置 {}", path.display()))?;
        cfg.path = path;
        Ok(cfg)
    }
}

pub(crate) fn default_config_path() -> Result<PathBuf> {
    Ok(platform_data_dir()?.join("config.toml"))
}

/// 平台数据目录 `{data_dir}/mcsup`，惰性创建
pub(crate) fn platform_data_dir() -> Result<PathBuf> {
    let base = dirs::data_dir().ok_or_else(|| anyhow!("无法确定系统数据目录"))?;
    Ok(base.join("mcsup"))
}

/// 元数据库路径 `{data_dir}/mcsup/db/mcsup.db`
pub(crate) fn database_path() -> Result<PathBuf> {
    Ok(platform_data_dir()?.join("db").join("mcsup.db"))
}
