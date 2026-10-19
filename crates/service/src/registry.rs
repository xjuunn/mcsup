use domain::server::meta::{InstalledJar, ServerId, ServerMeta, ServerName};
use domain::server::version::{ServerVariant, VersionId};
use domain::workspace::Workspace;
use port::{RepositoryError, ServerRepository, WorkspaceRepository};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

use crate::workspace::WorkspaceError;

const SERVER_MARKER: &str = "mcsup.json";
const SERVER_MARKER_SCHEMA: u32 = 1;

/// 服务器注册操作失败
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ServerRegistryError {
    /// 工作目录相关失败
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    /// 名称冲突（全局或目录已存在）
    #[error("server already exists: {0}")]
    AlreadyExists(String),
    /// 服务器不存在
    #[error("server not found: {0}")]
    NotFound(String),
    /// 文件系统失败
    /// 文件系统失败；原因走 source 链，避免 `{:#}` 渲染时重复
    #[error("io error on {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// marker 解析 / 序列化失败
    #[error("server marker {path}: {message}")]
    Marker { path: PathBuf, message: String },
    /// 注册表失败
    #[error(transparent)]
    Repo(#[from] RepositoryError),
    /// 名称非法
    #[error(transparent)]
    InvalidName(#[from] domain::server::error::InvalidServerName),
}

impl ServerRegistryError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// `mcsup.json` 载荷
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ServerMarkerJson {
    schema: u32,
    id: String,
    name: String,
}

/// 已注册服务器 + 绝对目录（路径 = workspace.path / servers / `rel_dir`）
#[derive(Debug, Clone)]
pub struct RegisteredServer {
    pub meta: ServerMeta,
    pub dir: PathBuf,
}

/// 服务器注册表编排：归属工作目录、目录创建、marker、注册
pub struct ServerRegistry {
    servers: Arc<dyn ServerRepository>,
    workspaces: Arc<dyn WorkspaceRepository>,
}

impl std::fmt::Debug for ServerRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerRegistry").finish_non_exhaustive()
    }
}

impl ServerRegistry {
    /// 组合两个注册表
    #[must_use]
    pub fn new(
        servers: Arc<dyn ServerRepository>,
        workspaces: Arc<dyn WorkspaceRepository>,
    ) -> Self {
        Self {
            servers,
            workspaces,
        }
    }

    /// 在所选工作目录下创建服务器：目录 + `mcsup.json` + 入库
    ///
    /// # Errors
    ///
    /// 名称全局冲突、工作目录不存在、IO / marker / 注册表失败时返回
    /// [`ServerRegistryError`]
    pub fn create(
        &self,
        workspace_selector: Option<&str>,
        name: &str,
        version: &VersionId,
        variant: ServerVariant,
    ) -> Result<RegisteredServer, ServerRegistryError> {
        let now = now_unix();
        let name = ServerName::parse(name)?;
        let ws = self.resolve_workspace(workspace_selector)?;

        if self.servers.get(&name).is_ok() {
            return Err(ServerRegistryError::AlreadyExists(name.as_str().to_owned()));
        }

        let dir = ws.path.join("servers").join(name.as_str());
        if dir.exists() {
            return Err(ServerRegistryError::AlreadyExists(name.as_str().to_owned()));
        }
        std::fs::create_dir_all(&dir).map_err(|err| ServerRegistryError::io(&dir, err))?;

        let id = ServerId::parse(&uuid::Uuid::new_v4().to_string()).map_err(|err| {
            ServerRegistryError::Marker {
                path: dir.join(SERVER_MARKER),
                message: err.to_string(),
            }
        })?;
        write_server_marker(
            &dir,
            &ServerMarkerJson {
                schema: SERVER_MARKER_SCHEMA,
                id: id.as_str().to_owned(),
                name: name.as_str().to_owned(),
            },
        )?;

        let rel_dir = PathBuf::from(name.as_str());
        let meta = ServerMeta {
            id,
            name,
            workspace_id: ws.id.clone(),
            rel_dir,
            version: version.clone(),
            variant,
            jar: None,
            created_at: now,
            updated_at: now,
        };
        self.servers.create(&meta)?;
        Ok(RegisteredServer { meta, dir })
    }

    /// 全部服务器，按名称升序，附绝对目录
    ///
    /// # Errors
    ///
    /// 注册表失败或路径无法拼接时返回 [`ServerRegistryError`]
    pub fn list(&self) -> Result<Vec<RegisteredServer>, ServerRegistryError> {
        let mut out = Vec::new();
        for meta in self.servers.list()? {
            let dir = self.absolute_dir(&meta)?;
            out.push(RegisteredServer { meta, dir });
        }
        Ok(out)
    }

