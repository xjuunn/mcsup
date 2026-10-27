//! 平台配置：`config.toml` 的类型、键表、校验与读显
//!
//! 配置是机器级偏好（数据目录位置），不含工作区列表——
//! 那在 `db/workspaces/`，双真相源会让两边分叉。
//!
//! 键表是唯一真相来源：`config show` / `config set` / `config edit` 的键名、
//! 类型、校验与归一化全部从这里派生，CLI 只做参数搬运与渲染。新增配置项
//! 时在 [`Config`] 加字段、在 [`ConfigKey`] 加变体，两处漏一处的编译错误
//! 会把差异挡在门禁里。
//!
//! 路径类字段按 `~` 展开与归一化后保存：文件里的值是给人看的，运行时
//! 才拼成绝对路径；保存前就归一化，避免同一目录因写法不同出现两份。

use port::RemoteName;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// 平台配置的磁盘形态（`config.toml` 的整体结构）
///
/// 字段全部 `Option`：未设置即「用平台缺省」。`resolved_*` 系列负责
/// 把缺省填上，写入时仍保留 `None`，避免把缺省值固化进文件。
///
/// `remotes` 是唯一不走 [`ConfigKey`] 的一项：它是**集合**而非标量，
/// 由 `mcsup remote connect` / `disconnect` 增删，`config set` 不接管。
/// 把它做成 `ConfigKey` 会让「用一行字符串表达一条配对记录」成为必然。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct Config {
    /// 平台数据目录（缺省取系统数据目录下的 `mcsup`）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<PathBuf>,
    /// 已配对的远端：键是远端名，值是一条配对记录
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub remotes: BTreeMap<String, RemoteRecord>,
}

/// 一条远端配对记录
///
/// **凭证本体不写进配置文件**：`credential` 只是句柄（钥匙串条目名或
/// `file:<路径>`），token 由它指向的存储保管（`docs/remote.md`「安全」）。
/// 这样配置文件可以安全地被备份、被 `cat`、被贴进 issue。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct RemoteRecord {
    /// 用户输入的原始 target（`user@host` / `host:port`），原样保存
    ///
    /// 不归一化：`reconnect` 要按原样重放同一条接入路径，改写会丢失
    /// 用户意图（例如 `user@host` 与 `host` 走的是不同认证方式）。
    pub target: String,
    /// 接入方式
    pub via: RemoteVia,
    /// 以只读权限配对（远端侧角色为 viewer）
    #[serde(default)]
    pub read_only: bool,
    /// 配对时间（Unix 秒）
    #[serde(default)]
    pub added_at: i64,
    /// 服务端证书指纹（TOFU 首次确认后固定；变化必须显式重新配对）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// 凭证句柄：钥匙串条目名，或 `file:<路径>`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// SSH 接入时远端 agent 的监听地址（隧道重建需要）
    ///
    /// `__serve-ssh` 每次启动用随机端口，所以隧道必须知道上一次握手给出的
    /// 地址；直连接入不需要这个字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_addr: Option<String>,
}

/// 远端接入方式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RemoteVia {
    /// 复用 SSH 信任：连得上就说明有权限
    Ssh,
    /// 直连 `host:port`（配合配对码）
    Direct,
}

impl RemoteVia {
    /// 稳定文本（配置值、JSON、提示共用）
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ssh => "ssh",
            Self::Direct => "direct",
        }
    }
}

impl std::fmt::Display for RemoteVia {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
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
}

impl ConfigKey {
    /// 全部配置键，顺序即 `config show` 与报错文案的稳定顺序
    pub const ALL: &'static [Self] = &[Self::DataDir];

    /// 配置文件里的键名（kebab-case，与 `server set` 同一口径）
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DataDir => "data-dir",
        }
    }

    /// 一行说明（`config show` 的人类视图）
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::DataDir => "平台数据目录（workspaces / java / db 都在其下）",
        }
    }

    /// 按名解析；未知名返回 `None`（调用方据此报用法错误并列出 [`Self::ALL`]）
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|key| key.as_str() == name)
    }

    /// 该键在未设置时的生效值（人类视图里标「缺省」用）
    #[must_use]
    pub fn default_display(self) -> String {
        match self {
            Self::DataDir => Config::default_data_dir().display().to_string(),
        }
    }
}

