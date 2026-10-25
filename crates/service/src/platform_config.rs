//! 平台配置：`config.toml` 的类型、键表、校验与读显
//!
//! 配置是机器级偏好（数据目录位置、下载与 Java 来源），不含工作区列表——
//! 那在 `db/workspaces/`，双真相源会让两边分叉。
//!
//! 键表是唯一真相来源：`config show` / `config set` / `config edit` 的键名、
//! 类型、校验与归一化全部从这里派生，CLI 只做参数搬运与渲染。新增配置项
//! 时在 [`Config`] 加字段、在 [`ConfigKey`] 加变体，两处漏一处的编译错误
//! 会把差异挡在门禁里。
//!
//! 路径类字段按 `~` 展开与归一化后保存：文件里的值是给人看的，运行时
//! 才拼成绝对路径；保存前就归一化，避免同一目录因写法不同出现两份。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// 平台配置的磁盘形态（`config.toml` 的整体结构）
///
/// 字段全部 `Option`：未设置即「用平台缺省」。`resolved_*` 系列负责
/// 把缺省填上，写入时仍保留 `None`，避免把缺省值固化进文件。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct Config {
    /// 平台数据目录（缺省取系统数据目录下的 `mcsup`）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<PathBuf>,
    /// 服务端核心下载源；缺省只用内置来源
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_mirror: Option<String>,
    /// Java 运行时下载源；缺省用全部内置来源
    #[serde(skip_serializing_if = "Option::is_none")]
    pub java_source: Option<String>,
}

/// 生效后的配置值（缺省已填）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffectiveValue {
    /// 目录路径
    Path(PathBuf),
    /// 枚举标识
    Choice(&'static str),
}

impl EffectiveValue {
    /// 展示口径：路径用平台分隔符，枚举用标识
    #[must_use]
    pub fn display(&self) -> String {
        match self {
            Self::Path(path) => path.display().to_string(),
            Self::Choice(choice) => (*choice).to_owned(),
        }
    }
}

/// 可设置的配置键
///
/// 每个变体锁死一个键名与一种类型：键表（[`ConfigKey::ALL`]）是 `show` /
/// `set` 的枚举来源，`--key` 拼错时能列出全部可用键。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigKey {
    /// `data-dir`：平台数据目录
    DataDir,
    /// `download-mirror`：服务端核心下载源
    DownloadMirror,
    /// `java-source`：Java 运行时下载源
    JavaSource,
}

impl ConfigKey {
    /// 全部配置键，顺序即 `config show` 与报错文案的稳定顺序
    pub const ALL: &'static [Self] = &[Self::DataDir, Self::DownloadMirror, Self::JavaSource];

    /// 配置文件里的键名（kebab-case，与 `server set` 同一口径）
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DataDir => "data-dir",
            Self::DownloadMirror => "download-mirror",
            Self::JavaSource => "java-source",
        }
    }

    /// 一行说明（`config show` 的人类视图）
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::DataDir => "平台数据目录（workspaces / java / db 都在其下）",
            Self::DownloadMirror => "服务端核心下载源",
            Self::JavaSource => "Java 运行时下载源",
        }
    }

    /// 按名解析；未知名返回 `None`（调用方据此报用法错误并列出 [`Self::ALL`]）
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|key| key.as_str() == name)
    }

    /// 该键的枚举取值；仅枚举型键有值
    #[must_use]
    pub fn choices(self) -> &'static [&'static str] {
        match self {
            Self::DataDir => &[],
            Self::DownloadMirror => DOWNLOAD_MIRRORS,
            Self::JavaSource => JAVA_SOURCES,
        }
    }

    /// 该键在未设置时的生效值（人类视图里标「缺省」用）
    #[must_use]
    pub fn default_display(self) -> String {
        match self {
            Self::DataDir => Config::default_data_dir().display().to_string(),
            Self::DownloadMirror => DOWNLOAD_MIRRORS.first().copied().unwrap_or("").to_owned(),
            Self::JavaSource => JAVA_SOURCE_ALL.to_owned(),
        }
    }
}