    /// 按名解析服务器
    ///
    /// # Errors
    ///
    /// 不存在或工作目录缺失时返回 [`ServerRegistryError`]
    pub fn get(&self, name: &str) -> Result<RegisteredServer, ServerRegistryError> {
        let name =
            ServerName::parse(name).map_err(|_| ServerRegistryError::NotFound(name.to_owned()))?;
        let meta = self
            .servers
            .get(&name)
            .map_err(|_| ServerRegistryError::NotFound(name.as_str().to_owned()))?;
        let dir = self.absolute_dir(&meta)?;
        Ok(RegisteredServer { meta, dir })
    }

    /// 回写核心元数据（安装完成后调用）
    ///
    /// # Errors
    ///
    /// 不存在或注册表失败时返回 [`ServerRegistryError`]
    pub fn update_jar(
        &self,
        name: &str,
        jar: &InstalledJar,
        updated_at: i64,
    ) -> Result<RegisteredServer, ServerRegistryError> {
        let name =
            ServerName::parse(name).map_err(|_| ServerRegistryError::NotFound(name.to_owned()))?;
        let meta = self.servers.update_jar(&name, jar, updated_at)?;
        let dir = self.absolute_dir(&meta)?;
        Ok(RegisteredServer { meta, dir })
    }

    fn resolve_workspace(&self, selector: Option<&str>) -> Result<Workspace, ServerRegistryError> {
        match selector {
            Some(sel) => {
                let all = self.workspaces.list()?;
                if let Some(exact) = all.iter().find(|w| w.name.as_str() == sel) {
                    return Ok(exact.clone());
                }
                let matched: Vec<&Workspace> = all
                    .iter()
                    .filter(|w| w.name.as_str().starts_with(sel))
                    .collect();
                match matched.as_slice() {
                    [one] => Ok((*one).clone()),
                    [] => Err(ServerRegistryError::Workspace(WorkspaceError::NotFound(
                        sel.to_owned(),
                    ))),
                    many => Err(ServerRegistryError::Workspace(WorkspaceError::Ambiguous {
                        selector: sel.to_owned(),
                        candidates: many.iter().map(|w| w.name.as_str().to_owned()).collect(),
                    })),
                }
            }
            None => self
                .workspaces
                .default()?
                .ok_or_else(|| ServerRegistryError::Workspace(WorkspaceError::NoDefault)),
        }
    }

    fn absolute_dir(&self, meta: &ServerMeta) -> Result<PathBuf, ServerRegistryError> {
        let ws = self
            .workspaces
            .get(&meta.workspace_id)
            .map_err(|_| ServerRegistryError::Workspace(WorkspaceError::NoDefault))?;
        Ok(ws.path.join("servers").join(&meta.rel_dir))
    }
}

