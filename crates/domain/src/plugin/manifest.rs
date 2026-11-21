//! manifest：插件自述与安装校验
//!
//! manifest 是插件对宿主的**声明**，不是可执行代码。安装阶段只读它，绝不
//! 运行插件；未知能力、未知契约、未知顶层字段一律拒绝——旧宿主装上「永远
//! 不可用」的插件，比明确拒绝更难排查。
//!
//! 校验结果的类型是 [`ValidatedManifest`]：能力、订阅、契约都已解析成闭集
//! 枚举，后续层不必再解析字符串。

use crate::plugin::capability::{self, Capability};
use crate::plugin::contract::{self, ContractId};
use crate::plugin::event::{self, Subscription};
use crate::plugin::{
    InvalidPluginId, MANIFEST_SCHEMA_VERSION, PLUGIN_API_VERSION, PluginId, PluginScope,
    UnknownScope,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// manifest 顶层允许的字段
const TOP_LEVEL_FIELDS: &[&str] = &[
    "schema",
    "id",
    "name",
    "description",
    "version",
    "api",
    "entry",
    "scope",
    "resident",
    "capabilities",
    "net",
    "subscriptions",
    "provides",
    "consumes",
    "resources",
    "publisher",
    "license",
    "source",
];

/// 能力列表上限
pub const MAX_CAPABILITIES: usize = 16;

/// 订阅列表上限
pub const MAX_SUBSCRIPTIONS: usize = 32;

/// 显示名 / 标题长度上限（字符）
pub const MAX_NAME_CHARS: usize = 64;

/// 描述长度上限（字符）
pub const MAX_DESCRIPTION_CHARS: usize = 512;

/// 推荐资源上限（MB）
pub const MAX_RECOMMENDED_MEMORY_MB: u32 = 65_536;

/// 插件入口：v1 只有外部进程
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntrySpec {
    /// 入口形态；v1 必须是 `process`
    #[serde(rename = "type")]
    pub kind: String,
    /// 相对产物根的默认命令
    pub cmd: String,
    /// 平台覆盖，如 `linux-x64` → `bin/linux-x64/plugin`
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub platforms: BTreeMap<String, String>,
}

impl EntrySpec {
    /// 解析 `cmd` 是否为安全的相对路径
    ///
    /// # Errors
    ///
    /// 绝对路径、`..`、盘符或反斜杠分隔符返回 [`ManifestError::InvalidEntry`]
    pub fn validate_cmd(&self) -> Result<(), ManifestError> {
        validate_relative_path(&self.cmd)
    }

    /// 当前平台的入口命令
    ///
    /// 平台键形如 `darwin-arm64` / `linux-x64` / `win32-x64`。
    #[must_use]
    pub fn cmd_for(&self, platform: &str) -> Option<&str> {
        self.platforms
            .get(platform)
            .map(String::as_str)
            .or(Some(self.cmd.as_str()))
    }

    /// 校验入口文件在产物目录里真实可用
    ///
    /// 规则（`docs/plugins.md` 10.3 第 5 条）：解析到当前平台的 `cmd` 必须
    /// 指向一个存在的普通文件；Unix 校验可执行位（`0o111` 任一位），
    /// Windows 校验扩展名：原生插件用 `.exe` 并检查 PE 头（`MZ`），
    /// 脚本型插件（如 Node SDK 产物）允许 `.cmd` / `.bat`，由宿主经
    /// `cmd.exe /C` 启动——Windows 上「可执行」由扩展名与头部决定，mode
    /// 位不存在。
    ///
    /// # Errors
    ///
    /// 平台命令缺失、文件不存在、不是普通文件或平台形态不符时返回
    /// [`ManifestError::InvalidEntry`]
    pub fn check_entry_file(
        &self,
        artifact_root: &Path,
        platform: &str,
    ) -> Result<(), ManifestError> {
        let Some(cmd) = self.cmd_for(platform) else {
            return Err(ManifestError::InvalidEntry {
                reason: format!("no entry command for platform {platform}"),
            });
        };
        // `cmd` 已过 `validate_cmd` 的相对路径校验；这里只拼路径。
        let path = artifact_root.join(cmd.replace('/', std::path::MAIN_SEPARATOR_STR));
        let meta = std::fs::metadata(&path).map_err(|err| ManifestError::InvalidEntry {
            reason: format!("entry file {} is missing: {err}", path.display()),
        })?;
        if !meta.is_file() {
            return Err(ManifestError::InvalidEntry {
                reason: format!("entry {} is not a regular file", path.display()),
            });
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if meta.permissions().mode() & 0o111 == 0 {
                return Err(ManifestError::InvalidEntry {
                    reason: format!("entry {} is not executable", path.display()),
                });
            }
        }
        #[cfg(windows)]
        {
            let has_ext = |expected: &str| {
                path.extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case(expected))
            };
            if has_ext("cmd") || has_ext("bat") {
                // 脚本启动器：内容由 `cmd.exe` 解释，没有 PE 头可校验
            } else if has_ext("exe") {
                // PE 头：文件前两字节必须是 `MZ`。损坏或改名的文件在这里拦下。
                let mut header = [0_u8; 2];
                let read = std::fs::File::open(&path)
                    .and_then(|mut file| std::io::Read::read_exact(&mut file, &mut header));
                match read {
                    Ok(()) if header == *b"MZ" => {}
                    Ok(()) => {
                        return Err(ManifestError::InvalidEntry {
                            reason: format!(
                                "entry {} does not have a PE header (MZ)",
                                path.display()
                            ),
                        });
                    }
                    Err(err) => {
                        return Err(ManifestError::InvalidEntry {
                            reason: format!("entry {} is unreadable: {err}", path.display()),
                        });
                    }
                }
            } else {
                return Err(ManifestError::InvalidEntry {
                    reason: format!(
                        "entry {} must have an .exe, .cmd or .bat extension",
                        path.display()
                    ),
                });
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = platform;
        }
        Ok(())
    }
}

/// 出站白名单
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetSpec {
    /// 允许的主机名或后缀
    #[serde(default)]
    pub allow: Vec<String>,
}

/// 推荐资源（只声明，不强制）
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resources {
    /// 建议内存（MB）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended_memory_mb: Option<u32>,
    /// 建议 CPU 占用（百分比）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended_cpu_percent: Option<u8>,
}

/// 发布者信息（市场审核用）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Publisher {
    /// 名称
    pub name: String,
    /// 主页
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// 安装来源（安装时写入，插件自己声明的值不被信任）
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceInfo {
    /// 来源种类：`local` / `url` / `git`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// 原始位置
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// git commit
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// 产物摘要
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// manifest 原始结构
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// schema 版本
    pub schema: u32,
    /// 插件 id
    pub id: String,
    /// 显示名
    pub name: String,
    /// 描述
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 版本（SemVer）
    pub version: String,
    /// 插件 API 主版本
    pub api: u32,
    /// 入口
    pub entry: EntrySpec,
    /// 作用域，缺省 `machine`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// 是否常驻
    #[serde(default)]
    pub resident: bool,
    /// 声明能力
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// 出站白名单
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net: Option<NetSpec>,
    /// 订阅
    #[serde(default)]
    pub subscriptions: Vec<String>,
    /// 提供契约
    #[serde(default)]
    pub provides: Vec<String>,
    /// 消费契约
    #[serde(default)]
    pub consumes: Vec<String>,
    /// 推荐资源
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Resources>,
    /// 发布者
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<Publisher>,
    /// SPDX 标识
    pub license: String,
    /// 安装来源
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceInfo>,
}

/// 校验通过的 manifest：闭集已解析，后续层直接用
#[derive(Debug, Clone)]
pub struct ValidatedManifest {
    /// 原始结构（供展示与写记录）
    pub raw: Manifest,
    /// 解析后的 id
    pub id: PluginId,
    /// 作用域
    pub scope: PluginScope,
    /// 能力集合（排序去重）
    pub capabilities: BTreeSet<Capability>,
    /// 订阅集合
    pub subscriptions: BTreeSet<Subscription>,
    /// 提供的契约
    pub provides: BTreeSet<ContractId>,
    /// 消费的契约
    pub consumes: BTreeSet<ContractId>,
}

impl ValidatedManifest {
    /// 是否存在中高风险能力（决定安装后的初始状态）
    #[must_use]
    pub fn needs_approval(&self) -> bool {
        self.capabilities
            .iter()
            .any(|cap| cap.risk().needs_approval())
    }

    /// 订阅是否已具备所需能力
    #[must_use]
    pub fn subscriptions_without_capability(&self) -> Vec<Subscription> {
        self.subscriptions
            .iter()
            .copied()
            .filter(|kind| !self.capabilities.contains(&kind.required_capability()))
            .collect()
    }
}

impl Manifest {
    /// 解析并校验 JSON
    ///
    /// # Errors
    ///
    /// JSON 非法、未知字段、字段越界或闭集校验失败时返回 [`ManifestError`]
    pub fn from_json(bytes: &[u8]) -> Result<ValidatedManifest, ManifestError> {
        let value: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|err| ManifestError::Json(err.to_string()))?;
        Self::from_value(&value)
    }

    /// 从已解析的 JSON 值校验
    ///
    /// # Errors
    ///
    /// 同 [`Self::from_json`]
    pub fn from_value(value: &serde_json::Value) -> Result<ValidatedManifest, ManifestError> {
        let object = value
            .as_object()
            .ok_or_else(|| ManifestError::InvalidField {
                field: "<root>".to_owned(),
                reason: "manifest must be a JSON object".to_owned(),
            })?;
        for key in object.keys() {
            if !TOP_LEVEL_FIELDS.contains(&key.as_str()) {
                return Err(ManifestError::UnknownField { field: key.clone() });
            }
        }
        let manifest: Manifest =
            serde_json::from_value(value.clone()).map_err(|err| ManifestError::InvalidField {
                field: field_from_serde(&err),
                reason: err.to_string(),
            })?;
        manifest.validate()
    }

    /// 校验已构造的结构
    ///
    /// # Errors
    ///
    /// 同 [`Self::from_json`]
    #[allow(clippy::too_many_lines)] // 字段逐项校验就是这份顺序清单本身
    pub fn validate(self) -> Result<ValidatedManifest, ManifestError> {
        if self.schema != MANIFEST_SCHEMA_VERSION {
            return Err(ManifestError::UnsupportedSchema {
                found: self.schema,
                supported: MANIFEST_SCHEMA_VERSION,
            });
        }
        if self.api != PLUGIN_API_VERSION {
            return Err(ManifestError::UnsupportedApi {
                found: self.api,
                supported: PLUGIN_API_VERSION,
            });
        }
        let id = PluginId::parse(&self.id)?;
        if !semver_is_valid(&self.version) {
            return Err(ManifestError::InvalidVersion {
                raw: self.version.clone(),
            });
        }
        if self.name.is_empty() || self.name.chars().count() > MAX_NAME_CHARS {
            return Err(ManifestError::InvalidField {
                field: "name".to_owned(),
                reason: format!("must be 1..={MAX_NAME_CHARS} characters"),
            });
        }
        if let Some(description) = &self.description
            && description.chars().count() > MAX_DESCRIPTION_CHARS
        {
            return Err(ManifestError::InvalidField {
                field: "description".to_owned(),
                reason: format!("must be at most {MAX_DESCRIPTION_CHARS} characters"),
            });
        }
        if self.entry.kind != "process" {
            return Err(ManifestError::UnsupportedEntry {
                raw: self.entry.kind.clone(),
            });
        }
        self.entry.validate_cmd()?;
        for (platform, cmd) in &self.entry.platforms {
            if platform.is_empty() {
                return Err(ManifestError::InvalidEntry {
                    reason: "empty platform key".to_owned(),
                });
            }
            validate_relative_path(cmd)?;
        }
        if self.capabilities.len() > MAX_CAPABILITIES {
            return Err(ManifestError::InvalidField {
                field: "capabilities".to_owned(),
                reason: format!("at most {MAX_CAPABILITIES} entries"),
            });
        }
        if self.subscriptions.len() > MAX_SUBSCRIPTIONS {
            return Err(ManifestError::InvalidField {
                field: "subscriptions".to_owned(),
                reason: format!("at most {MAX_SUBSCRIPTIONS} entries"),
            });
        }
        let capabilities = capability::parse_all(&self.capabilities)?;
        if let Some(closed) = capability::closed_declared(&capabilities).first() {
            return Err(ManifestError::UnsupportedCapability {
                capability: *closed,
            });
        }
        let subscriptions = event::parse_all(&self.subscriptions)?;
        if !subscriptions.is_empty() && !capabilities.contains(&Capability::Events) {
            return Err(ManifestError::MissingCapability {
                capability: Capability::Events,
                reason: "non-empty subscriptions".to_owned(),
            });
        }
        for kind in &subscriptions {
            if !capabilities.contains(&kind.required_capability()) {
                return Err(ManifestError::MissingCapability {
                    capability: kind.required_capability(),
                    reason: format!("subscription {kind}"),
                });
            }
        }
        let provides = contract::parse_all(&self.provides)?;
        let consumes = contract::parse_all(&self.consumes)?;
        for contract in &provides {
            let missing = contract::missing_capabilities(contract.contract(), &capabilities);
            if let Some(capability) = missing.first() {
                return Err(ManifestError::MissingCapability {
                    capability: *capability,
                    reason: format!("provides {contract}"),
                });
            }
        }
        let scope = match &self.scope {
            Some(raw) => PluginScope::parse(raw)?,
            None => PluginScope::Machine,
        };
        if scope == PluginScope::Server
            && let Some(contract) = provides
                .iter()
                .find(|id| id.contract().requires_machine_scope())
        {
            return Err(ManifestError::ScopeConflict {
                detail: format!("{contract} requires machine scope"),
            });
        }
        if let Some(net) = &self.net {
            if !net.allow.is_empty() && !capabilities.contains(&Capability::NetOutbound) {
                return Err(ManifestError::MissingCapability {
                    capability: Capability::NetOutbound,
                    reason: "net.allow".to_owned(),
                });
            }
            for host in &net.allow {
                if !host_is_valid(host) {
                    return Err(ManifestError::InvalidField {
                        field: "net.allow".to_owned(),
                        reason: format!("invalid host: {host:?}"),
                    });
                }
            }
        }
        if let Some(resources) = &self.resources {
            if let Some(memory) = resources.recommended_memory_mb
                && !(1..=MAX_RECOMMENDED_MEMORY_MB).contains(&memory)
            {
                return Err(ManifestError::InvalidResource {
                    field: "recommended_memory_mb".to_owned(),
                    value: memory.to_string(),
                });
            }
            if let Some(cpu) = resources.recommended_cpu_percent
                && !(1..=100).contains(&cpu)
            {
                return Err(ManifestError::InvalidResource {
                    field: "recommended_cpu_percent".to_owned(),
                    value: cpu.to_string(),
                });
            }
        }
        if !spdx_is_valid(&self.license) {
            return Err(ManifestError::InvalidField {
                field: "license".to_owned(),
                reason: "must be an SPDX identifier".to_owned(),
            });
        }
        Ok(ValidatedManifest {
            raw: self,
            id,
            scope,
            capabilities,
            subscriptions,
            provides,
            consumes,
        })
    }
}

