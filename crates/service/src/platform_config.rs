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
use url::Url;

/// 批量下载并发文件数的缺省值
///
/// 与 `file` crate 的 `BATCH_CONCURRENCY` 一致；这里再写一份是刻意的：
/// 配置缺省是**产品决定**，下载器的内置缺省是实现细节，两者可以独立变。
///
/// 实测单连接约 45 KiB/s，小于分段阈值的小文件只能单流，批量吞吐完全由
/// 这个并发数决定；24 路在小文件占多数的整合包清单上能把吞吐拉满（详见
/// `docs/concurrency-performance.md` 的下载并发一节）。
pub const DEFAULT_DOWNLOAD_CONCURRENCY: usize = 24;

/// 并发文件数上限
///
/// 高于缺省的加并发只在大量中大型文件同时下载时才继续有收益，且同一份文件被
/// 切成几十段并发拉时，服务端与中间设备更容易掐连接。给个上限既防着用户手抖
/// 写个 1000，也不至于把真正宽裕的链路卡死。
pub const MAX_DOWNLOAD_CONCURRENCY: usize = 64;

/// 全部下载在途连接数的缺省上限
///
/// 两层并发各自有界、乘起来没有界；单连接约 45 KiB/s 的限速下，聚合吞吐随
/// 在途连接数近似线性增长，直到约 15 MiB/s 的单节点容量（详见
/// `docs/concurrency-performance.md`）。256 是「批量并发 24 路 × 每个大文件
/// 约 10 段」的量级，实测把 109 文件 / 594 MiB 的整合包从约 1.4 MiB/s 提到
/// 约 8 MiB/s，同时仍低于单节点容量、不触发上游限流。
pub const DEFAULT_DOWNLOAD_INFLIGHT: usize = 256;

/// 在途连接上限的最大值
///
/// 再高只会换来本地端口与上游限流上的抖动；这里给足空间（512）同时挡住
/// 手抖写成几万的情况。
pub const MAX_DOWNLOAD_INFLIGHT: usize = 512;

/// 列表型配置项的分隔符（人类可读写，无需引号）
const ENTRIES_SEPARATOR: &str = ",";

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
    /// 批量下载并发文件数（缺省 [`DEFAULT_DOWNLOAD_CONCURRENCY`]）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_concurrency: Option<usize>,
    /// mod 文件镜像前缀，按顺序在主源之前尝试
    ///
    /// 存**前缀**而不是完整地址：镜像站的路径规则跟着上游走（Modrinth 是
    /// `https://mirror.example/data/<项目>/versions/<版本>/<文件>`），存整条
    /// 地址就得为一个整合包写几百行配置，而且上游一换文件名全作废。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub download_sources: Vec<String>,
    /// 全部下载在途连接上限（缺省 [`DEFAULT_DOWNLOAD_INFLIGHT`]）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_inflight: Option<usize>,
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
}

/// 远端接入方式
///
/// 刻意不加 `#[non_exhaustive]`：每种方式要装配不同的连接实现（SSH 隧道或
/// 直连），未知方式不能静默按其中一种连接。
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
#[non_exhaustive]
pub enum ConfigKey {
    /// `data-dir`：平台数据目录
    DataDir,
    /// `download-concurrency`：批量下载的并发文件数
    DownloadConcurrency,
    /// `download-sources`：mod 文件的镜像前缀，逗号分隔
    DownloadSources,
    /// `download-inflight`：全部下载合起来的同时在途连接数上限
    DownloadInflight,
}

impl ConfigKey {
    /// 全部配置键，顺序即 `config show` 与报错文案的稳定顺序
    pub const ALL: &'static [Self] = &[
        Self::DataDir,
        Self::DownloadConcurrency,
        Self::DownloadSources,
        Self::DownloadInflight,
    ];

    /// 配置文件里的键名（kebab-case，与 `server set` 同一口径）
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DataDir => "data-dir",
            Self::DownloadConcurrency => "download-concurrency",
            Self::DownloadSources => "download-sources",
            Self::DownloadInflight => "download-inflight",
        }
    }

    /// 一行说明（`config show` 的人类视图）
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::DataDir => "平台数据目录（workspaces / java / db 都在其下）",
            Self::DownloadConcurrency => "批量下载并发文件数（缺省 24，范围 1–64）",
            Self::DownloadSources => "mod 文件镜像前缀，逗号分隔（缺省只用官方源）",
            Self::DownloadInflight => "全部下载在途连接上限（缺省 256，范围 1–512）",
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
            Self::DownloadConcurrency => DEFAULT_DOWNLOAD_CONCURRENCY.to_string(),
            Self::DownloadSources => ENTRIES_SEPARATOR.to_owned(),
            Self::DownloadInflight => DEFAULT_DOWNLOAD_INFLIGHT.to_string(),
        }
    }
}

