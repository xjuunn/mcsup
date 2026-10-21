//! `server.properties` 读写编排：注册表定位服务器目录 → 读 / 原子写
//!
//! 解析与渲染规则在 `domain::server::props`，本层只负责定位与 IO；
//! 判定不分叉，入口（CLI / API / MCP）不直接碰文件。

use crate::fs_error::FsError;
use crate::registry::{RegisteredServer, ServerRegistry, ServerRegistryError};
use domain::server::props::{PROPERTIES_FILE, Properties};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;

/// 临时文件后缀：写入完成前不触碰正式文件
const TMP_SUFFIX: &str = ".tmp";

/// 属性读写失败
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PropsError {
    /// 注册表失败（含服务器不存在）
    #[error(transparent)]
    Registry(#[from] ServerRegistryError),
    /// 键或值不满足写入规则
    #[error(transparent)]
    InvalidSet(#[from] domain::server::error::PropertySetError),
    /// 文件读写失败
    #[error(transparent)]
    Io(#[from] FsError),
}

/// 一次属性读写的结果快照
#[derive(Debug, Clone)]
pub struct PropsReport {
    /// 服务器（含绝对目录）
    pub server: RegisteredServer,
    /// `server.properties` 绝对路径
    pub path: PathBuf,
    /// 当前内容；文件不存在时为空视图
    pub properties: Properties,
}

/// 服务器属性文件读写
pub struct ServerPropsService {
    registry: Arc<ServerRegistry>,
}

impl std::fmt::Debug for ServerPropsService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerPropsService")
            .field("registry", &"…")
            .finish_non_exhaustive()
    }
}

impl ServerPropsService {
    /// 组合注册表（目录定位由注册表回答）
    #[must_use]
    pub fn new(registry: Arc<ServerRegistry>) -> Self {
        Self { registry }
    }

    /// 读取当前内容；文件不存在视为空（服务器首次启动后由服务端生成）
    ///
    /// # Errors
    ///
    /// 服务器不存在或读取失败时返回 [`PropsError`]
    pub fn read(&self, name: &str) -> Result<PropsReport, PropsError> {
        let server = self.registry.get(name)?;
        let path = server.dir.join(PROPERTIES_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == ErrorKind::NotFound => String::new(),
            Err(err) => return Err(FsError::io(&path, err).into()),
        };
        Ok(PropsReport {
            server,
            path,
            properties: Properties::parse(&text),
        })
    }

    /// 写入若干键值并落盘，返回写入后的快照
    ///
    /// 全部键先在内存视图上校验通过，再一次性原子写盘——中途失败不留下
    /// 半截文件，也不出现「改了一半」的属性文件。
    ///
    /// # Errors
    ///
    /// 服务器不存在、键 / 值非法或写入失败时返回 [`PropsError`]
    pub fn set(&self, name: &str, updates: &[(String, String)]) -> Result<PropsReport, PropsError> {
        let mut report = self.read(name)?;
        for (key, value) in updates {
            report.properties.set(key, value)?;
        }
        write_atomic(&report.path, &report.properties.render())?;
        Ok(report)
    }
}

/// 临时文件写满后 rename 覆盖：中途失败时正式文件保持原样
fn write_atomic(path: &Path, text: &str) -> Result<(), PropsError> {
    let tmp = path.with_file_name(format!("{PROPERTIES_FILE}{TMP_SUFFIX}"));
    std::fs::write(&tmp, text).map_err(|err| FsError::io(&tmp, err))?;
    if let Err(err) = std::fs::rename(&tmp, path) {
        // 清理残留临时文件，避免服务器把它当成别的文件
        let _ = std::fs::remove_file(&tmp);
        return Err(FsError::io(path, err).into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::server::version::{ServerVariant, VersionId};
    use domain::workspace::{Workspace, WorkspaceId, WorkspaceName};
    use port::{RepositoryError, ServerRepository, WorkspaceRepository};
    use std::path::Path;

    struct MemServers {
        items: std::sync::Mutex<Vec<domain::server::meta::ServerMeta>>,
    }

    impl ServerRepository for MemServers {
        fn create(&self, meta: &domain::server::meta::ServerMeta) -> Result<(), RepositoryError> {
            self.items.lock().expect("lock").push(meta.clone());
            Ok(())
        }
        fn get(
            &self,
            name: &domain::server::meta::ServerName,
        ) -> Result<domain::server::meta::ServerMeta, RepositoryError> {
            self.items
                .lock()
                .expect("lock")
                .iter()
                .find(|m| &m.name == name)
                .cloned()
                .ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))
        }
        fn list(&self) -> Result<Vec<domain::server::meta::ServerMeta>, RepositoryError> {
            Ok(self.items.lock().expect("lock").clone())
        }
        fn update_jar(
            &self,
            _name: &domain::server::meta::ServerName,
            _jar: &domain::server::meta::InstalledJar,
            _updated_at: i64,
        ) -> Result<domain::server::meta::ServerMeta, RepositoryError> {
            unreachable!("not used in tests")
        }
        fn update_settings(
            &self,
            _name: &domain::server::meta::ServerName,
            _settings: &domain::server::settings::ServerSettings,
            _updated_at: i64,
        ) -> Result<domain::server::meta::ServerMeta, RepositoryError> {
            unreachable!("not used in tests")
        }
        fn remove(&self, _name: &domain::server::meta::ServerName) -> Result<(), RepositoryError> {
            unreachable!("not used in tests")
        }
        fn remove_by_workspace(&self, _workspace_id: &WorkspaceId) -> Result<(), RepositoryError> {
            Ok(())
        }
        fn relink(
            &self,
            _name: &domain::server::meta::ServerName,
            _workspace_id: &WorkspaceId,
            _rel_dir: &Path,
            _updated_at: i64,
        ) -> Result<domain::server::meta::ServerMeta, RepositoryError> {
            unreachable!("not used in tests")
        }
    }

    struct MemWorkspaces {
        item: std::sync::Mutex<Option<Workspace>>,
    }

    impl WorkspaceRepository for MemWorkspaces {
        fn create(&self, ws: &Workspace) -> Result<(), RepositoryError> {
            *self.item.lock().expect("lock") = Some(ws.clone());
            Ok(())
        }
        fn get(&self, id: &WorkspaceId) -> Result<Workspace, RepositoryError> {
            self.item
                .lock()
                .expect("lock")
                .clone()
                .filter(|ws| &ws.id == id)
                .ok_or_else(|| RepositoryError::NotFound(id.as_str().to_owned()))
        }
        fn get_by_name(&self, name: &WorkspaceName) -> Result<Workspace, RepositoryError> {
            self.item
                .lock()
                .expect("lock")
                .clone()
                .filter(|ws| &ws.name == name)
                .ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))
        }
        fn list(&self) -> Result<Vec<Workspace>, RepositoryError> {
            Ok(self
                .item
                .lock()
                .expect("lock")
                .clone()
                .into_iter()
                .collect())
        }
        fn current(&self) -> Result<Option<Workspace>, RepositoryError> {
            Ok(self.item.lock().expect("lock").clone())
        }
        fn set_current(&self, _id: &WorkspaceId) -> Result<(), RepositoryError> {
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

    /// 建好 workspace + 服务器的服务与目录
    fn fixture() -> (ServerPropsService, PathBuf, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tmp");
        let ws_path = tmp.path().join("ws");
        std::fs::create_dir_all(ws_path.join("servers")).expect("mkdir");
        let ws = Workspace {
            id: WorkspaceId::parse("550e8400-e29b-41d4-a716-446655440001").expect("uuid"),
            name: WorkspaceName::parse("ws01").expect("valid"),
            path: ws_path,
            is_current: true,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
        };
        let registry = Arc::new(ServerRegistry::new(
            Arc::new(MemServers {
                items: std::sync::Mutex::new(Vec::new()),
            }),
            Arc::new(MemWorkspaces {
                item: std::sync::Mutex::new(Some(ws)),
            }),
        ));
        let server = registry
            .create(
                None,
                "alpha",
                &VersionId::new("1.21.1"),
                ServerVariant::Vanilla,
            )
            .expect("create");
        let dir = server.dir.clone();
        (ServerPropsService::new(registry), dir, tmp)
    }

    fn pair(key: &str, value: &str) -> (String, String) {
        (key.to_owned(), value.to_owned())
    }

    #[test]
    fn read_missing_file_is_empty_view() {
        let (svc, dir, _tmp) = fixture();
        let report = svc.read("alpha").expect("read");
        assert_eq!(report.path, dir.join(PROPERTIES_FILE));
        assert!(report.properties.is_empty());
    }

    #[test]
    fn set_writes_file_and_read_roundtrips() {
        let (svc, dir, _tmp) = fixture();
        let report = svc
            .set("alpha", &[pair("motd", "hello"), pair("max-players", "8")])
            .expect("set");
        assert_eq!(report.properties.get("motd"), Some("hello"));

        let text = std::fs::read_to_string(dir.join(PROPERTIES_FILE)).expect("file");
        assert_eq!(text, "motd=hello\nmax-players=8\n");

        let reread = svc.read("alpha").expect("read");
        assert_eq!(reread.properties.get("max-players"), Some("8"));
        // 临时文件不残留
        assert!(!dir.join(format!("{PROPERTIES_FILE}.tmp")).exists());
    }

    #[test]
    fn set_rejects_invalid_pairs_without_touching_file() {
        let (svc, dir, _tmp) = fixture();
        svc.set("alpha", &[pair("motd", "kept")]).expect("seed");
        let err = svc
            .set("alpha", &[pair("bad=key", "v")])
            .expect_err("invalid key");
        assert!(matches!(err, PropsError::InvalidSet(_)));

        let text = std::fs::read_to_string(dir.join(PROPERTIES_FILE)).expect("file");
        assert_eq!(text, "motd=kept\n");
    }

    #[test]
    fn set_unknown_server_is_registry_error() {
        let (svc, _dir, _tmp) = fixture();
        let err = svc.set("ghost", &[pair("motd", "x")]).expect_err("missing");
        assert!(matches!(err, PropsError::Registry(_)));
    }
}
