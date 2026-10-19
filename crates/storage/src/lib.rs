mod models;
mod schema;

use crate::models::{JarChangeset, ServerRow};
use crate::schema::servers;
use diesel::connection::SimpleConnection;
use diesel::prelude::*;
use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};
use domain::server::meta::{ServerMeta, ServerName};
use port::{RepositoryError, ServerRepository};
use std::path::Path;
use std::sync::{Mutex, PoisonError};

/// 编译进二进制的迁移集；`migrations/` 位于本 crate 根
pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!();
const BUSY_TIMEOUT_MS: u64 = 5_000;

/// SQLite 实现的服务元数据库
pub struct SqliteStore {
    conn: Mutex<SqliteConnection>,
}

impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteStore").finish_non_exhaustive()
    }
}

impl SqliteStore {
    /// 打开或创建数据库文件并执行待应用的迁移
    ///
    /// # Errors
    ///
    /// 创建目录、打开连接、PRAGMA 或迁移失败时返回 [`RepositoryError`]
    pub fn open(path: &Path) -> Result<Self, RepositoryError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(storage_err)?;
        }
        let url = path.to_string_lossy().into_owned();
        let conn = SqliteConnection::establish(&url).map_err(storage_err)?;
        Self::from_conn(conn)
    }

    /// 内存库：测试与临时查询用
    ///
    /// # Errors
    ///
    /// 同 [`SqliteStore::open`]
    pub fn open_in_memory() -> Result<Self, RepositoryError> {
        let conn = SqliteConnection::establish(":memory:").map_err(storage_err)?;
        Self::from_conn(conn)
    }

    fn from_conn(mut conn: SqliteConnection) -> Result<Self, RepositoryError> {
        conn.batch_execute(&format!(
            "PRAGMA journal_mode = WAL;
             PRAGMA busy_timeout = {BUSY_TIMEOUT_MS};"
        ))
        .map_err(storage_err)?;
        conn.run_pending_migrations(MIGRATIONS)
            .map_err(RepositoryError::Storage)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// 取锁后在连接上执行一段操作
    ///
    /// 内部组合（如 `update_jar` 之后回读）必须走同一个闭包共享连接，
    /// 禁止方法间重入 `self`——`std::sync::Mutex` 不可重入，会死锁
    fn with_conn<T>(
        &self,
        f: impl FnOnce(&mut SqliteConnection) -> Result<T, RepositoryError>,
    ) -> Result<T, RepositoryError> {
        let mut conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut conn)
    }
}

impl ServerRepository for SqliteStore {
    fn create(&self, meta: &ServerMeta) -> Result<(), RepositoryError> {
        self.with_conn(|conn| {
            let row = ServerRow::from(meta);
            diesel::insert_into(servers::table)
                .values(&row)
                .execute(conn)
                .map_err(|err| map_insert_error(err, meta.name.as_str()))?;
            Ok(())
        })
    }

    fn get(&self, name: &ServerName) -> Result<ServerMeta, RepositoryError> {
        self.with_conn(|conn| select_meta(conn, name))
    }

    fn list(&self) -> Result<Vec<ServerMeta>, RepositoryError> {
        self.with_conn(|conn| {
            let row = servers::table
                .order(servers::name)
                .select(ServerRow::as_select())
                .load::<ServerRow>(conn)
                .map_err(storage_err)?;
            row.into_iter().map(ServerMeta::try_from).collect()
        })
    }

    fn update_jar(
        &self,
        name: &ServerName,
        jar: &domain::server::meta::InstalledJar,
        updated_at: i64,
    ) -> Result<ServerMeta, RepositoryError> {
        self.with_conn(|conn| {
            let changeset = JarChangeset {
                jar_path: jar.path.to_string_lossy().into_owned(),
                jar_size: i64::try_from(jar.size).unwrap_or(i64::MAX),
                checksum_algo: jar.checksum.algorithm().as_str().to_owned(),
                checksum: jar.checksum.hex().to_owned(),
                updated_at,
            };
            let affected = diesel::update(servers::table.filter(servers::name.eq(name.as_str())))
                .set(&changeset)
                .execute(conn)
                .map_err(storage_err)?;
            if affected == 0 {
                return Err(RepositoryError::NotFound(name.as_str().to_owned()));
            }

            select_meta(conn, name)
        })
    }
}