impl std::fmt::Display for ConfigKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 配置值非法
///
/// `Display` 只给英文短语：库产出的文案由入口层决定（`docs/rust-standards.md`
/// 的「判定与文案分离」）。面向用户的中文说明由入口用 [`Self::kind`] 与各字段
/// 自行拼装。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ConfigValueError {
    /// 路径为空或无法归一化
    #[error("{key} is not a usable path: {value} ({reason})")]
    Path {
        /// 键名
        key: &'static str,
        /// 原始输入
        value: String,
        /// 具体原因（英文短语，由本 crate 给出）
        reason: &'static str,
    },
    /// 数值型键的取值越界或不是数字
    #[error("{key} must be a number in {min}..={max}, got {value:?}")]
    Number {
        /// 键名
        key: &'static str,
        /// 原始输入
        value: String,
        /// 允许的下界
        min: usize,
        /// 允许的上界
        max: usize,
    },
    /// 列表型键的某一项不可用（空项、非 http(s)、含查询或锚点）
    #[error("{key} entry {value:?} is not a usable base URL ({reason})")]
    Entry {
        /// 键名
        key: &'static str,
        /// 不可用的那一项
        value: String,
        /// 具体原因（英文短语，由本 crate 给出）
        reason: &'static str,
    },
}

impl ConfigValueError {
    /// 失败等级：键的取值不可用，改值即可成功
    #[must_use]
    pub fn category(&self) -> port::ErrorCategory {
        match self {
            Self::Path { .. } | Self::Number { .. } | Self::Entry { .. } => {
                port::ErrorCategory::Usage
            }
        }
    }

    /// 错误种类：入口据此选择本地化文案
    #[must_use]
    pub fn kind(&self) -> ConfigValueErrorKind {
        match self {
            Self::Path { .. } => ConfigValueErrorKind::Path,
            Self::Number { .. } => ConfigValueErrorKind::Number,
            Self::Entry { .. } => ConfigValueErrorKind::Entry,
        }
    }
}

/// [`ConfigValueError`] 的可本地化种类
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConfigValueErrorKind {
    /// 路径不可用
    Path,
    /// 数值越界
    Number,
    /// 列表项不是可用的地址前缀
    Entry,
}

/// 整份配置文本无法解析
///
/// 与 [`ConfigValueError`]（单个键的取值问题）分开：调用方对「整份文件读不懂」
/// 与「某个键的值不对」的处置不同，混成一个 `String` 就再也分不开。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ConfigDocumentError {
    /// TOML 语法错误
    #[error("config file is not valid TOML: {reason}")]
    Syntax {
        /// 解析器给出的说明
        reason: String,
    },
    /// 某个键的取值非法
    #[error("{0}")]
    Value(#[from] ConfigValueError),
    /// 远端名不可用作配置键
    #[error("remote name {name:?} is not usable as a config key")]
    RemoteName {
        /// 原始键名
        name: String,
    },
}

