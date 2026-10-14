use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 平台配置： 缺省用系统配置目录，--config 可覆盖路径
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Config {
    /// 服务根目录，缺省当前目录
    pub(crate) install_dir: Option<PathBuf>,
    #[serde(skip)]
    pub(crate) path: PathBuf,
}

impl Config {
    /// 加载配置；文件缺失时返回默认值
    ///
    /// # Errors
    ///
    /// 找不到系统配置目录、读出单无法解析、成功读入但内容非法时返回错误
    pub(crate) fn load(explicit: Option<&Path>) -> Result<Self> {
        let path = match explicit {
            Some(path) => path.to_path_buf(),
            None => default_path()?,
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    path,
                    ..Self::default()
                });
            }
            Err(err) => return Err(err).context(format!("读取配置 {}", path.display())),
        };
        let mut cfg: Self =
            toml::from_str(&text).context(format!("解析配置 {}", path.display()))?;
        cfg.path = path;
        Ok(cfg)
    }
}

pub(crate) fn default_path() -> Result<PathBuf> {
    let base = dirs::config_dir().ok_or_else(|| anyhow!("无法确定系统配置目录"))?;
    Ok(base.join("mcsup").join("confiig.toml"))
}
