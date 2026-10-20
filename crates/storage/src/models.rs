use diesel::prelude::*;
use domain::java::{JavaMajor, JavaRuntime, JavaVendor};
use domain::server::artifact::{Checksum, ChecksumAlgorithm};
use domain::server::meta::{InstalledJar, ServerId, ServerMeta, ServerName};
use domain::server::version::{ServerVariant, VersionId};
use domain::workspace::{Workspace, WorkspaceId, WorkspaceName};
use port::RepositoryError;
use std::path::PathBuf;

use crate::schema::{java_runtimes, servers, workspaces};

/// 行字段解析失败 → [`RepositoryError::Corrupt`]，统一 `{字段} {值}: {err}` 形状
///
/// Corrupt 消息是给排查者看的，形状散在各处容易漏掉值或字段名。
fn corrupt(
    label: &str,
    value: impl std::fmt::Display,
    err: impl std::fmt::Display,
) -> RepositoryError {
    RepositoryError::Corrupt(format!("{label} {value}: {err}"))
}

/// `workspaces` 表的一行
#[derive(Debug, Clone, Queryable, Selectable, Insertable)]
#[diesel(table_name = workspaces)]
pub(crate) struct WorkspaceRow {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) is_default: bool,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
}

/// `servers` 表的一行
#[derive(Debug, Clone, Queryable, Selectable, Insertable)]
#[diesel(table_name = servers)]
pub(crate) struct ServerRow {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) workspace_id: String,
    pub(crate) rel_dir: String,
    pub(crate) version: String,
    pub(crate) variant: String,
    pub(crate) jar_path: Option<String>,
    pub(crate) jar_size: Option<i64>,
    pub(crate) checksum_algo: Option<String>,
    pub(crate) checksum: Option<String>,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
}

/// 核心元数据变更集：只更新这几列，不动身份 / 归属 / `created_at`
#[derive(Debug, AsChangeset)]
#[diesel(table_name = servers)]
pub(crate) struct JarChangeset {
    pub(crate) jar_path: String,
    pub(crate) jar_size: i64,
    pub(crate) checksum_algo: String,
    pub(crate) checksum: String,
    pub(crate) updated_at: i64,
}

/// 移动归属变更集
#[derive(Debug, AsChangeset)]
#[diesel(table_name = servers)]
pub(crate) struct RelinkChangeset {
    pub(crate) workspace_id: String,
    pub(crate) rel_dir: String,
    pub(crate) updated_at: i64,
}

/// `java_runtimes` 表的一行
#[derive(Debug, Clone, Queryable, Selectable, Insertable)]
#[diesel(table_name = java_runtimes)]
pub(crate) struct JavaRuntimeRow {
    pub(crate) id: String,
    pub(crate) path: String,
    pub(crate) major: i64,
    pub(crate) version: String,
    pub(crate) vendor: String,
    pub(crate) managed: bool,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
}

impl From<&Workspace> for WorkspaceRow {
    fn from(ws: &Workspace) -> Self {
        Self {
            id: ws.id.as_str().to_owned(),
            name: ws.name.as_str().to_owned(),
            path: ws.path.to_string_lossy().into_owned(),
            is_default: ws.is_default,
            created_at: ws.created_at,
            updated_at: ws.updated_at,
        }
    }
}

impl TryFrom<WorkspaceRow> for Workspace {
    type Error = RepositoryError;

    fn try_from(row: WorkspaceRow) -> Result<Self, Self::Error> {
        let id =
            WorkspaceId::parse(&row.id).map_err(|err| corrupt("workspace id", &row.id, err))?;
        let name = WorkspaceName::parse(&row.name)
            .map_err(|err| corrupt("workspace name", &row.name, err))?;
        Ok(Self {
            id,
            name,
            path: PathBuf::from(row.path),
            is_default: row.is_default,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

impl From<&ServerMeta> for ServerRow {
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
        }
    }
}

impl TryFrom<ServerRow> for ServerMeta {
    type Error = RepositoryError;

    fn try_from(row: ServerRow) -> Result<Self, Self::Error> {
        let id = ServerId::parse(&row.id).map_err(|err| corrupt("server id", &row.id, err))?;
        let name = ServerName::parse(&row.name).map_err(|err| corrupt("name", &row.name, err))?;
        let workspace_id = WorkspaceId::parse(&row.workspace_id)
            .map_err(|err| corrupt("workspace_id", &row.workspace_id, err))?;
        let variant = ServerVariant::parse(&row.variant)
            .map_err(|err| corrupt("variant", &row.variant, err))?;
        let jar = match (row.jar_path, row.jar_size, row.checksum_algo, row.checksum) {
            (Some(path), Some(size), Some(algo), Some(hex)) => {
                let algorithm = ChecksumAlgorithm::parse(&algo)
                    .map_err(|err| corrupt("checksum algo", &algo, err))?;
                Some(InstalledJar {
                    path: PathBuf::from(path),
                    size: u64::try_from(size).unwrap_or(u64::MAX),
                    checksum: Checksum::new(algorithm, hex),
                })
            }
            (None, None, None, None) => None,
            _ => {
                return Err(RepositoryError::Corrupt(format!(
                    "partial jar columns of {}",
                    row.name
                )));
            }
        };

        Ok(Self {
            id,
            name,
            workspace_id,
            rel_dir: PathBuf::from(row.rel_dir),
            version: VersionId::new(row.version),
            variant,
            jar,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

impl From<&JavaRuntime> for JavaRuntimeRow {
    fn from(rt: &JavaRuntime) -> Self {
        Self {
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

impl TryFrom<JavaRuntimeRow> for JavaRuntime {
    type Error = RepositoryError;

    fn try_from(row: JavaRuntimeRow) -> Result<Self, Self::Error> {
        let major =
            JavaMajor::parse(row.major).map_err(|err| corrupt("java major", row.major, err))?;
        Ok(Self {
            id: row.id,
            path: PathBuf::from(row.path),
            major,
            version: row.version,
            vendor: JavaVendor::parse_fuzzy(&row.vendor),
            managed: row.managed,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}