impl ConfigDocumentError {
    /// 失败等级：整份配置文本读不懂是调用方写错了文件
    #[must_use]
    pub fn category(&self) -> port::ErrorCategory {
        match self {
            Self::Value(err) => err.category(),
            Self::Syntax { .. } | Self::RemoteName { .. } => port::ErrorCategory::Usage,
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

    /// 生效的批量下载并发文件数
    ///
    /// 未设置时给 [`DEFAULT_DOWNLOAD_CONCURRENCY`]。写入侧已由
    /// [`ConfigKey::DownloadConcurrency`] 的校验保证落在 `1..=MAX`，这里不再
    /// 二次夹取——真出现越界说明配置被绕过了键表，应当暴露而不是被悄悄修正。
    #[must_use]
    pub fn resolved_download_concurrency(&self) -> usize {
        self.download_concurrency
            .unwrap_or(DEFAULT_DOWNLOAD_CONCURRENCY)
    }

    /// 生效的全部下载在途连接上限
    ///
    /// 未设置时给 [`DEFAULT_DOWNLOAD_INFLIGHT`]。它是批量并发与分段并发
    /// 共同的闸门，缺省值只要不比缺省并发数小就不会拖慢正常流程。
    #[must_use]
    pub fn resolved_download_inflight(&self) -> usize {
        self.download_inflight.unwrap_or(DEFAULT_DOWNLOAD_INFLIGHT)
    }

    /// 生效的 mod 镜像前缀，已解析为地址
    ///
    /// 解析放在这里而不是入口：拼接镜像地址是下载语义的一部分，三个入口
    /// （CLI / API / MCP）都要用同一套规则，放进任何一个入口都会分叉。
    /// 写入侧已挡掉非法前缀，因此解析失败只会来自手改文件，按「忽略这一项」
    /// 处理——镜像只是加速手段，为它整体失败会让一份能跑的配置突然不可用。
    #[must_use]
    pub fn resolved_download_sources(&self) -> Vec<Url> {
        self.download_sources
            .iter()
            .filter_map(|raw| Url::parse(raw).ok())
            .collect()
    }

    /// 该键的生效值，统一为展示用字符串
    ///
    /// 返回 `String` 而不是 `PathBuf`：键表里既有路径也有数值与列表，用一个
    /// 能表达三者的形态才不会为了「多一个数值键」去改所有调用方。列表按
    /// 逗号连接，因此 `raw` 与 `effective` 对同一份配置
    /// 产出的文本是一致的（未设置时 `effective` 给缺省、`raw` 给 `None`）。
    #[must_use]
    pub fn effective(&self, key: ConfigKey) -> String {
        match key {
            ConfigKey::DataDir => self.resolved_data_dir().display().to_string(),
            ConfigKey::DownloadConcurrency => self
                .download_concurrency
                .unwrap_or(DEFAULT_DOWNLOAD_CONCURRENCY)
                .to_string(),
            ConfigKey::DownloadSources => self.download_sources.join(ENTRIES_SEPARATOR),
            ConfigKey::DownloadInflight => self
                .download_inflight
                .unwrap_or(DEFAULT_DOWNLOAD_INFLIGHT)
                .to_string(),
        }
    }

    /// 该键是否在文件中显式设置（`show` 用它区分缺省与显式值）
    #[must_use]
    pub fn is_set(&self, key: ConfigKey) -> bool {
        match key {
            ConfigKey::DataDir => self.data_dir.is_some(),
            ConfigKey::DownloadConcurrency => self.download_concurrency.is_some(),
            ConfigKey::DownloadSources => !self.download_sources.is_empty(),
            ConfigKey::DownloadInflight => self.download_inflight.is_some(),
        }
    }

    /// 设置一项并归一化；失败时配置保持原样（先校验后落地）
    ///
    /// # Errors
    ///
    /// 路径无法归一化、数值越界或列表项不是合法 URL 前缀时返回
    /// [`ConfigValueError`]
    pub fn set(&mut self, key: ConfigKey, value: &str) -> Result<(), ConfigValueError> {
        match key {
            ConfigKey::DataDir => {
                let path = parse_path(key.as_str(), value)?;
                self.data_dir = Some(path);
            }
            ConfigKey::DownloadConcurrency => {
                self.download_concurrency = Some(parse_concurrency(key.as_str(), value)?);
            }
            ConfigKey::DownloadSources => {
                self.download_sources = parse_sources(key.as_str(), value)?;
            }
            ConfigKey::DownloadInflight => {
                self.download_inflight = Some(parse_inflight(key.as_str(), value)?);
            }
        }
        Ok(())
    }

    /// 清掉一项，回到平台缺省
    pub fn unset(&mut self, key: ConfigKey) {
        match key {
            ConfigKey::DataDir => self.data_dir = None,
            ConfigKey::DownloadConcurrency => self.download_concurrency = None,
            ConfigKey::DownloadSources => self.download_sources.clear(),
            ConfigKey::DownloadInflight => self.download_inflight = None,
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
            ConfigKey::DownloadConcurrency => self.download_concurrency.map(|n| n.to_string()),
            ConfigKey::DownloadSources => (!self.download_sources.is_empty())
                .then(|| self.download_sources.join(ENTRIES_SEPARATOR)),
            ConfigKey::DownloadInflight => self.download_inflight.map(|n| n.to_string()),
        }
    }

    /// 解析 + 校验整份文本（`config edit` 保存前过这一关）
    ///
    /// # Errors
    ///
    /// 返回 `ConfigDocumentError`：TOML 非法、未知键或取值非法
    pub fn parse_document(text: &str) -> Result<Self, ConfigDocumentError> {
        let mut cfg: Self = toml::from_str(text).map_err(|err| ConfigDocumentError::Syntax {
            reason: err.to_string(),
        })?;
        // 逐项重跑设置校验：手改文件同样要过键表（路径归一化、枚举取值）
        for key in ConfigKey::ALL {
            if let Some(raw) = cfg.raw(*key) {
                cfg.set(*key, &raw)?;
            }
        }
        // 远端名是配置键，手改文件同样要过同一套校验
        for name in cfg.remotes.keys() {
            RemoteName::parse(name)
                .map_err(|_| ConfigDocumentError::RemoteName { name: name.clone() })?;
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
     # data-dir / download-concurrency / download-sources / download-inflight，\n\
     # 删除任一项即回到该项的平台缺省\n\
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
            reason: "path is empty",
        });
    }
    let err = |reason: &'static str| ConfigValueError::Path {
        key,
        value: value.to_owned(),
        reason,
    };
    let expanded = expand_home(trimmed);
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .map_err(|_| err("cannot resolve the current directory"))?
            .join(expanded)
    };
    let normalized = lexical_normalize(&absolute);
    if normalized.exists() && !normalized.is_dir() {
        return Err(err("path exists and is not a directory"));
    }
    Ok(normalized)
}