impl std::fmt::Display for ConfigKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `download-mirror` 的取值：`mojang`（内置官方源，不写文件即此值）
const DOWNLOAD_MIRRORS: &[&str] = &["mojang"];

/// `java-source` 的取值：与 provider 注册的 Java 源一一对应，缺省用全部
const JAVA_SOURCES: &[&str] = &["adoptium", "azul", "foojay"];

/// [`JAVA_SOURCE_ALL`]：`java-source` 未设置时的口径
pub const JAVA_SOURCE_ALL: &str = "all";

/// 配置值非法
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ConfigValueError {
    /// 路径为空或无法归一化
    #[error("{key} 不是可用路径: {value}")]
    Path {
        /// 键名
        key: &'static str,
        /// 原始输入
        value: String,
        /// 具体原因
        reason: String,
    },
    /// 枚举值不在取值表内
    #[error("{key} 取值非法: {value}")]
    Choice {
        /// 键名
        key: &'static str,
        /// 原始输入
        value: String,
        /// 可用取值
        choices: &'static [&'static str],
    },
}

impl ConfigValueError {
    /// 面向用户的完整说明（含可用取值）
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Path { key, value, reason } => {
                format!("{key} 不是可用路径: {value}（{reason}）")
            }
            Self::Choice {
                key,
                value,
                choices,
            } => format!("{key} 取值非法: {value}；可用: {}", choices.join(", ")),
        }
    }
}

impl Config {
    /// 平台缺省的 `data-dir`：系统数据目录下的 `mcsup`
    ///
    /// 取不到系统数据目录时退到当前目录：不阻断命令，由 `doctor` 报出来。
    #[must_use]
    pub fn default_data_dir() -> PathBuf {
        let dir = dirs::data_dir().map_or_else(
            || std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            |dir| dir.join("mcsup"),
        );
        lexical_normalize(&dir)
    }

    /// 生效的 `data-dir`
    #[must_use]
    pub fn resolved_data_dir(&self) -> PathBuf {
        self.data_dir.clone().unwrap_or_else(Self::default_data_dir)
    }

