use diesel::prelude::*;
use domain::server::artifact::{Checksum, ChecksumAlgorithm};
use domain::server::meta::{InstalledJar, ServerMeta, ServerName};
use domain::server::version::{ServerVariant, VersionId};
use port::RepositoryError;
use std::path::PathBuf;

use crate::schema::servers::{self};

/// `server` 表的一行
#[derive(Debug, Clone, Queryable, Selectable, Insertable)]
#[diesel(table_name = servers)]
pub(crate) struct ServerRow {
    pub(crate) name: String,
    pub(crate) dir: String,
    pub(crate) version: String,
    pub(crate) variant: String,
    pub(crate) jar_path: Option<String>,
    pub(crate) jar_size: Option<i64>,
    pub(crate) checksum_algo: Option<String>,
    pub(crate) checksum: Option<String>,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
}

/// 核心元数据变更集：只更新这几列，不动 `name` / `dir`/ `created_at`
#[derive(Debug, AsChangeset)]
#[diesel(table_name = servers)]
pub(crate) struct JarChangeset {
    pub(crate) jar_path: String,
    pub(crate) jar_size: i64,
    pub(crate) checksum_algo: String,
    pub(crate) checksum: String,
    pub(crate) updated_at: i64,
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
            name: meta.name.as_str().to_owned(),
            dir: meta.dir.to_string_lossy().into_owned(),
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
        let name = ServerName::parse(&row.name)
            .map_err(|err| RepositoryError::Corrupt(format!("name {}: {err}", row.name)))?;
        let variant = ServerVariant::parse(&row.variant)
            .map_err(|err| RepositoryError::Corrupt(format!("variant {}: {err}", row.variant)))?;
        let jar = match (row.jar_path, row.jar_size, row.checksum_algo, row.checksum) {
            (Some(path), Some(size), Some(algo), Some(hex)) => {
                let algorithm = ChecksumAlgorithm::parse(&algo).map_err(|err| {
                    RepositoryError::Corrupt(format!("checksum algo {algo}: {err}"))
                })?;
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
            name,
            dir: PathBuf::from(row.dir),
            version: VersionId::new(row.version),
            variant,
            jar,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}