fn field_from_serde(err: &serde_json::Error) -> String {
    // serde_json 的报错带行号，取不到字段名时用整条消息；调用方只需可读。
    let text = err.to_string();
    for marker in ["missing field `", "unknown field `"] {
        if let Some(rest) = text.split(marker).nth(1)
            && let Some(field) = rest.split('`').next()
        {
            return field.to_owned();
        }
    }
    "<manifest>".to_owned()
}

/// 相对路径校验：拒绝绝对路径、`..`、盘符与反斜杠
fn validate_relative_path(raw: &str) -> Result<(), ManifestError> {
    let reason = if raw.is_empty() {
        Some("must not be empty")
    } else if raw.starts_with('/') || raw.starts_with('\\') {
        Some("must be relative")
    } else if raw.as_bytes().get(1) == Some(&b':') {
        Some("must not contain a drive prefix")
    } else if raw.contains('\\') {
        Some("must use forward slashes")
    } else if Path::new(raw).components().any(|c| {
        matches!(
            c,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        )
    }) {
        Some("must not escape the artifact root")
    } else {
        None
    };
    match reason {
        Some(reason) => Err(ManifestError::InvalidEntry {
            reason: format!("{raw:?}: {reason}"),
        }),
        None => Ok(()),
    }
}

/// 主机名或后缀：小写字母、数字、点、连字符；不含 scheme 与路径
fn host_is_valid(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && !host.starts_with('.')
        && !host.ends_with('.')
        && host
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
}

