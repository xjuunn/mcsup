//! 服务层夹具：真实布局 + 可控 supervisor + 可选的真实仓库
//!
//! 这一层解决的问题是「每条集成测试都要重写一遍装配」：`service` 的
//! `doctor.rs` 与 `workspace_remove.rs` 各写一份 `Fixture`，`backup.rs` 的
//! 模块内测试又维护一整套 `MemServers` / `MemWorkspaces` 替身——三套装配
//! 迟早会漂移成三种语义。
//!
//! 目录布局与产品完全一致：
//!
//! ```text
//! {root}/workspace.toml   工作目录 marker
//! {root}/servers/<name>/  服务器目录
//! {root}/backups/         工作区的 CAS 与快照
//! ```
//!
//! 本模块只依赖 `domain` / `port`：`testkit` 不认识 `storage`，真实仓库由
//! 各 crate 的 `tests/` 自己装配后传进 `server_repo()` / `workspace_repo()`
//! 的返回值里；需要真实 `JsonStore` 的场景直接 `JsonStore::open(root/db)`，
//! 与产品走同一条路径。
//!
//! 仓库替身 [`MemServerRepo`] / [`SingleWorkspaceRepo`] 只负责「记录」，不负责
//! 目录与 marker——那两样由夹具在磁盘上真实创建，避免桩把「记录与磁盘对不上」
//! 这类真实故障抹平。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use domain::server::meta::{ServerId, ServerMeta, ServerName};
use domain::server::settings::ServerSettings;
use domain::server::version::{ServerVariant, VersionId};
use domain::workspace::{Workspace, WorkspaceId, WorkspaceName};
use port::{RepositoryError, ServerRepository, WorkspaceRepository};

use crate::assert::{Failure, assert_invariant};

/// 夹具用的固定工作区身份：同一进程内多条夹具不会撞 id
pub const WORKSPACE_ID: &str = "550e8400-e29b-41d4-a716-446655440001";

fn not_found(name: &str) -> RepositoryError {
    RepositoryError::NotFound(name.to_owned())
}

/// 内存服务器仓库：只存记录，不碰磁盘
#[derive(Debug, Default)]
pub struct MemServerRepo {
    items: Mutex<Vec<ServerMeta>>,
}

impl MemServerRepo {
    /// 空仓库
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 直接塞一条记录（跳过注册表的目录与 marker 写入）
    ///
    /// # Panics
    ///
    /// 同名记录已存在时 panic：夹具里的重复插入一定是用例写错了
    pub fn insert(&self, meta: ServerMeta) {
        let mut items = self.items.lock().expect("lock");
        assert_invariant(!items.iter().any(|known| known.name == meta.name), || {
            Failure::invariant("同一名称不得重复插入").actual(meta.name.as_str())
        });
        items.push(meta);
    }

    /// 当前全部记录（按插入顺序）
    #[must_use]
    pub fn snapshot(&self) -> Vec<ServerMeta> {
        self.items.lock().expect("lock").clone()
    }
}