    /// 生效的 `download-mirror`
    #[must_use]
    pub fn resolved_download_mirror(&self) -> &'static str {
        self.download_mirror
            .as_deref()
            .and_then(|raw| DOWNLOAD_MIRRORS.iter().copied().find(|c| *c == raw))
            .unwrap_or(DOWNLOAD_MIRRORS[0])
    }

    /// 生效的 `java-source`；未设置为 [`JAVA_SOURCE_ALL`]
    #[must_use]
    pub fn resolved_java_source(&self) -> &'static str {
        self.java_source
            .as_deref()
            .and_then(|raw| JAVA_SOURCES.iter().copied().find(|c| *c == raw))
            .unwrap_or(JAVA_SOURCE_ALL)
    }

    /// 该键的生效值
    #[must_use]
    pub fn effective(&self, key: ConfigKey) -> EffectiveValue {
        match key {
            ConfigKey::DataDir => EffectiveValue::Path(self.resolved_data_dir()),
            ConfigKey::DownloadMirror => EffectiveValue::Choice(self.resolved_download_mirror()),
            ConfigKey::JavaSource => EffectiveValue::Choice(self.resolved_java_source()),
        }
    }

    /// 该键是否在文件中显式设置（`show` 用它区分缺省与显式值）
    #[must_use]
    pub fn is_set(&self, key: ConfigKey) -> bool {
        match key {
            ConfigKey::DataDir => self.data_dir.is_some(),
            ConfigKey::DownloadMirror => self.download_mirror.is_some(),
            ConfigKey::JavaSource => self.java_source.is_some(),
        }
    }

    /// 设置一项并归一化；失败时配置保持原样（先校验后落地）
    ///
    /// # Errors
    ///
    /// 路径无法归一化或枚举取值不在表内时返回 [`ConfigValueError`]
    pub fn set(&mut self, key: ConfigKey, value: &str) -> Result<(), ConfigValueError> {
        match key {
            ConfigKey::DataDir => {
                let path = parse_path(key.as_str(), value)?;
                self.data_dir = Some(path);
            }
            ConfigKey::DownloadMirror => {
                self.download_mirror = Some(parse_choice(key, value)?);
            }
            ConfigKey::JavaSource => {
                self.java_source = Some(parse_choice(key, value)?);
            }
        }
        Ok(())
    }

    /// 清掉一项，回到平台缺省
    pub fn unset(&mut self, key: ConfigKey) {
        match key {
            ConfigKey::DataDir => self.data_dir = None,
            ConfigKey::DownloadMirror => self.download_mirror = None,
            ConfigKey::JavaSource => self.java_source = None,
        }
    }

    /// 该键的字符串形态（写文件、回显共用）
    #[must_use]
    pub fn raw(&self, key: ConfigKey) -> Option<String> {
        match key {
            ConfigKey::DataDir => self
                .data_dir
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            ConfigKey::DownloadMirror => self.download_mirror.clone(),
            ConfigKey::JavaSource => self.java_source.clone(),
        }
    }

    /// 解析 + 校验整份文本（`config edit` 保存前过这一关）
    ///
    /// # Errors
    ///
    /// TOML 非法、未知键或取值非法时返回说明
    pub fn parse_document(text: &str) -> Result<Self, String> {
        let mut cfg: Self = toml::from_str(text).map_err(|err| err.to_string())?;
        // 逐项重跑设置校验：手改文件同样要过键表（路径归一化、枚举取值）
        for key in ConfigKey::ALL {
            if let Some(raw) = cfg.raw(*key) {
                cfg.set(*key, &raw).map_err(|err| err.describe())?;
            }
        }
        Ok(cfg)
    }

    /// 序列化为文件内容：[`FEATURE_HEADER`] + 已设置的键
    ///
    /// # Errors
    ///
    /// 序列化失败时返回底层错误（字段都是基础类型，正常不可达）
    pub fn to_document(&self) -> Result<String, toml::ser::Error> {
        Ok(format!("{FEATURE_HEADER}{}", toml::to_string_pretty(self)?))
    }
}

/// 写入文件时置顶的说明：手改的人先看到可用键
pub const FEATURE_HEADER: &str = "# mcsup 平台配置；键表见 `mcsup config show`\n\
     # data-dir / download-mirror / java-source，删除某项即回到平台缺省\n\n";

/// 校验并归一化路径
///
/// 相对路径按当前目录展开：存相对路径的话，换个目录运行就指向别处。
/// 只做词法规整，不解析符号链接——配置写的是哪条路径就存哪条。
fn parse_path(key: &'static str, value: &str) -> Result<PathBuf, ConfigValueError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ConfigValueError::Path {
            key,
            value: value.to_owned(),
            reason: "路径为空".to_owned(),
        });
    }
    let err = |reason: String| ConfigValueError::Path {
        key,
        value: value.to_owned(),
        reason,
    };
    let expanded = expand_home(trimmed);
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .map_err(|e| err(e.to_string()))?
            .join(expanded)
    };
    let normalized = lexical_normalize(&absolute);
    if normalized.exists() && !normalized.is_dir() {
        return Err(err("该路径已存在且不是目录".to_owned()));
    }
    Ok(normalized)
}

/// `~` / `~/x` 展开到家目录；其余原样
fn expand_home(value: &str) -> PathBuf {
    let home = dirs::home_dir();
    if value == "~" {
        return home.unwrap_or_else(|| PathBuf::from(value));
    }
    match value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
    {
        Some(rest) => home.map_or_else(|| PathBuf::from(value), |home| home.join(rest)),
        None => PathBuf::from(value),
    }
}