/// SPDX 标识：字母、数字、`.`、`+`、`-`，长度 1..=64
fn spdx_is_valid(license: &str) -> bool {
    !license.is_empty()
        && license.len() <= 64
        && license
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'+' || b == b'-')
}

/// 严格 `SemVer` 2.0.0 校验（不含 `v` 前缀）
#[must_use]
pub fn semver_is_valid(raw: &str) -> bool {
    let (rest, build) = match raw.split_once('+') {
        Some((rest, build)) => (rest, Some(build)),
        None => (raw, None),
    };
    let (core, pre) = match rest.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (rest, None),
    };
    let mut parts = core.split('.');
    let (Some(major), Some(minor), Some(patch), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    for part in [major, minor, patch] {
        if !numeric_identifier(part) {
            return false;
        }
    }
    if let Some(pre) = pre {
        if pre.is_empty() {
            return false;
        }
        for ident in pre.split('.') {
            if ident.is_empty()
                || !ident
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            {
                return false;
            }
            if ident.bytes().all(|b| b.is_ascii_digit()) && !numeric_identifier(ident) {
                return false;
            }
        }
    }
    if let Some(build) = build {
        if build.is_empty() {
            return false;
        }
        for ident in build.split('.') {
            if ident.is_empty()
                || !ident
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            {
                return false;
            }
        }
    }
    true
}