impl ServerRepository for MemServerRepo {
    fn create(&self, meta: &ServerMeta) -> Result<(), RepositoryError> {
        let mut items = self.items.lock().expect("lock");
        if items.iter().any(|known| known.name == meta.name) {
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
            .find(|meta| meta.name == *name)
            .cloned()
            .ok_or_else(|| not_found(name.as_str()))
    }

    fn list(&self) -> Result<Vec<ServerMeta>, RepositoryError> {
        Ok(self.snapshot())
    }

    fn update_core(
        &self,
        name: &ServerName,
        version: &VersionId,
        jar: &domain::server::meta::InstalledJar,
        launch: Option<&domain::server::launch::LaunchSpec>,
        required_java_major: Option<domain::java::JavaMajor>,
        updated_at: i64,
    ) -> Result<ServerMeta, RepositoryError> {
        let mut items = self.items.lock().expect("lock");
        let meta = items
            .iter_mut()
            .find(|meta| meta.name == *name)
            .ok_or_else(|| not_found(name.as_str()))?;
        meta.version = version.clone();
        meta.jar = Some(jar.clone());
        meta.launch = launch.cloned();
        meta.java_major = required_java_major;
        meta.updated_at = updated_at;
        Ok(meta.clone())
    }

    fn update_settings(
        &self,
        name: &ServerName,
        settings: &ServerSettings,
        updated_at: i64,
    ) -> Result<ServerMeta, RepositoryError> {
        let mut items = self.items.lock().expect("lock");
        let meta = items
            .iter_mut()
            .find(|meta| meta.name == *name)
            .ok_or_else(|| not_found(name.as_str()))?;
        meta.settings = settings.clone();
        meta.updated_at = updated_at;
        Ok(meta.clone())
    }

    fn remove(&self, name: &ServerName) -> Result<(), RepositoryError> {
        let mut items = self.items.lock().expect("lock");
        let before = items.len();
        items.retain(|meta| meta.name != *name);
        if items.len() == before {
            return Err(not_found(name.as_str()));
        }
        Ok(())
    }

    fn remove_by_workspace(&self, workspace_id: &WorkspaceId) -> Result<(), RepositoryError> {
        self.items
            .lock()
            .expect("lock")
            .retain(|meta| meta.workspace_id != *workspace_id);
        Ok(())
    }

    fn relink(
        &self,
        name: &ServerName,
        workspace_id: &WorkspaceId,
        rel_dir: &Path,
        updated_at: i64,
    ) -> Result<ServerMeta, RepositoryError> {
        let mut items = self.items.lock().expect("lock");
        let meta = items
            .iter_mut()
            .find(|meta| meta.name == *name)
            .ok_or_else(|| not_found(name.as_str()))?;
        meta.workspace_id = workspace_id.clone();
        meta.rel_dir = rel_dir.to_path_buf();
        meta.updated_at = updated_at;
        Ok(meta.clone())
    }
}

/// 只有一个工作目录的内存仓库
#[derive(Debug)]
pub struct SingleWorkspaceRepo {
    workspace: Workspace,
}

impl SingleWorkspaceRepo {
    /// 以给定工作目录构造
    #[must_use]
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

impl WorkspaceRepository for SingleWorkspaceRepo {
    fn create(&self, _workspace: &Workspace) -> Result<(), RepositoryError> {
        Ok(())
    }

    fn get(&self, id: &WorkspaceId) -> Result<Workspace, RepositoryError> {
        if self.workspace.id == *id {
            Ok(self.workspace.clone())
        } else {
            Err(not_found(id.as_str()))
        }
    }

    fn get_by_name(&self, name: &WorkspaceName) -> Result<Workspace, RepositoryError> {
        if self.workspace.name == *name {
            Ok(self.workspace.clone())
        } else {
            Err(not_found(name.as_str()))
        }
    }

    fn list(&self) -> Result<Vec<Workspace>, RepositoryError> {
        Ok(vec![self.workspace.clone()])
    }

    fn current(&self) -> Result<Option<Workspace>, RepositoryError> {
        Ok(Some(self.workspace.clone()))
    }

    fn set_current(&self, id: &WorkspaceId) -> Result<(), RepositoryError> {
        if self.workspace.id == *id {
            Ok(())
        } else {
            Err(not_found(id.as_str()))
        }
    }

    fn remove(&self, id: &WorkspaceId) -> Result<(), RepositoryError> {
        if self.workspace.id == *id {
            Ok(())
        } else {
            Err(not_found(id.as_str()))
        }
    }

    fn relink_path(
        &self,
        id: &WorkspaceId,
        _path: &Path,
        _updated_at: i64,
    ) -> Result<(), RepositoryError> {
        if self.workspace.id == *id {
            Ok(())
        } else {
            Err(not_found(id.as_str()))
        }
    }
}

/// 一台在磁盘上真实存在的服务器
#[derive(Debug, Clone)]
pub struct ServerFixture {
    /// 服务器目录（`{root}/servers/<name>`）
    pub dir: PathBuf,
    /// 注册表记录
    pub meta: ServerMeta,
}

impl ServerFixture {
    /// 服务器名
    #[must_use]
    pub fn name(&self) -> &str {
        self.meta.name.as_str()
    }

    /// 在服务器目录写一个相对路径文件（自动建父目录）
    pub fn write(&self, rel: &str, bytes: impl AsRef<[u8]>) {
        let path = self.dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir parent");
        }
        std::fs::write(path, bytes.as_ref()).expect("write file");
    }

    /// 读回服务器目录里的相对路径文件
    #[must_use]
    pub fn read(&self, rel: &str) -> Vec<u8> {
        std::fs::read(self.dir.join(rel)).expect("read file")
    }