/// 词法归一化：去掉 `.`、折叠 `..`、统一分隔符，不做 IO
#[must_use]
pub fn lexical_normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // 根之上的 `..` 无处可退，忽略（与 shell 的 cd .. 在 / 下同一语义）
                if !out.pop() {
                    out.push(component);
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

fn parse_choice(key: ConfigKey, value: &str) -> Result<String, ConfigValueError> {
    let trimmed = value.trim();
    let choices = key.choices();
    if let Some(found) = choices.iter().find(|c| **c == trimmed) {
        return Ok((*found).to_owned());
    }
    Err(ConfigValueError::Choice {
        key: key.as_str(),
        value: value.to_owned(),
        choices,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_names_are_stable_kebab_case() {
        let names: Vec<&str> = ConfigKey::ALL.iter().map(|k| k.as_str()).collect();
        assert_eq!(names, vec!["data-dir", "download-mirror", "java-source"]);
        assert_eq!(ConfigKey::parse("data-dir"), Some(ConfigKey::DataDir));
        assert_eq!(ConfigKey::parse("Data-Dir"), None);
        assert_eq!(ConfigKey::parse("nope"), None);
    }

    #[test]
    fn defaults_apply_when_unset() {
        let cfg = Config::default();
        assert_eq!(cfg.resolved_download_mirror(), "mojang");
        assert_eq!(cfg.resolved_java_source(), JAVA_SOURCE_ALL);
        for key in ConfigKey::ALL {
            assert!(!cfg.is_set(*key), "{key} 不应算显式设置");
        }
    }

    #[test]
    fn set_validates_before_mutating() {
        let mut cfg = Config::default();
        let err = cfg
            .set(ConfigKey::JavaSource, "nope")
            .expect_err("非法取值");
        assert!(err.describe().contains("adoptium"), "{}", err.describe());
        assert!(!cfg.is_set(ConfigKey::JavaSource), "失败不得写入");

        cfg.set(ConfigKey::JavaSource, "azul").expect("合法");
        assert_eq!(cfg.resolved_java_source(), "azul");
        cfg.unset(ConfigKey::JavaSource);
        assert_eq!(cfg.resolved_java_source(), JAVA_SOURCE_ALL);
    }

    #[test]
    fn data_dir_is_made_absolute_and_normalized() {
        let mut cfg = Config::default();
        cfg.set(ConfigKey::DataDir, "  .//a/../data  ")
            .expect("相对路径按当前目录展开");
        let dir = cfg.resolved_data_dir();
        assert!(dir.is_absolute(), "{}", dir.display());
        assert!(dir.ends_with("data"), "{}", dir.display());
        assert!(!dir.to_string_lossy().contains(".."));
    }

    #[test]
    fn data_dir_rejects_empty_and_files() {
        let mut cfg = Config::default();
        assert!(cfg.set(ConfigKey::DataDir, "   ").is_err());
        let file = std::env::temp_dir().join(format!("mcsup-cfg-{}", std::process::id()));
        std::fs::write(&file, b"x").expect("write");
        let err = cfg
            .set(ConfigKey::DataDir, &file.to_string_lossy())
            .expect_err("文件不是目录");
        assert!(err.describe().contains("不是目录"), "{}", err.describe());
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn document_roundtrips_and_rejects_unknown_keys() {
        let cfg = Config::parse_document("java-source = \"azul\"\n").expect("parse");
        assert_eq!(cfg.resolved_java_source(), "azul");
        let text = cfg.to_document().expect("serialize");
        assert!(text.starts_with(FEATURE_HEADER), "{text}");
        let again = Config::parse_document(&text).expect("reparse");
        assert_eq!(again, cfg);

        assert!(
            Config::parse_document("nope = 1\n").is_err(),
            "未知键必须拒绝"
        );
        assert!(
            Config::parse_document("java-source = \"nope\"\n").is_err(),
            "非法取值必须拒绝"
        );
    }

    #[test]
    fn lexical_normalize_folds_dot_segments() {
        let out = lexical_normalize(Path::new("/a/b/../c/./d"));
        assert_eq!(out, PathBuf::from("/a/c/d"));
    }
}
