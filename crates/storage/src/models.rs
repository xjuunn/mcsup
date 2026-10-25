use domain::java::{JavaMajor, JavaRuntime, JavaVendor};
use domain::server::artifact::{Checksum, ChecksumAlgorithm};
use domain::server::meta::{InstalledJar, ServerId, ServerMeta, ServerName};
use domain::server::settings::ServerSettings;
use domain::server::version::{ServerVariant, VersionId};
use domain::workspace::{Workspace, WorkspaceId};
use port::RepositoryError;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 字段解析失败 → [`RepositoryError::Corrupt`]，统一 `{字段} {值}: {err}` 形状
///
/// Corrupt 消息是给排查者看的，形状散在各处容易漏掉值或字段名。
pub(crate) fn corrupt(
    label: &str,
    value: impl std::fmt::Display,
    err: impl std::fmt::Display,
) -> RepositoryError {
    RepositoryError::Corrupt(format!("{label} {value}: {err}"))
}

/// 每条记录的格式版本
///
/// 一条记录一个文件，版本随记录存放：将来可以逐条升级，不必重写全部数据。
pub(crate) const RECORD_VERSION: u32 = 1;

/// 记录文件的一项：工作目录
///
/// 「当前工作区」不在这里：它是全局唯一指针，存 `state.json`。记录内
/// 保存该布尔值会让单值语义分散到 n 个文件，写入必须改两个文件才一致。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WorkspaceEntry {
    pub(crate) version: u32,
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
}

/// 注册表的一项：服务器
///
/// `jar_*` 四列同生同灭：要么全是 `None`，要么全部有值，与旧库的
/// 「partial jar columns」判据保持一致。`java_major` 是版本要求，
/// `settings.java_major` 是用户绑定，两者不可混用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ServerEntry {
    pub(crate) schema: u32,
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) workspace_id: String,
    pub(crate) rel_dir: String,
    pub(crate) version: String,
    pub(crate) variant: String,
    #[serde(default)]
    pub(crate) jar_path: Option<String>,
    #[serde(default)]
    pub(crate) jar_size: Option<i64>,
    #[serde(default)]
    pub(crate) checksum_algo: Option<String>,
    #[serde(default)]
    pub(crate) checksum: Option<String>,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
    #[serde(default)]
    pub(crate) java_major: Option<i64>,
    #[serde(default)]
    pub(crate) required_java_major: Option<i64>,
    #[serde(default)]
    pub(crate) ram_mib: Option<i32>,
    #[serde(default)]
    pub(crate) autostart: bool,
}

/// 记录文件的一项：Java 运行时
///
/// 字段 `version` 是 JDK 版本号，格式版本因此用 `schema`，避免同名冲突。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct JavaRuntimeEntry {
    pub(crate) schema: u32,
    pub(crate) id: String,
    pub(crate) path: String,
    pub(crate) major: i64,
    pub(crate) version: String,
    pub(crate) vendor: String,
    #[serde(default)]
    pub(crate) managed: bool,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
}

impl From<&Workspace> for WorkspaceEntry {
    fn from(ws: &Workspace) -> Self {
        Self {
            version: RECORD_VERSION,
            id: ws.id.as_str().to_owned(),
            name: ws.name.as_str().to_owned(),
            path: ws.path.to_string_lossy().into_owned(),
            created_at: ws.created_at,
            updated_at: ws.updated_at,
        }
    }
}

// 记录 → 领域对象的转换在 `lib.rs` 的 `into_workspace` 里做：
// 记录文件不含当前标记，`is_current` 必须由 `state.json` 决定，
// 因此这里不提供会产出语义不完整中间态的 `TryFrom`。

impl From<&ServerMeta> for ServerEntry {
    fn from(meta: &ServerMeta) -> Self {
        let (jar_path, jar_size, checksum_algo, checksum) = match &meta.jar {
            Some(jar) => (
                Some(jar.path.to_string_lossy().into_owned()),
                Some(i64::try_from(jar.size).unwrap_or(i64::MAX)),
                Some(jar.checksum.algorithm().as_str().to_owned()),
                Some(jar.checksum.hex().to_owned()),
            ),
            None => (None, None, None, None),
        };
        Self {
            schema: RECORD_VERSION,
            id: meta.id.as_str().to_owned(),
            name: meta.name.as_str().to_owned(),
            workspace_id: meta.workspace_id.as_str().to_owned(),
            rel_dir: meta.rel_dir.to_string_lossy().into_owned(),
            version: meta.version.as_str().to_owned(),
            variant: meta.variant.as_str().to_owned(),
            jar_path,
            jar_size,
            checksum_algo,
            checksum,
            created_at: meta.created_at,
            updated_at: meta.updated_at,
            java_major: meta.settings.java_major.map(JavaMajor::as_i64),
            required_java_major: meta.java_major.map(JavaMajor::as_i64),
            ram_mib: meta
                .settings
                .ram_mib
                .and_then(|mib| i32::try_from(mib).ok()),
            autostart: meta.settings.autostart,
        }
    }
}

