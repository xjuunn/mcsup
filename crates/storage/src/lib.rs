mod models;
mod schema;

use crate::models::{JarChangeset, JavaRuntimeRow, RelinkChangeset, ServerRow, WorkspaceRow};
use crate::schema::{java_runtimes, java_scan_state, servers, workspaces};
use diesel::connection::SimpleConnection;
use diesel::prelude::*;
use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};
use domain::java::JavaRuntime;
use domain::server::meta::{ServerMeta, ServerName};
use domain::workspace::{Workspace, WorkspaceId, WorkspaceName};
use port::{JavaRuntimeRepository, RepositoryError, ServerRepository, WorkspaceRepository};
use std::path::Path;
use std::sync::{Mutex, PoisonError};

/// 编译进二进制的迁移集；`migrations/` 位于本 crate 根
pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!();
const BUSY_TIMEOUT_MS: u64 = 5_000;

/// SQLite 实现的元数据库
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
             PRAGMA busy_timeout = {BUSY_TIMEOUT_MS};
             PRAGMA foreign_keys = ON;"
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

impl WorkspaceRepository for SqliteStore {
    fn create(&self, ws: &Workspace) -> Result<(), RepositoryError> {
        self.with_conn(|conn| {
            let row = WorkspaceRow::from(ws);
            diesel::insert_into(workspaces::table)
                .values(&row)
                .execute(conn)
                .map_err(|err| map_insert_error(err, ws.name.as_str(), "workspaces"))?;
            Ok(())
        })
    }

    fn get(&self, id: &WorkspaceId) -> Result<Workspace, RepositoryError> {
        self.with_conn(|conn| {
            let row: Option<WorkspaceRow> = workspaces::table
                .filter(workspaces::id.eq(id.as_str()))
                .select(WorkspaceRow::as_select())
                .first(conn)
                .optional()
                .map_err(storage_err)?;
            let row = row.ok_or_else(|| RepositoryError::NotFound(id.as_str().to_owned()))?;
            Workspace::try_from(row)
        })
    }

    fn get_by_name(&self, name: &WorkspaceName) -> Result<Workspace, RepositoryError> {
        self.with_conn(|conn| {
            let row: Option<WorkspaceRow> = workspaces::table
                .filter(workspaces::name.eq(name.as_str()))
                .select(WorkspaceRow::as_select())
                .first(conn)
                .optional()
                .map_err(storage_err)?;
            let row = row.ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))?;
            Workspace::try_from(row)
        })
    }

    fn list(&self) -> Result<Vec<Workspace>, RepositoryError> {
        self.with_conn(|conn| {
            let row = workspaces::table
                .order(workspaces::name)
                .select(WorkspaceRow::as_select())
                .load::<WorkspaceRow>(conn)
                .map_err(storage_err)?;
            row.into_iter().map(Workspace::try_from).collect()
        })
    }

    fn default(&self) -> Result<Option<Workspace>, RepositoryError> {
        self.with_conn(|conn| {
            let row: Option<WorkspaceRow> = workspaces::table
                .filter(workspaces::is_default.eq(true))
                .select(WorkspaceRow::as_select())
                .first(conn)
                .optional()
                .map_err(storage_err)?;
            row.map(Workspace::try_from).transpose()
        })
    }

    fn set_default(&self, id: &WorkspaceId) -> Result<(), RepositoryError> {
        self.with_conn(|conn| {
            conn.transaction(|conn| {
                diesel::update(workspaces::table.filter(workspaces::is_default.eq(true)))
                    .set(workspaces::is_default.eq(false))
                    .execute(conn)?;
                let affected =
                    diesel::update(workspaces::table.filter(workspaces::id.eq(id.as_str())))
                        .set(workspaces::is_default.eq(true))
                        .execute(conn)?;
                if affected == 0 {
                    return Err(diesel::result::Error::NotFound);
                }
                Ok(())
            })
            .map_err(|err| match err {
                diesel::result::Error::NotFound => {
                    RepositoryError::NotFound(id.as_str().to_owned())
                }
                other => storage_err(other),
            })
        })
    }

    fn remove(&self, id: &WorkspaceId) -> Result<(), RepositoryError> {
        self.with_conn(|conn| {
            let affected = diesel::delete(workspaces::table.filter(workspaces::id.eq(id.as_str())))
                .execute(conn)
                .map_err(storage_err)?;
            if affected == 0 {
                return Err(RepositoryError::NotFound(id.as_str().to_owned()));
            }
            Ok(())
        })
    }

    fn relink_path(
        &self,
        id: &WorkspaceId,
        path: &Path,
        updated_at: i64,
    ) -> Result<(), RepositoryError> {
        self.with_conn(|conn| {
            let affected = diesel::update(workspaces::table.filter(workspaces::id.eq(id.as_str())))
                .set((
                    workspaces::path.eq(path.to_string_lossy().into_owned()),
                    workspaces::updated_at.eq(updated_at),
                ))
                .execute(conn)
                .map_err(storage_err)?;
            if affected == 0 {
                return Err(RepositoryError::NotFound(id.as_str().to_owned()));
            }
            Ok(())
        })
    }
}

