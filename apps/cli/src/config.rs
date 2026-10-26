//! 平台配置文件的位置与读写
//!
//! 配置的**内容**（键表、类型、校验、归一化）在 `mcsup::service`；
//! 本模块只管「文件在哪、怎么原子写入、加载失败怎么办」。入口各命令
//! 一律经 [`ConfigFile`] 取配置，禁止各自拼路径。

use anyhow::{Context, Result};
use mcsup::service::Config;
use std::io::Write;
use std::path::{Path, PathBuf};

/// 平台配置文件的句柄
#[derive(Debug, Clone)]
pub(crate) struct ConfigFile {
    path: PathBuf,
}

impl ConfigFile {
    /// 定位配置文件：`--config` 覆盖，否则 `{data_dir}/config.toml`
    ///
    /// `data_dir` 的缺省由 `service` 决定，与库侧组合根同一口径。
    pub(crate) fn locate(explicit: Option<&Path>) -> Self {
        let path = match explicit {
            Some(path) => path.to_path_buf(),
            None => default_config_path(),
        };
        Self { path }
    }

    /// 配置文件路径
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// 文件是否存在
    pub(crate) fn exists(&self) -> bool {
        self.path.is_file()
    }

    /// 读取并校验配置；文件缺失时为平台缺省
    ///
    /// # Errors
    ///
    /// 读取失败、TOML 非法、未知键或取值非法时返回带文件路径的错误
    pub(crate) fn load(&self) -> Result<Config> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(err) => return Err(err).context(format!("读取配置 {}", self.path.display())),
        };
        Config::parse_document(&text)
            .map_err(|reason| anyhow::anyhow!("解析配置 {}: {reason}", self.path.display()))
    }

    /// 原子写入：临时文件 + fsync + rename
    ///
    /// 同目录临时文件保证 rename 不跨卷；写入失败时旧文件原样保留。
    ///
    /// # Errors
    ///
    /// 目录创建、序列化、写入或改名失败时返回错误
    pub(crate) fn save(&self, cfg: &Config) -> Result<()> {
        let text = cfg
            .to_document()
            .map_err(|err| anyhow::anyhow!("序列化配置失败: {err}"))?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建配置目录 {}", parent.display()))?;
        }
        let tmp = self.path.with_extension("toml.tmp");
        {
            let mut file = std::fs::File::create(&tmp)
                .with_context(|| format!("创建临时配置 {}", tmp.display()))?;
            file.write_all(text.as_bytes())
                .with_context(|| format!("写入临时配置 {}", tmp.display()))?;
            file.sync_all()
                .with_context(|| format!("同步临时配置 {}", tmp.display()))?;
        }
        std::fs::rename(&tmp, &self.path).with_context(|| {
            format!(
                "替换配置 {}（临时文件 {}）",
                self.path.display(),
                tmp.display()
            )
        })
    }
}

/// 缺省配置文件：生效数据目录下的 `config.toml`
pub(crate) fn default_config_path() -> PathBuf {
    platform_data_dir().join("config.toml")
}

/// 生效的数据目录
///
/// 平台数据目录只由 [`Config::default_data_dir`] 决定（取不到系统数据目录时
/// 退到当前目录，不返回 `Result`）；做成函数是为了让「数据目录」只有一个
/// 取用点，将来支持覆盖时改一处即可。
pub(crate) fn platform_data_dir() -> PathBuf {
    Config::default_data_dir()
}

/// 元数据仓库根目录 `{data_dir}/db`
pub(crate) fn database_path() -> PathBuf {
    platform_data_dir().join("db")
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcsup::service::ConfigKey;

    fn temp_file() -> (tempfile::TempDir, ConfigFile) {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let file = ConfigFile::locate(Some(&dir.path().join("config.toml")));
        (dir, file)
    }

    /// 文件缺失 = 平台缺省，不是错误
    #[test]
    fn missing_file_loads_defaults() {
        let (_dir, file) = temp_file();
        let cfg = file.load().expect("load");
        assert_eq!(cfg, Config::default());
        assert!(!file.exists());
    }

    /// 写入后可读回；内容带说明头
    #[test]
    fn save_then_load_roundtrips() {
        let (_dir, file) = temp_file();
        let mut cfg = Config::default();
        cfg.set(ConfigKey::DataDir, "sub/data").expect("set");
        file.save(&cfg).expect("save");
        assert!(file.exists());
        let again = file.load().expect("reload");
        assert_eq!(again, cfg);
        let text = std::fs::read_to_string(file.path()).expect("read");
        assert!(text.starts_with("# mcsup"), "{text}");
    }

    /// 手工写坏的文件报错并带上路径，而不是静默回落缺省
    #[test]
    fn broken_file_reports_path() {
        let (_dir, file) = temp_file();
        std::fs::write(file.path(), "nope = 1\n").expect("write");
        let err = file.load().expect_err("broken");
        let text = format!("{err:#}");
        assert!(text.contains("config.toml"), "{text}");
    }

    /// 原子写入不留临时文件
    #[test]
    fn save_leaves_no_temp_file() {
        let (dir, file) = temp_file();
        file.save(&Config::default()).expect("save");
        let tmp = file.path().with_extension("toml.tmp");
        assert!(!tmp.exists(), "临时文件应已 rename");
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(entries.len(), 1, "{entries:?}");
    }
}