impl std::fmt::Display for ConfigKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

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
}

impl ConfigValueError {
    /// 面向用户的完整说明（含可用取值）
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Path { key, value, reason } => {
                format!("{key} 不是可用路径: {value}（{reason}）")
            }
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

    /// 该键的生效值（当前只有 `data-dir`，即规范化后的绝对路径）
    #[must_use]
    pub fn effective(&self, key: ConfigKey) -> PathBuf {
        match key {
            ConfigKey::DataDir => self.resolved_data_dir(),
        }
    }

    /// 该键是否在文件中显式设置（`show` 用它区分缺省与显式值）
    #[must_use]
    pub fn is_set(&self, key: ConfigKey) -> bool {
        match key {
            ConfigKey::DataDir => self.data_dir.is_some(),
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
        }
        Ok(())
    }

    /// 清掉一项，回到平台缺省
    pub fn unset(&mut self, key: ConfigKey) {
        match key {
            ConfigKey::DataDir => self.data_dir = None,
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
        // 远端名是配置键，手改文件同样要过同一套校验
        for name in cfg.remotes.keys() {
            RemoteName::parse(name).map_err(|err| format!("remotes.{name}: {err}"))?;
        }
        Ok(cfg)
    }

    /// 已配对远端的名字，按名字升序（`BTreeMap` 天然有序，展示稳定）
    pub fn remote_names(&self) -> impl Iterator<Item = &str> {
        self.remotes.keys().map(String::as_str)
    }

    /// 按名取一条配对记录
    #[must_use]
    pub fn remote(&self, name: &RemoteName) -> Option<&RemoteRecord> {
        self.remotes.get(name.as_str())
    }

    /// 写入（覆盖）一条配对记录
    ///
    /// 覆盖语义是有意的：`connect` 幂等，重复配对等于刷新 token 与指纹。
    pub fn put_remote(&mut self, name: &RemoteName, record: RemoteRecord) {
        self.remotes.insert(name.as_str().to_owned(), record);
    }

    /// 删除一条配对记录，返回被删的那条（`None` = 本来就没有）
    pub fn remove_remote(&mut self, name: &RemoteName) -> Option<RemoteRecord> {
        self.remotes.remove(name.as_str())
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
     # data-dir，删除该项即回到平台缺省\n\
     # remotes.* 由 `mcsup remote connect` / `disconnect` 维护，不要手改\n\n";

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_names_are_stable_kebab_case() {
        let names: Vec<&str> = ConfigKey::ALL.iter().map(|k| k.as_str()).collect();
        assert_eq!(names, vec!["data-dir"]);
        assert_eq!(ConfigKey::parse("data-dir"), Some(ConfigKey::DataDir));
        assert_eq!(ConfigKey::parse("Data-Dir"), None);
        assert_eq!(ConfigKey::parse("nope"), None);
    }

    #[test]
    fn defaults_apply_when_unset() {
        let cfg = Config::default();
        assert_eq!(
            cfg.effective(ConfigKey::DataDir),
            Config::default_data_dir()
        );
        for key in ConfigKey::ALL {
            assert!(!cfg.is_set(*key), "{key} 不应算显式设置");
        }
    }

    #[test]
    fn set_validates_before_mutating() {
        let mut cfg = Config::default();
        let before = cfg.effective(ConfigKey::DataDir);
        assert!(cfg.set(ConfigKey::DataDir, "   ").is_err(), "空路径非法");
        assert!(!cfg.is_set(ConfigKey::DataDir), "失败不得写入");
        assert_eq!(cfg.effective(ConfigKey::DataDir), before);

        cfg.set(ConfigKey::DataDir, "sub/data").expect("合法");
        assert!(cfg.is_set(ConfigKey::DataDir));
        cfg.unset(ConfigKey::DataDir);
        assert!(!cfg.is_set(ConfigKey::DataDir));
        assert_eq!(cfg.effective(ConfigKey::DataDir), before);
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
        // 用平台自带的绝对路径：`/tmp/...` 在 Windows 上不是绝对路径，
        // 会被按当前目录展开，断言两边必然不一致
        let raw = std::env::temp_dir()
            .join("mcsup-data")
            .to_string_lossy()
            .into_owned();
        let cfg = Config::parse_document(&format!("data-dir = '{raw}'\n")).expect("parse");
        assert_eq!(
            cfg.effective(ConfigKey::DataDir),
            PathBuf::from(&raw),
            "{raw}"
        );
        let text = cfg.to_document().expect("serialize");
        assert!(text.starts_with(FEATURE_HEADER), "{text}");
        let again = Config::parse_document(&text).expect("reparse");
        assert_eq!(again, cfg);

        assert!(
            Config::parse_document("nope = 1\n").is_err(),
            "未知键必须拒绝"
        );
        assert!(
            Config::parse_document("download-mirror = \"mojang\"\n").is_err(),
            "已移除的键必须拒绝"
        );
        assert!(
            Config::parse_document("java-source = \"azul\"\n").is_err(),
            "已移除的键必须拒绝"
        );
        assert!(
            Config::parse_document("data-dir = \"   \"\n").is_err(),
            "非法取值必须拒绝"
        );
    }

    #[test]
    fn lexical_normalize_folds_dot_segments() {
        let out = lexical_normalize(Path::new("/a/b/../c/./d"));
        assert_eq!(out, PathBuf::from("/a/c/d"));
    }

    fn record() -> RemoteRecord {
        RemoteRecord {
            target: "user@prod-box".to_owned(),
            via: RemoteVia::Ssh,
            read_only: false,
            added_at: 1_700_000_000,
            fingerprint: None,
            credential: None,
            remote_addr: None,
        }
    }

    /// 配对记录走 kebab-case 落盘，且不含 token 本体
    #[test]
    fn remote_records_roundtrip_without_secrets() {
        let mut cfg = Config::default();
        let name = RemoteName::parse("prod-box").expect("name");
        cfg.put_remote(
            &name,
            RemoteRecord {
                credential: Some("keyring:mcsup/prod-box".to_owned()),
                ..record()
            },
        );
        let text = cfg.to_document().expect("serialize");
        assert!(text.contains("read-only = false"), "{text}");
        assert!(text.contains("via = \"ssh\""), "{text}");
        assert!(!text.to_lowercase().contains("token"), "{text}");

        let again = Config::parse_document(&text).expect("reparse");
        assert_eq!(again, cfg);
        assert_eq!(again.remote(&name).expect("record").target, "user@prod-box");
    }

    /// 手改出来的非法远端名必须在加载时被挡下，而不是留到运行期
    #[test]
    fn invalid_remote_names_are_rejected_on_load() {
        let text = "[remotes.\"a/b\"]\ntarget = \"host\"\nvia = \"ssh\"\n";
        let err = Config::parse_document(text).expect_err("名字含 / 必须拒绝");
        assert!(err.contains("remotes.a/b"), "{err}");

        let text = "[remotes.all]\ntarget = \"host\"\nvia = \"ssh\"\n";
        assert!(Config::parse_document(text).is_err(), "保留字必须拒绝");
    }

    /// 未知字段被拒绝：手写错键名不该静默失效
    #[test]
    fn remote_records_reject_unknown_fields() {
        let text = "[remotes.prod]\ntarget = \"h\"\nvia = \"ssh\"\nnope = 1\n";
        assert!(Config::parse_document(text).is_err());
        let text = "[remotes.prod]\ntarget = \"h\"\nvia = \"carrier-pigeon\"\n";
        assert!(Config::parse_document(text).is_err());
    }

    #[test]
    fn remove_remote_reports_whether_it_existed() {
        let mut cfg = Config::default();
        let name = RemoteName::parse("prod-box").expect("name");
        cfg.put_remote(&name, record());
        assert_eq!(cfg.remote_names().collect::<Vec<_>>(), vec!["prod-box"]);
        assert!(cfg.remove_remote(&name).is_some());
        assert!(cfg.remove_remote(&name).is_none());
        assert_eq!(cfg.remote_names().count(), 0);
    }
}