impl ServerRepository for SqliteStore {
    fn create(&self, meta: &ServerMeta) -> Result<(), RepositoryError> {
        self.with_conn(|conn| {
            let row = ServerRow::from(meta);
            diesel::insert_into(servers::table)
                .values(&row)
                .execute(conn)
                .map_err(|err| map_insert_error(err, meta.name.as_str(), "servers"))?;
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

    fn remove_by_workspace(&self, workspace_id: &WorkspaceId) -> Result<(), RepositoryError> {
        self.with_conn(|conn| {
            diesel::delete(servers::table.filter(servers::workspace_id.eq(workspace_id.as_str())))
                .execute(conn)
                .map_err(storage_err)?;
            Ok(())
        })
    }

    fn relink(
        &self,
        name: &ServerName,
        workspace_id: &WorkspaceId,
        rel_dir: &Path,
        updated_at: i64,
    ) -> Result<ServerMeta, RepositoryError> {
        self.with_conn(|conn| {
            let changeset = RelinkChangeset {
                workspace_id: workspace_id.as_str().to_owned(),
                rel_dir: rel_dir.to_string_lossy().into_owned(),
                updated_at,
            };
            let affected = diesel::update(servers::table.filter(servers::name.eq(name.as_str())))
                .set(&changeset)
                .execute(conn)
                .map_err(|err| map_insert_error(err, name.as_str(), "servers"))?;
            if affected == 0 {
                return Err(RepositoryError::NotFound(name.as_str().to_owned()));
            }

            select_meta(conn, name)
        })
    }
}

impl JavaRuntimeRepository for SqliteStore {
    fn upsert(&self, runtime: &JavaRuntime) -> Result<(), RepositoryError> {
        self.with_conn(|conn| {
            let row = JavaRuntimeRow::from(runtime);
            diesel::insert_into(java_runtimes::table)
                .values(&row)
                .on_conflict(java_runtimes::path)
                .do_update()
                .set((
                    java_runtimes::major.eq(row.major),
                    java_runtimes::version.eq(&row.version),
                    java_runtimes::vendor.eq(&row.vendor),
                    java_runtimes::managed.eq(row.managed),
                    java_runtimes::updated_at.eq(row.updated_at),
                ))
                .execute(conn)
                .map_err(storage_err)?;
            Ok(())
        })
    }

    fn list(&self) -> Result<Vec<JavaRuntime>, RepositoryError> {
        self.with_conn(|conn| {
            let row = java_runtimes::table
                .order((java_runtimes::major.desc(), java_runtimes::path))
                .select(JavaRuntimeRow::as_select())
                .load::<JavaRuntimeRow>(conn)
                .map_err(storage_err)?;
            row.into_iter().map(JavaRuntime::try_from).collect()
        })
    }

    fn get_by_path(&self, path: &Path) -> Result<JavaRuntime, RepositoryError> {
        self.with_conn(|conn| {
            let row: Option<JavaRuntimeRow> = java_runtimes::table
                .filter(java_runtimes::path.eq(path.to_string_lossy().into_owned()))
                .select(JavaRuntimeRow::as_select())
                .first(conn)
                .optional()
                .map_err(storage_err)?;
            let row =
                row.ok_or_else(|| RepositoryError::NotFound(path.to_string_lossy().into_owned()))?;
            JavaRuntime::try_from(row)
        })
    }

    fn remove(&self, path: &Path) -> Result<(), RepositoryError> {
        self.with_conn(|conn| {
            let affected = diesel::delete(
                java_runtimes::table
                    .filter(java_runtimes::path.eq(path.to_string_lossy().into_owned())),
            )
            .execute(conn)
            .map_err(storage_err)?;
            if affected == 0 {
                return Err(RepositoryError::NotFound(
                    path.to_string_lossy().into_owned(),
                ));
            }
            Ok(())
        })
    }

    fn scan_state(&self) -> Result<Option<i64>, RepositoryError> {
        self.with_conn(|conn| {
            let at: Option<i64> = java_scan_state::table
                .filter(java_scan_state::key.eq("scanner"))
                .select(java_scan_state::scanned_at)
                .first(conn)
                .optional()
                .map_err(storage_err)?;
            Ok(at)
        })
    }

    fn mark_scanned(&self, at: i64) -> Result<(), RepositoryError> {
        self.with_conn(|conn| {
            diesel::insert_into(java_scan_state::table)
                .values((
                    java_scan_state::key.eq("scanner"),
                    java_scan_state::scanned_at.eq(at),
                ))
                .on_conflict(java_scan_state::key)
                .do_update()
                .set(java_scan_state::scanned_at.eq(at))
                .execute(conn)
                .map_err(storage_err)?;
            Ok(())
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

fn map_insert_error(err: diesel::result::Error, key: &str, table: &str) -> RepositoryError {
    if let diesel::result::Error::DatabaseError(kind, info) = &err
        && *kind == diesel::result::DatabaseErrorKind::UniqueViolation
    {
        let message = info.message();
        if message.contains(table) {
            return RepositoryError::AlreadyExists(key.to_owned());
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
    use domain::server::meta::{InstalledJar, ServerId};
    use domain::server::version::{ServerVariant, VersionId};
    use std::path::PathBuf;

    fn sample_ws(name: &str, path: &str, id: &str) -> Workspace {
        Workspace {
            id: WorkspaceId::parse(id).expect("uuid"),
            name: WorkspaceName::parse(name).expect("valid"),
            path: PathBuf::from(path),
            is_default: false,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
        }
    }

    fn sample_meta(name: &str) -> ServerMeta {
        let mut id = *b"550e8400-e29b-41d4-a716-????????????";
        let mut hash: u32 = 0;
        for b in name.bytes() {
            hash = hash.wrapping_mul(31).wrapping_add(u32::from(b));
        }
        let hex = format!("{hash:012x}");
        id[24..].copy_from_slice(hex.as_bytes());
        let id = std::str::from_utf8(&id).expect("ascii");
        ServerMeta {
            id: ServerId::parse(id).expect("uuid"),
            name: ServerName::parse(name).expect("valid"),
            workspace_id: WorkspaceId::parse(WS01).expect("uuid"),
            rel_dir: PathBuf::from(name),
            version: VersionId::new("1.21.1"),
            variant: ServerVariant::Vanilla,
            jar: None,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
        }
    }

    const WS01: &str = "550e8400-e29b-41d4-a716-446655440001";
    const WS02: &str = "550e8400-e29b-41d4-a716-446655440002";

    fn add_ws(store: &SqliteStore, ws: &Workspace) {
        WorkspaceRepository::create(store, ws).expect("ws");
    }

    fn add_server(store: &SqliteStore, meta: &ServerMeta) {
        ServerRepository::create(store, meta).expect("server");
    }

    fn store_with_ws() -> SqliteStore {
        let store = SqliteStore::open_in_memory().expect("open");
        add_ws(&store, &sample_ws("ws01", "/tmp/ws01", WS01));
        store
    }

    #[test]
    fn create_get_roundtrip() {
        let store = store_with_ws();
        add_server(&store, &sample_meta("alpha"));
        let got = ServerRepository::get(&store, &ServerName::parse("alpha").expect("valid"))
            .expect("get");
        assert_eq!(got, sample_meta("alpha"));
    }

    #[test]
    fn duplicate_name_rejected() {
        let store = store_with_ws();
        add_server(&store, &sample_meta("alpha"));
        let err = ServerRepository::create(&store, &sample_meta("alpha")).expect_err("dup");
        assert!(matches!(err, RepositoryError::AlreadyExists(_)));
    }

    #[test]
    fn update_jar_persists_and_bumps_timestamp() {
        let store = store_with_ws();
        add_server(&store, &sample_meta("alpha"));
        let jar = InstalledJar {
            path: PathBuf::from("server.jar"),
            size: 42,
            checksum: Checksum::new(ChecksumAlgorithm::Sha1, "a".repeat(40)),
        };
        let updated = ServerRepository::update_jar(
            &store,
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
        let store = store_with_ws();
        add_server(&store, &sample_meta("beta"));
        add_server(&store, &sample_meta("alpha"));
        let names: Vec<_> = ServerRepository::list(&store)
            .expect("list")
            .into_iter()
            .map(|m| m.name.as_str().to_owned())
            .collect();
        assert_eq!(names, ["alpha", "beta"]);
    }

    #[test]
    fn missing_server_is_not_found() {
        let store = store_with_ws();
        let err = ServerRepository::get(&store, &ServerName::parse("ghost").expect("valid"))
            .expect_err("missing");
        assert!(matches!(err, RepositoryError::NotFound(_)));
    }

    #[test]
    fn workspace_default_is_single() {
        let store = SqliteStore::open_in_memory().expect("open");
        add_ws(&store, &sample_ws("ws01", "/tmp/ws01", WS01));
        add_ws(&store, &sample_ws("ws02", "/tmp/ws02", WS02));
        let id01 = WorkspaceId::parse(WS01).expect("uuid");
        let id02 = WorkspaceId::parse(WS02).expect("uuid");
        WorkspaceRepository::set_default(&store, &id01).expect("default 1");
        WorkspaceRepository::set_default(&store, &id02).expect("default 2");
        let defaults: Vec<_> = WorkspaceRepository::list(&store)
            .expect("list")
            .into_iter()
            .filter(|w| w.is_default)
            .collect();
        assert_eq!(defaults.len(), 1);
        assert_eq!(defaults[0].name.as_str(), "ws02");
    }

    #[test]
    fn remove_by_workspace_clears_servers() {
        let store = store_with_ws();
        add_server(&store, &sample_meta("alpha"));
        add_server(&store, &sample_meta("beta"));
        let ws_id = WorkspaceId::parse(WS01).expect("uuid");
        ServerRepository::remove_by_workspace(&store, &ws_id).expect("clear");
        assert!(ServerRepository::list(&store).expect("list").is_empty());
    }

    #[test]
    fn relink_moves_server() {
        let store = SqliteStore::open_in_memory().expect("open");
        add_ws(&store, &sample_ws("ws01", "/tmp/ws01", WS01));
        add_ws(&store, &sample_ws("ws02", "/tmp/ws02", WS02));
        add_server(&store, &sample_meta("alpha"));
        let ws_id02 = WorkspaceId::parse(WS02).expect("uuid");
        let moved = ServerRepository::relink(
            &store,
            &ServerName::parse("alpha").expect("valid"),
            &ws_id02,
            Path::new("alpha"),
            1_700_000_200,
        )
        .expect("relink");
        assert_eq!(moved.workspace_id, ws_id02);
        assert_eq!(moved.rel_dir, PathBuf::from("alpha"));
    }

    #[test]
    fn relink_missing_is_not_found() {
        let store = store_with_ws();
        let ws_id = WorkspaceId::parse(WS01).expect("uuid");
        let err = ServerRepository::relink(
            &store,
            &ServerName::parse("ghost").expect("valid"),
            &ws_id,
            Path::new("ghost"),
            0,
        )
        .expect_err("missing");
        assert!(matches!(err, RepositoryError::NotFound(_)));
    }

    fn sample_java(path: &str, major: i64, managed: bool) -> JavaRuntime {
        JavaRuntime {
            id: format!("java-{path}"),
            path: PathBuf::from(path),
            major: domain::java::JavaMajor::parse(major).expect("major"),
            version: format!("{major}.0.9+9"),
            vendor: domain::java::JavaVendor::Temurin,
            managed,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
        }
    }

    #[test]
    fn java_upsert_and_list_orders_by_major_desc() {
        let store = SqliteStore::open_in_memory().expect("open");
        JavaRuntimeRepository::upsert(&store, &sample_java("/opt/jdk17", 17, false)).expect("17");
        JavaRuntimeRepository::upsert(&store, &sample_java("/opt/jdk21", 21, true)).expect("21");
        let list = JavaRuntimeRepository::list(&store).expect("list");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].major.as_i64(), 21);
        assert_eq!(list[1].major.as_i64(), 17);
    }

    #[test]
    fn java_upsert_same_path_updates_in_place() {
        let store = SqliteStore::open_in_memory().expect("open");
        JavaRuntimeRepository::upsert(&store, &sample_java("/opt/jdk17", 17, false))
            .expect("first");
        let mut updated = sample_java("/opt/jdk17", 17, true);
        updated.version = "17.0.11+9".to_owned();
        updated.updated_at = 1_700_000_100;
        JavaRuntimeRepository::upsert(&store, &updated).expect("second");
        let list = JavaRuntimeRepository::list(&store).expect("list");
        assert_eq!(list.len(), 1);
        assert!(list[0].managed);
        assert_eq!(list[0].version, "17.0.11+9");
    }

    #[test]
    fn java_remove_missing_is_not_found() {
        let store = SqliteStore::open_in_memory().expect("open");
        let err = JavaRuntimeRepository::remove(&store, Path::new("/ghost")).expect_err("missing");
        assert!(matches!(err, RepositoryError::NotFound(_)));
    }

    #[test]
    fn java_scan_state_roundtrip() {
        let store = SqliteStore::open_in_memory().expect("open");
        assert_eq!(
            JavaRuntimeRepository::scan_state(&store).expect("state"),
            None
        );
        JavaRuntimeRepository::mark_scanned(&store, 1_700_000_000).expect("mark");
        assert_eq!(
            JavaRuntimeRepository::scan_state(&store).expect("state"),
            Some(1_700_000_000)
        );
        JavaRuntimeRepository::mark_scanned(&store, 1_700_000_200).expect("remark");
        assert_eq!(
            JavaRuntimeRepository::scan_state(&store).expect("state"),
            Some(1_700_000_200)
        );
    }
}