fn write_server_marker(dir: &Path, marker: &ServerMarkerJson) -> Result<(), ServerRegistryError> {
    let path = dir.join(SERVER_MARKER);
    let text = serde_json::to_string_pretty(marker).map_err(|err| ServerRegistryError::Marker {
        path: path.clone(),
        message: err.to_string(),
    })?;
    std::fs::write(&path, text).map_err(|err| ServerRegistryError::io(&path, err))
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().cast_signed())
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::workspace::{Workspace, WorkspaceId, WorkspaceName};
    use port::{ServerRepository, WorkspaceRepository};
    use std::path::Path;

    struct MemServers {
        items: std::sync::Mutex<Vec<ServerMeta>>,
    }

    impl MemServers {
        fn new() -> Self {
            Self {
                items: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl ServerRepository for MemServers {
        fn create(&self, meta: &ServerMeta) -> Result<(), RepositoryError> {
            let mut items = self.items.lock().expect("lock");
            if items.iter().any(|m| m.name == meta.name) {
                return Err(RepositoryError::AlreadyExists(
                    meta.name.as_str().to_owned(),
                ));
            }
            items.push(meta.clone());
            Ok(())
        }
        fn get(&self, name: &ServerName) -> Result<ServerMeta, RepositoryError> {
            self.items
                .lock()
                .expect("lock")
                .iter()
                .find(|m| &m.name == name)
                .cloned()
                .ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))
        }
        fn list(&self) -> Result<Vec<ServerMeta>, RepositoryError> {
            let mut items = self.items.lock().expect("lock").clone();
            items.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(items)
        }
        fn update_jar(
            &self,
            name: &ServerName,
            jar: &InstalledJar,
            updated_at: i64,
        ) -> Result<ServerMeta, RepositoryError> {
            let mut items = self.items.lock().expect("lock");
            let meta = items
                .iter_mut()
                .find(|m| &m.name == name)
                .ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))?;
            meta.jar = Some(jar.clone());
            meta.updated_at = updated_at;
            Ok(meta.clone())
        }
        fn remove_by_workspace(&self, _workspace_id: &WorkspaceId) -> Result<(), RepositoryError> {
            Ok(())
        }
        fn relink(
            &self,
            _name: &ServerName,
            _workspace_id: &WorkspaceId,
            _rel_dir: &Path,
            _updated_at: i64,
        ) -> Result<ServerMeta, RepositoryError> {
            unreachable!("not used in tests")
        }
    }

    struct MemWorkspaces {
        items: std::sync::Mutex<Vec<Workspace>>,
    }

    impl MemWorkspaces {
        fn new(ws: Workspace) -> Self {
            Self {
                items: std::sync::Mutex::new(vec![ws]),
            }
        }
    }

    impl WorkspaceRepository for MemWorkspaces {
        fn create(&self, ws: &Workspace) -> Result<(), RepositoryError> {
            self.items.lock().expect("lock").push(ws.clone());
            Ok(())
        }
        fn get(&self, id: &WorkspaceId) -> Result<Workspace, RepositoryError> {
            self.items
                .lock()
                .expect("lock")
                .iter()
                .find(|w| &w.id == id)
                .cloned()
                .ok_or_else(|| RepositoryError::NotFound(id.as_str().to_owned()))
        }
        fn get_by_name(&self, name: &WorkspaceName) -> Result<Workspace, RepositoryError> {
            self.items
                .lock()
                .expect("lock")
                .iter()
                .find(|w| &w.name == name)
                .cloned()
                .ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))
        }
        fn list(&self) -> Result<Vec<Workspace>, RepositoryError> {
            Ok(self.items.lock().expect("lock").clone())
        }
        fn default(&self) -> Result<Option<Workspace>, RepositoryError> {
            Ok(self
                .items
                .lock()
                .expect("lock")
                .iter()
                .find(|w| w.is_default)
                .cloned())
        }
        fn set_default(&self, _id: &WorkspaceId) -> Result<(), RepositoryError> {
            Ok(())
        }
        fn remove(&self, _id: &WorkspaceId) -> Result<(), RepositoryError> {
            Ok(())
        }
        fn relink_path(
            &self,
            _id: &WorkspaceId,
            _path: &Path,
            _updated_at: i64,
        ) -> Result<(), RepositoryError> {
            Ok(())
        }
    }

    fn ws_fixture(root: &Path) -> Workspace {
        let path = root.join("ws");
        std::fs::create_dir_all(path.join("servers")).expect("mkdir");
        Workspace {
            id: WorkspaceId::parse("550e8400-e29b-41d4-a716-446655440001").expect("uuid"),
            name: WorkspaceName::parse("ws01").expect("valid"),
            path,
            is_default: true,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
        }
    }

    #[test]
    fn create_writes_marker_and_registers() {
        let tmp = tempfile::tempdir().expect("tmp");
        let ws = ws_fixture(tmp.path());
        let reg = ServerRegistry::new(
            Arc::new(MemServers::new()),
            Arc::new(MemWorkspaces::new(ws)),
        );
        let out = reg
            .create(
                None,
                "alpha",
                &VersionId::new("1.21.1"),
                ServerVariant::Vanilla,
            )
            .expect("create");
        assert_eq!(out.meta.name.as_str(), "alpha");
        assert!(out.dir.join("mcsup.json").is_file());
        let got = reg.get("alpha").expect("get");
        assert_eq!(got.meta.id, out.meta.id);
    }

    #[test]
    fn create_duplicate_name_rejected() {
        let tmp = tempfile::tempdir().expect("tmp");
        let ws = ws_fixture(tmp.path());
        let reg = ServerRegistry::new(
            Arc::new(MemServers::new()),
            Arc::new(MemWorkspaces::new(ws)),
        );
        reg.create(
            None,
            "alpha",
            &VersionId::new("1.21.1"),
            ServerVariant::Vanilla,
        )
        .expect("first");
        let err = reg
            .create(
                None,
                "alpha",
                &VersionId::new("1.21.1"),
                ServerVariant::Vanilla,
            )
            .expect_err("dup");
        assert!(matches!(err, ServerRegistryError::AlreadyExists(_)));
    }

    #[test]
    fn get_missing_is_not_found() {
        let tmp = tempfile::tempdir().expect("tmp");
        let ws = ws_fixture(tmp.path());
        let reg = ServerRegistry::new(
            Arc::new(MemServers::new()),
            Arc::new(MemWorkspaces::new(ws)),
        );
        let err = reg.get("ghost").expect_err("missing");
        assert!(matches!(err, ServerRegistryError::NotFound(_)));
    }

    #[test]
    fn list_orders_by_name() {
        let tmp = tempfile::tempdir().expect("tmp");
        let ws = ws_fixture(tmp.path());
        let reg = ServerRegistry::new(
            Arc::new(MemServers::new()),
            Arc::new(MemWorkspaces::new(ws)),
        );
        reg.create(
            None,
            "beta",
            &VersionId::new("1.21.1"),
            ServerVariant::Vanilla,
        )
        .expect("b");
        reg.create(
            None,
            "alpha",
            &VersionId::new("1.21.1"),
            ServerVariant::Vanilla,
        )
        .expect("a");
        let names: Vec<_> = reg
            .list()
            .expect("list")
            .into_iter()
            .map(|r| r.meta.name.as_str().to_owned())
            .collect();
        assert_eq!(names, ["alpha", "beta"]);
    }
}