impl TryFrom<ServerEntry> for ServerMeta {
    type Error = RepositoryError;

    fn try_from(entry: ServerEntry) -> Result<Self, Self::Error> {
        let id = ServerId::parse(&entry.id).map_err(|err| corrupt("server id", &entry.id, err))?;
        let name =
            ServerName::parse(&entry.name).map_err(|err| corrupt("name", &entry.name, err))?;
        let workspace_id = WorkspaceId::parse(&entry.workspace_id)
            .map_err(|err| corrupt("workspace_id", &entry.workspace_id, err))?;
        let variant = ServerVariant::parse(&entry.variant)
            .map_err(|err| corrupt("variant", &entry.variant, err))?;
        let jar = match (
            entry.jar_path,
            entry.jar_size,
            entry.checksum_algo,
            entry.checksum,
        ) {
            (Some(path), Some(size), Some(algo), Some(hex)) => {
                let algorithm = ChecksumAlgorithm::parse(&algo)
                    .map_err(|err| corrupt("checksum algo", &algo, err))?;
                // 空 hex 是「上游未提供校验和」的落盘形态（如 Fabric），
                // 重建时必须还原为未校验，不能当成真实哈希去比对
                let checksum = if hex.is_empty() {
                    Checksum::unverified()
                } else {
                    Checksum::new(algorithm, hex)
                };
                Some(InstalledJar {
                    path: PathBuf::from(path),
                    size: u64::try_from(size).unwrap_or(u64::MAX),
                    checksum,
                })
            }
            (None, None, None, None) => None,
            _ => {
                return Err(RepositoryError::Corrupt(format!(
                    "partial jar columns of {}",
                    entry.name
                )));
            }
        };

        let java_major = match entry.java_major {
            Some(raw) => {
                Some(JavaMajor::parse(raw).map_err(|err| corrupt("java major", raw, err))?)
            }
            None => None,
        };
        let required_java_major = match entry.required_java_major {
            Some(raw) => Some(
                JavaMajor::parse(raw).map_err(|err| corrupt("required java major", raw, err))?,
            ),
            None => None,
        };
        let ram_mib = match entry.ram_mib {
            Some(raw) => Some(u32::try_from(raw).map_err(|err| corrupt("ram_mib", raw, err))?),
            None => None,
        };
        let settings = ServerSettings {
            java_major,
            ram_mib,
            autostart: entry.autostart,
        };

        Ok(Self {
            id,
            name,
            workspace_id,
            rel_dir: PathBuf::from(entry.rel_dir),
            version: VersionId::new(entry.version),
            variant,
            jar,
            settings,
            java_major: required_java_major,
            created_at: entry.created_at,
            updated_at: entry.updated_at,
        })
    }
}

impl From<&JavaRuntime> for JavaRuntimeEntry {
    fn from(rt: &JavaRuntime) -> Self {
        Self {
            schema: RECORD_VERSION,
            id: rt.id.clone(),
            path: rt.path.to_string_lossy().into_owned(),
            major: rt.major.as_i64(),
            version: rt.version.clone(),
            vendor: rt.vendor.as_str().to_owned(),
            managed: rt.managed,
            created_at: rt.created_at,
            updated_at: rt.updated_at,
        }
    }
}

impl TryFrom<JavaRuntimeEntry> for JavaRuntime {
    type Error = RepositoryError;

    fn try_from(entry: JavaRuntimeEntry) -> Result<Self, Self::Error> {
        let major =
            JavaMajor::parse(entry.major).map_err(|err| corrupt("java major", entry.major, err))?;
        Ok(Self {
            id: entry.id,
            path: PathBuf::from(entry.path),
            major,
            version: entry.version,
            vendor: JavaVendor::parse_fuzzy(&entry.vendor),
            managed: entry.managed,
            created_at: entry.created_at,
            updated_at: entry.updated_at,
        })
    }
}