    /// 删除服务器目录里的相对路径文件
    pub fn remove(&self, rel: &str) {
        std::fs::remove_file(self.dir.join(rel)).expect("remove file");
    }
}

/// 服务层夹具：一个工作区 + 若干服务器
///
/// 临时目录随夹具存活，drop 时连同全部磁盘内容一起清理。
#[derive(Debug)]
pub struct ServiceFixture {
    root: PathBuf,
    workspace: Workspace,
    servers: Vec<ServerFixture>,
    repo: Arc<MemServerRepo>,
    /// 保活：不能在 drop 前删掉，否则测试跑到一半目录消失
    _tmp: tempfile::TempDir,
}

impl ServiceFixture {
    /// 建一个空工作区（不含服务器）
    #[must_use]
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("ws");
        std::fs::create_dir_all(root.join("servers")).expect("mkdir servers");
        let workspace = Workspace {
            id: WorkspaceId::parse(WORKSPACE_ID).expect("uuid"),
            name: WorkspaceName::parse("ws01").expect("name"),
            path: root.clone(),
            is_current: true,
            created_at: 1,
            updated_at: 1,
        };
        Self {
            root,
            workspace,
            servers: Vec::new(),
            repo: Arc::new(MemServerRepo::new()),
            _tmp: tmp,
        }
    }

    /// 建一个工作区，并注册服务器 `alpha`
    #[must_use]
    pub fn with_server(name: &str) -> Self {
        let mut fixture = Self::new();
        fixture.add_server(name);
        fixture
    }

    /// 在工作区里注册一台服务器（目录真实创建，记录进内存仓库）
    pub fn add_server(&mut self, name: &str) -> &ServerFixture {
        let dir = self.root.join("servers").join(name);
        std::fs::create_dir_all(&dir).expect("mkdir server dir");
        let meta = server_meta(name, self.workspace.id.as_str());
        self.repo.insert(meta.clone());
        self.servers.push(ServerFixture { dir, meta });
        self.servers.last().expect("just pushed")
    }

    /// 工作区根目录
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 工作区对象
    #[must_use]
    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    /// 按名称取服务器；不存在时 panic（用例里拼错名字属于用例错误）
    #[must_use]
    pub fn server(&self, name: &str) -> &ServerFixture {
        self.servers
            .iter()
            .find(|server| server.name() == name)
            .unwrap_or_else(|| panic!("fixture has no server {name}"))
    }

    /// 服务器仓库（构造 `ServerRegistry` 用）
    #[must_use]
    pub fn server_repo(&self) -> Arc<dyn ServerRepository> {
        self.repo.clone()
    }

    /// 工作区仓库（构造 `ServerRegistry` 用）
    #[must_use]
    pub fn workspace_repo(&self) -> Arc<dyn WorkspaceRepository> {
        Arc::new(SingleWorkspaceRepo::new(self.workspace.clone()))
    }

    /// 工作区的备份根目录（`backups/`，CAS 与快照都在其下）
    #[must_use]
    pub fn backups_root(&self) -> PathBuf {
        self.root.join("backups")
    }

    /// CAS 对象根目录（`backups/cas`）
    #[must_use]
    pub fn cas_root(&self) -> PathBuf {
        self.backups_root().join("cas")
    }

    /// 快照根目录（`backups/snapshots`）
    #[must_use]
    pub fn snapshots_root(&self) -> PathBuf {
        self.backups_root().join("snapshots")
    }
}

impl Default for ServiceFixture {
    fn default() -> Self {
        Self::new()
    }
}

/// 构造一条服务器记录（`rel_dir` 与目录名一致）
///
/// 身份每次新生成：同一测试里建多台服务器时不会撞 id。
#[must_use]
pub fn server_meta(name: &str, workspace_id: &str) -> ServerMeta {
    ServerMeta {
        id: ServerId::parse(&uuid::Uuid::new_v4().to_string()).expect("uuid"),
        name: ServerName::parse(name).expect("server name"),
        workspace_id: WorkspaceId::parse(workspace_id).expect("workspace id"),
        rel_dir: PathBuf::from(name),
        version: VersionId::new("1.21.1"),
        variant: ServerVariant::Vanilla,
        jar: None,
        launch: None,
        settings: ServerSettings::default(),
        java_major: None,
        created_at: 1,
        updated_at: 1,
    }
}

/// 断言磁盘上某个路径不存在
pub fn assert_absent(path: &Path, invariant: &str) {
    assert_invariant(!path.exists(), || {
        Failure::invariant(invariant)
            .expected("不存在")
            .actual("存在")
            .tree(path.parent().unwrap_or(path))
    });
}

/// 断言磁盘上某个路径存在
pub fn assert_present(path: &Path, invariant: &str) {
    assert_invariant(path.exists(), || {
        Failure::invariant(invariant)
            .expected("存在")
            .actual("不存在")
            .tree(path.parent().unwrap_or(path))
    });
}