fn numeric_identifier(part: &str) -> bool {
    !part.is_empty()
        && part.bytes().all(|b| b.is_ascii_digit())
        && (part == "0" || !part.starts_with('0'))
}

/// manifest 校验失败
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ManifestError {
    /// 不是合法 JSON
    #[error("manifest is not valid JSON: {0}")]
    Json(String),
    /// 未知顶层字段
    #[error("unknown field: {field}")]
    UnknownField {
        /// 字段名
        field: String,
    },
    /// 字段形状或取值非法
    #[error("invalid field {field}: {reason}")]
    InvalidField {
        /// 字段名
        field: String,
        /// 原因
        reason: String,
    },
    /// 插件 id 非法
    #[error(transparent)]
    Id(#[from] InvalidPluginId),
    /// 版本不是 `SemVer`
    #[error("invalid version: {raw}")]
    InvalidVersion {
        /// 原始值
        raw: String,
    },
    /// manifest schema 不匹配
    #[error("unsupported manifest schema {found}, host supports {supported}")]
    UnsupportedSchema {
        /// 插件声明
        found: u32,
        /// 宿主支持
        supported: u32,
    },
    /// 插件 API 不匹配
    #[error("unsupported plugin api {found}, host supports {supported}")]
    UnsupportedApi {
        /// 插件声明
        found: u32,
        /// 宿主支持
        supported: u32,
    },
    /// 入口路径非法
    #[error("invalid entry: {reason}")]
    InvalidEntry {
        /// 原因
        reason: String,
    },
    /// 入口形态不支持
    #[error("unsupported entry type: {raw}")]
    UnsupportedEntry {
        /// 原始值
        raw: String,
    },
    /// 未知能力
    #[error(transparent)]
    Capability(#[from] capability::UnknownCapability),
    /// v1 不开放的能力
    #[error("capability {capability} is not open in this host")]
    UnsupportedCapability {
        /// 能力
        capability: Capability,
    },
    /// 未知订阅
    #[error(transparent)]
    Subscription(#[from] event::UnknownSubscription),
    /// 未知契约
    #[error(transparent)]
    Contract(#[from] contract::ContractError),
    /// 缺少声明所需能力
    #[error("missing capability {capability} required by {reason}")]
    MissingCapability {
        /// 缺失能力
        capability: Capability,
        /// 触发校验的声明
        reason: String,
    },
    /// 作用域冲突
    #[error("scope conflict: {detail}")]
    ScopeConflict {
        /// 细节
        detail: String,
    },
    /// 资源声明越界
    #[error("invalid resource {field} = {value}")]
    InvalidResource {
        /// 字段名
        field: String,
        /// 原始值
        value: String,
    },
    /// 未知作用域
    #[error(transparent)]
    Scope(#[from] UnknownScope),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn minimal() -> serde_json::Value {
        json!({
            "schema": 1,
            "id": "demo",
            "name": "Demo",
            "version": "0.1.0",
            "api": 1,
            "entry": { "type": "process", "cmd": "bin/demo" },
            "capabilities": ["events"],
            "license": "MIT"
        })
    }

    fn with(
        mut value: serde_json::Value,
        key: &str,
        replacement: serde_json::Value,
    ) -> serde_json::Value {
        value[key] = replacement;
        value
    }

    #[test]
    fn minimal_manifest_validates() {
        let validated = Manifest::from_value(&minimal()).expect("valid");
        assert_eq!(validated.id.as_str(), "demo");
        assert_eq!(validated.scope, PluginScope::Machine);
        assert_eq!(
            validated.capabilities,
            [Capability::Events].into_iter().collect()
        );
        assert!(!validated.needs_approval());
    }

    /// 入口文件检查：存在 + 平台形态（unix 可执行位 / Windows exe + PE 头）
    #[test]
    fn entry_file_check_requires_a_real_executable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let entry = EntrySpec {
            kind: "process".to_owned(),
            cmd: "bin/demo".to_owned(),
            platforms: BTreeMap::from([("win32-x64".to_owned(), "bin/demo.exe".to_owned())]),
        };

        // 文件缺失
        let err = entry
            .check_entry_file(dir.path(), "linux-x64")
            .expect_err("missing");
        assert!(err.to_string().contains("missing"), "{err}");

        // 存在但不可执行（unix）
        std::fs::create_dir_all(dir.path().join("bin")).expect("bin");
        std::fs::write(dir.path().join("bin/demo"), b"text").expect("entry");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(
                dir.path().join("bin/demo"),
                std::fs::Permissions::from_mode(0o644),
            )
            .expect("chmod");
            let err = entry
                .check_entry_file(dir.path(), "linux-x64")
                .expect_err("not executable");
            assert!(err.to_string().contains("not executable"), "{err}");
        }

        // 平台覆盖的命令缺失
        let err = entry
            .check_entry_file(dir.path(), "win32-x64")
            .expect_err("missing override");
        assert!(err.to_string().contains("missing"), "{err}");

        // 未声明平台时回退默认命令（Windows 语义在下面单独验证）
        #[cfg(not(windows))]
        let fallback = EntrySpec {
            kind: "process".to_owned(),
            cmd: "bin/demo".to_owned(),
            platforms: BTreeMap::new(),
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(
                dir.path().join("bin/demo"),
                std::fs::Permissions::from_mode(0o755),
            )
            .expect("chmod");
        }
        #[cfg(not(windows))]
        {
            fallback
                .check_entry_file(dir.path(), "linux-x64")
                .expect("ok on unix");
        }

        // Windows：.exe 需 PE 头，.cmd/.bat 放行
        #[cfg(windows)]
        {
            // 扩展名不是 exe
            let wrong_ext = EntrySpec {
                kind: "process".to_owned(),
                cmd: "bin/demo".to_owned(),
                platforms: BTreeMap::new(),
            };
            let err = wrong_ext
                .check_entry_file(dir.path(), "win32-x64")
                .expect_err("not .exe");
            assert!(err.to_string().contains(".exe"), "{err}");

            // exe 存在但没有 MZ 头
            std::fs::write(dir.path().join("bin/demo.exe"), b"plain text").expect("exe");
            let err = entry
                .check_entry_file(dir.path(), "win32-x64")
                .expect_err("no PE header");
            assert!(err.to_string().contains("PE header"), "{err}");

            // 都满足
            std::fs::write(dir.path().join("bin/demo.exe"), b"MZ\x90payload").expect("exe");
            entry
                .check_entry_file(dir.path(), "win32-x64")
                .expect("valid exe");

            // 脚本型插件：.cmd / .bat 不需要 PE 头
            let scripted = EntrySpec {
                kind: "process".to_owned(),
                cmd: "bin/run.cmd".to_owned(),
                platforms: BTreeMap::from([("win32-x64".to_owned(), "bin/run.cmd".to_owned())]),
            };
            std::fs::write(
                dir.path().join("bin/run.cmd"),
                b"@echo off\r\nnode entry.mjs %*\r\n",
            )
            .expect("cmd");
            scripted
                .check_entry_file(dir.path(), "win32-x64")
                .expect("valid cmd");
            std::fs::write(dir.path().join("bin/run.bat"), b"@echo off\r\n").expect("bat");
            let bat = EntrySpec {
                kind: "process".to_owned(),
                cmd: "bin/run.bat".to_owned(),
                platforms: BTreeMap::new(),
            };
            bat.check_entry_file(dir.path(), "win32-x64")
                .expect("valid bat");
        }
    }

    #[test]
    fn unknown_top_level_field_is_rejected() {
        let value = with(minimal(), "sandbox", json!(true));
        assert_eq!(
            Manifest::from_value(&value).expect_err("unknown"),
            ManifestError::UnknownField {
                field: "sandbox".to_owned()
            }
        );
    }

    #[test]
    fn reserved_and_invalid_ids_are_rejected() {
        for raw in ["mcsup", "Core", "chat_bridge", "1chat"] {
            let value = with(minimal(), "id", json!(raw));
            assert!(Manifest::from_value(&value).is_err(), "{raw}");
        }
    }

    #[test]
    fn schema_and_api_mismatch_are_rejected() {
        let schema = with(minimal(), "schema", json!(2));
        assert!(matches!(
            Manifest::from_value(&schema),
            Err(ManifestError::UnsupportedSchema { .. })
        ));
        let api = with(minimal(), "api", json!(2));
        assert!(matches!(
            Manifest::from_value(&api),
            Err(ManifestError::UnsupportedApi { .. })
        ));
    }

    #[test]
    fn invalid_versions_are_rejected() {
        for raw in [
            "1", "1.2", "01.2.3", "1.2.3-", "1.2.3+", "v1.2.3", "1.2.3-01",
        ] {
            let value = with(minimal(), "version", json!(raw));
            assert!(Manifest::from_value(&value).is_err(), "{raw}");
        }
        for raw in ["1.2.3", "0.0.1", "1.2.3-rc.1", "1.2.3+build.5"] {
            let value = with(minimal(), "version", json!(raw));
            assert!(Manifest::from_value(&value).is_ok(), "{raw}");
        }
    }

    #[test]
    fn entry_must_be_process_and_relative() {
        let mut value = minimal();
        value["entry"] = json!({ "type": "wasm", "cmd": "bin/demo" });
        assert!(matches!(
            Manifest::from_value(&value),
            Err(ManifestError::UnsupportedEntry { .. })
        ));

        for cmd in ["/usr/bin/demo", "bin/../demo", "C:\\demo", "bin\\demo"] {
            let mut value = minimal();
            value["entry"] = json!({ "type": "process", "cmd": cmd });
            assert!(Manifest::from_value(&value).is_err(), "{cmd}");
        }
    }

    #[test]
    fn unknown_capability_and_closed_capability_are_rejected() {
        let unknown = with(minimal(), "capabilities", json!(["sandbox"]));
        assert!(Manifest::from_value(&unknown).is_err());

        let closed = with(minimal(), "capabilities", json!(["process:spawn"]));
        assert!(matches!(
            Manifest::from_value(&closed),
            Err(ManifestError::UnsupportedCapability { .. })
        ));
    }

    #[test]
    fn subscription_requires_events_and_console_read() {
        let mut value = minimal();
        value["subscriptions"] = json!(["chat:message"]);
        assert!(matches!(
            Manifest::from_value(&value),
            Err(ManifestError::MissingCapability {
                capability: Capability::ConsoleRead,
                ..
            })
        ));

        let mut value = minimal();
        value["capabilities"] = json!([]);
        value["subscriptions"] = json!(["server:crashed"]);
        assert!(matches!(
            Manifest::from_value(&value),
            Err(ManifestError::MissingCapability {
                capability: Capability::Events,
                ..
            })
        ));
    }

    #[test]
    fn provides_requires_capabilities() {
        let mut value = minimal();
        value["capabilities"] = json!(["events"]);
        value["provides"] = json!(["chat.publish@1"]);
        assert!(matches!(
            Manifest::from_value(&value),
            Err(ManifestError::MissingCapability {
                capability: Capability::ConsoleWrite,
                ..
            })
        ));
    }

    #[test]
    fn net_allow_requires_net_outbound() {
        let mut value = minimal();
        value["net"] = json!({ "allow": ["api.telegram.org"] });
        assert!(matches!(
            Manifest::from_value(&value),
            Err(ManifestError::MissingCapability {
                capability: Capability::NetOutbound,
                ..
            })
        ));

        let mut value = minimal();
        value["capabilities"] = json!(["events", "net:outbound"]);
        value["net"] = json!({ "allow": ["https://api.telegram.org"] });
        assert!(Manifest::from_value(&value).is_err());
    }

    #[test]
    fn server_scope_cannot_provide_agent_converse() {
        let mut value = minimal();
        value["scope"] = json!("server");
        value["capabilities"] = json!(["events", "net:outbound"]);
        value["provides"] = json!(["agent.converse@1"]);
        assert!(matches!(
            Manifest::from_value(&value),
            Err(ManifestError::ScopeConflict { .. })
        ));
    }

    #[test]
    fn resources_are_range_checked() {
        let mut value = minimal();
        value["resources"] = json!({ "recommended_memory_mb": 0 });
        assert!(matches!(
            Manifest::from_value(&value),
            Err(ManifestError::InvalidResource { .. })
        ));
        value["resources"] =
            json!({ "recommended_memory_mb": 65536, "recommended_cpu_percent": 10 });
        assert!(Manifest::from_value(&value).is_ok());
    }

    #[test]
    fn approval_is_driven_by_capability_risk() {
        let mut value = minimal();
        value["capabilities"] = json!(["events", "console:read"]);
        assert!(
            Manifest::from_value(&value)
                .expect("valid")
                .needs_approval()
        );
        assert!(
            !Manifest::from_value(&minimal())
                .expect("valid")
                .needs_approval()
        );
    }
}