/// 校验并发文件数
///
/// 范围收紧到 `1..=MAX_DOWNLOAD_CONCURRENCY`：`0` 在下层是「用实现内置缺省」的
/// 哨兵值，配置里允许写会让人以为关掉了下载；上限见该常量的说明。
fn parse_concurrency(key: &'static str, value: &str) -> Result<usize, ConfigValueError> {
    let trimmed = value.trim();
    let number = trimmed
        .parse::<usize>()
        .ok()
        .filter(|n| (1..=MAX_DOWNLOAD_CONCURRENCY).contains(n));
    number.ok_or_else(|| ConfigValueError::Number {
        key,
        value: value.to_owned(),
        min: 1,
        max: MAX_DOWNLOAD_CONCURRENCY,
    })
}

/// 校验在途连接上限
fn parse_inflight(key: &'static str, value: &str) -> Result<usize, ConfigValueError> {
    let trimmed = value.trim();
    let number = trimmed
        .parse::<usize>()
        .ok()
        .filter(|n| (1..=MAX_DOWNLOAD_INFLIGHT).contains(n));
    number.ok_or_else(|| ConfigValueError::Number {
        key,
        value: value.to_owned(),
        min: 1,
        max: MAX_DOWNLOAD_INFLIGHT,
    })
}

/// 校验并归一化镜像前缀列表
///
/// 每一项都是**前缀**，不是完整地址：下载器会把上游路径原样接在后面。因此
/// 校验必须拒掉带查询串或锚点的写法——它们拼上路径后语义不可意料，用户会
/// 得到一个只在部分文件上失败的诡异状态，而不是一条明确的配置错误。
///
/// 空项被丢弃而不是报错：`a.com,,b.com` 这种手滑不该让整份配置不可用。
fn parse_sources(key: &'static str, value: &str) -> Result<Vec<String>, ConfigValueError> {
    let mut sources = Vec::new();
    for entry in value.split(ENTRIES_SEPARATOR) {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        let err = |reason: &'static str| ConfigValueError::Entry {
            key,
            value: trimmed.to_owned(),
            reason,
        };
        if !(trimmed.starts_with("https://") || trimmed.starts_with("http://")) {
            return Err(err("only http:// or https:// prefixes are supported"));
        }
        if trimmed.contains('?') || trimmed.contains('#') {
            return Err(err("a mirror prefix cannot carry a query or a fragment"));
        }
        sources.push(trimmed.trim_end_matches('/').to_owned());
    }
    Ok(sources)
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
        assert_eq!(
            names,
            vec![
                "data-dir",
                "download-concurrency",
                "download-sources",
                "download-inflight"
            ]
        );
        assert_eq!(ConfigKey::parse("data-dir"), Some(ConfigKey::DataDir));
        assert_eq!(
            ConfigKey::parse("download-concurrency"),
            Some(ConfigKey::DownloadConcurrency)
        );
        assert_eq!(
            ConfigKey::parse("download-sources"),
            Some(ConfigKey::DownloadSources)
        );
        assert_eq!(
            ConfigKey::parse("download-inflight"),
            Some(ConfigKey::DownloadInflight)
        );
        assert_eq!(ConfigKey::parse("Data-Dir"), None);
        assert_eq!(ConfigKey::parse("nope"), None);
    }

    /// 并发数只接受 1..=上限：`0` 在下层是「用实现内置缺省」的哨兵，
    /// 配置里写它会让人以为关掉了下载，所以必须在键表这一层就挡掉。
    #[test]
    fn concurrency_rejects_out_of_range() {
        let mut cfg = Config::default();
        for bad in ["0", "65", "abc", "", "-1", "8.5"] {
            assert!(
                cfg.set(ConfigKey::DownloadConcurrency, bad).is_err(),
                "{bad:?} 应被拒绝"
            );
            assert!(!cfg.is_set(ConfigKey::DownloadConcurrency), "失败不得写入");
        }
        for good in ["1", "8", "64"] {
            cfg.set(ConfigKey::DownloadConcurrency, good)
                .unwrap_or_else(|err| panic!("{good:?} 应被接受: {err}"));
            assert_eq!(cfg.effective(ConfigKey::DownloadConcurrency), good);
        }
        cfg.unset(ConfigKey::DownloadConcurrency);
        assert_eq!(
            cfg.effective(ConfigKey::DownloadConcurrency),
            DEFAULT_DOWNLOAD_CONCURRENCY.to_string()
        );
    }

    /// 镜像前缀必须是纯前缀：带查询串或锚点的写法拼上路径后语义不可意料，
    /// 会让下载只在部分文件上失败，比直接报配置错误难查得多。
    #[test]
    fn sources_reject_non_prefix_urls() {
        let mut cfg = Config::default();
        for bad in [
            "mirror.example",
            "ftp://mirror.example",
            "https://mirror.example/?a=1",
            "https://mirror.example/#x",
        ] {
            assert!(
                cfg.set(ConfigKey::DownloadSources, bad).is_err(),
                "{bad:?} 应被拒绝"
            );
        }
        // 空项被丢弃而不是整份报错：手滑多打一个逗号不该让配置不可用
        cfg.set(
            ConfigKey::DownloadSources,
            " https://a.example/ , , https://b.example ",
        )
        .expect("合法");
        assert_eq!(
            cfg.download_sources,
            vec![
                "https://a.example".to_owned(),
                "https://b.example".to_owned()
            ]
        );
        assert!(cfg.is_set(ConfigKey::DownloadSources));
        // 列表写回文本后要能被重新解析出同一份列表
        let raw = cfg.raw(ConfigKey::DownloadSources).expect("已设置");
        assert_eq!(raw, "https://a.example,https://b.example");
        let mut again = Config::default();
        again.set(ConfigKey::DownloadSources, &raw).expect("回填");
        assert_eq!(again.download_sources, cfg.download_sources);
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
        assert_eq!(err.kind(), ConfigValueErrorKind::Path);
        assert!(err.to_string().contains("not a usable path"), "{err}");
        assert!(
            err.to_string()
                .contains("path exists and is not a directory"),
            "{err}"
        );
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
        // 结构化错误：不再是「整份说明字符串里找子串」，而是能直接取到那个键
        let ConfigDocumentError::RemoteName { name } = &err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(name, "a/b");

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