fn select_meta(
    conn: &mut SqliteConnection,
    name: &ServerName,
) -> Result<ServerMeta, RepositoryError> {
    let row: Option<ServerRow> = servers::table
        .filter(servers::name.eq(name.as_str()))
        .select(ServerRow::as_select())
        .first(conn)
        .optional()
        .map_err(storage_err)?;
    let row = row.ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))?;
    ServerMeta::try_from(row)
}

fn map_insert_error(err: diesel::result::Error, name: &str) -> RepositoryError {
    if let diesel::result::Error::DatabaseError(kind, info) = &err
        && *kind == diesel::result::DatabaseErrorKind::UniqueViolation
    {
        let message = info.message();
        if message.contains("servers.name") || message.contains("servers.dir") {
            return RepositoryError::AlreadyExists(name.to_owned());
        }
    }
    storage_err(err)
}

fn storage_err(err: impl std::error::Error + Send + Sync + 'static) -> RepositoryError {
    RepositoryError::Storage(Box::new(err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::server::artifact::{Checksum, ChecksumAlgorithm};
    use domain::server::meta::InstalledJar;
    use domain::server::version::{ServerVariant, VersionId};
    use std::path::PathBuf;

    fn sample_meta(name: &str) -> ServerMeta {
        ServerMeta {
            name: ServerName::parse(name).expect("valid name"),
            dir: PathBuf::from(format!("/tmp/{name}")),
            version: VersionId::new("1.21.1"),
            variant: ServerVariant::Vanilla,
            jar: None,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
        }
    }

    #[test]
    fn create_get_roundtrip() {
        let store = SqliteStore::open_in_memory().expect("open");
        store.create(&sample_meta("alpha")).expect("create");
        let got = store
            .get(&ServerName::parse("alpha").expect("valid"))
            .expect("get");
        assert_eq!(got, sample_meta("alpha"));
    }

    #[test]
    fn duplicate_name_rejected() {
        let store = SqliteStore::open_in_memory().expect("open");
        store.create(&sample_meta("alpha")).expect("create");
        let err = store.create(&sample_meta("alpha")).expect_err("dup");
        assert!(matches!(err, RepositoryError::AlreadyExists(_)));
    }

    #[test]
    fn update_jar_persists_and_bumps_timestamp() {
        let store = SqliteStore::open_in_memory().expect("open");
        store.create(&sample_meta("alpha")).expect("create");
        let jar = InstalledJar {
            path: PathBuf::from("/tmp/alpha/server.jar"),
            size: 42,
            checksum: Checksum::new(ChecksumAlgorithm::Sha1, "a".repeat(40)),
        };
        let updated = store
            .update_jar(
                &ServerName::parse("alpha").expect("valid"),
                &jar,
                1_700_000_100,
            )
            .expect("update");
        assert_eq!(updated.jar, Some(jar));
        assert_eq!(updated.updated_at, 1_700_000_100);
    }

    #[test]
    fn list_orders_by_name() {
        let store = SqliteStore::open_in_memory().expect("open");
        store.create(&sample_meta("beta")).expect("create");
        store.create(&sample_meta("alpha")).expect("create");
        let names: Vec<_> = store
            .list()
            .expect("list")
            .into_iter()
            .map(|m| m.name.as_str().to_owned())
            .collect();
        assert_eq!(names, ["alpha", "beta"]);
    }

    #[test]
    fn missing_server_is_not_found() {
        let store = SqliteStore::open_in_memory().expect("open");
        let err = store
            .get(&ServerName::parse("ghost").expect("valid"))
            .expect_err("missing");
        assert!(matches!(err, RepositoryError::NotFound(_)));
    }
}
