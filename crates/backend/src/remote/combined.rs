//! 组合仓库：本地 + 多台远端合成一个查询视图
//!
//! `docs/remote.md`「设计原则 1」：远端是资源属性，不是全局状态。组合仓库
//! 是这个原则在数据面的落点——`service` 拿到的仍是一个
//! `Arc<dyn ServerRepository>`，列表是合并结果、每条带真实来源，`service`
//! 里没有一处 `if remote { … } else { … }`。
//!
//! # 读与写的分工
//!
//! 读（`list` / `get` / `list_sourced`）：本地查本地仓库，每台远端查
//! 自己的镜像仓库，合并返回。远端不可达时降级为空集并记 warn——离线不阻塞
//! 本地操作（`docs/remote.md`「设计原则 4」），离线缓存的 `(offline)` 标记
//! 由入口渲染层负责。
//!
//! 写：**委托本地**。能到达组合仓库的写调用只有两类：service 的本地操作
//! （`WorkspaceService::init`、`JavaService` 扫描登记……）与本地注册表维护，
//! 二者目标都是本机，直接委托本地仓库；远端写**不会**经过这里——入口的
//! 路由层按目标来源把它分派给镜像仓库的显式方法（`init` / `create` /
//! `remove` …）。这条约定不靠组合仓库强制（拒绝会误伤本地写），而由路由层
//! 的分派保证：镜像方法是远端写的唯一通道。
//!
//! # 线程模型
//!
//! 每次读逐台远端串行请求：远端是低频管理操作，串行让失败定位与日志阅读
//! 都简单；等远端数量成为问题时再并行化，接口不变。

use crate::remote::mirror::{
    RemoteJavaRepository, RemoteServerRepository, RemoteWorkspaceRepository,
};
use domain::java::{JavaMajor, JavaRuntime};
use domain::server::launch::LaunchSpec;
use domain::server::meta::{InstalledJar, ServerMeta, ServerName};
use domain::server::settings::ServerSettings;
use domain::server::version::VersionId;
use domain::workspace::{Workspace, WorkspaceId, WorkspaceName};
use port::{
    JavaRuntimeRepository, Origin, RemoteName, RepositoryError, ServerRepository, Sourced,
    WorkspaceRepository,
};
use std::sync::Arc;

/// 一台已配对远端在组合仓库里的接入点
#[derive(Clone)]
pub struct RemoteMember {
    /// 远端名（来源标注与日志共用）
    pub name: RemoteName,
    /// 三个镜像仓库
    pub workspaces: Arc<RemoteWorkspaceRepository>,
    pub servers: Arc<RemoteServerRepository>,
    pub java: Arc<RemoteJavaRepository>,
}

impl RemoteMember {
    /// 用一条连接装配三个镜像
    #[must_use]
    pub fn new(link: crate::remote::RemoteLink, name: RemoteName) -> Self {
        let origin = Origin::Remote(name.clone());
        Self {
            workspaces: Arc::new(RemoteWorkspaceRepository::new(link.clone(), origin.clone())),
            servers: Arc::new(RemoteServerRepository::new(link.clone(), origin.clone())),
            java: Arc::new(RemoteJavaRepository::new(link, origin)),
            name,
        }
    }
}

impl std::fmt::Debug for RemoteMember {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteMember")
            .field("name", &self.name.as_str())
            .finish_non_exhaustive()
    }
}

/// 组合仓库的远端侧：按名取成员
#[derive(Debug, Clone, Default)]
pub struct RemoteMembers {
    members: Vec<RemoteMember>,
}

impl RemoteMembers {
    /// 空（纯本地装配）
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// 追加一台远端
    pub fn push(&mut self, member: RemoteMember) {
        self.members.push(member);
    }

    /// 已接入的远端数
    #[must_use]
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// 是否没有任何远端
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// 按名取一台远端
    #[must_use]
    pub fn get(&self, name: &RemoteName) -> Option<&RemoteMember> {
        self.members.iter().find(|member| &member.name == name)
    }

    /// 全部成员（稳定顺序）
    #[must_use]
    pub fn all(&self) -> &[RemoteMember] {
        &self.members
    }
}

/// 组合工作目录仓库：本地 + 全部远端
pub struct CombinedWorkspaceRepository {
    local: Arc<dyn WorkspaceRepository>,
    remotes: RemoteMembers,
}

impl std::fmt::Debug for CombinedWorkspaceRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CombinedWorkspaceRepository")
            .field("remotes", &self.remotes)
            .finish_non_exhaustive()
    }
}

impl CombinedWorkspaceRepository {
    /// 装配
    #[must_use]
    pub fn new(local: Arc<dyn WorkspaceRepository>, remotes: RemoteMembers) -> Self {
        Self { local, remotes }
    }
}

impl WorkspaceRepository for CombinedWorkspaceRepository {
    /// 经由本仓库的创建就是本地创建
    ///
    /// 路由规则：service（`WorkspaceService::init` 等）到达这里时目标必然是
    /// 本机——远端写由入口的路由层截走，走镜像仓库的显式方法（`init` /
    /// `set_current` / `remove`），根本不会到达组合仓库。因此这里把写**委托
    /// 给本地仓库**而不是拒绝：拒绝会把「有远端接入」变成「本地不能建工作区」，
    /// 这不是任何人想要的行为。
    fn create(&self, ws: &Workspace) -> Result<(), RepositoryError> {
        self.local.create(ws)
    }

    fn get(&self, id: &WorkspaceId) -> Result<Workspace, RepositoryError> {
        self.list()?
            .into_iter()
            .find(|ws| &ws.id == id)
            .ok_or_else(|| RepositoryError::NotFound(id.as_str().to_owned()))
    }

    fn get_by_name(&self, name: &WorkspaceName) -> Result<Workspace, RepositoryError> {
        self.list()?
            .into_iter()
            .find(|ws| &ws.name == name)
            .ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))
    }

    fn list(&self) -> Result<Vec<Workspace>, RepositoryError> {
        let mut out = self.local.list()?;
        for member in self.remotes.all() {
            match member.workspaces.list() {
                Ok(items) => out.extend(items),
                Err(err) => {
                    // 离线不阻塞：本地列表照常返回，远端条目缺失由入口提示
                    tracing::warn!(remote = member.name.as_str(), %err, "remote workspace list failed; skipping");
                }
            }
        }
        Ok(out)
    }

    fn current(&self) -> Result<Option<Workspace>, RepositoryError> {
        // 当前工作区是本机状态：本地有就用本地的，本地没有再看远端
        if let Some(current) = self.local.current()? {
            return Ok(Some(current));
        }
        for member in self.remotes.all() {
            match member.workspaces.current() {
                Ok(Some(ws)) => return Ok(Some(ws)),
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(remote = member.name.as_str(), %err, "remote current lookup failed");
                }
            }
        }
        Ok(None)
    }

    /// 本地优先：远端的「当前工作区」是对端状态，不由本机写路径切换
    ///
    /// service（`WorkspaceService::set_current`）到达这里的调用目标是本机
    /// 当前工作区（同 [`Self::create`] 的路由规则），委托本地。
    fn set_current(&self, id: &WorkspaceId) -> Result<(), RepositoryError> {
        self.local.set_current(id)
    }

    fn remove(&self, id: &WorkspaceId) -> Result<(), RepositoryError> {
        self.local.remove(id)
    }

    fn relink_path(
        &self,
        _id: &WorkspaceId,
        _path: &std::path::Path,
        _updated_at: i64,
    ) -> Result<(), RepositoryError> {
        Err(RepositoryError::Storage(Box::new(std::io::Error::other(
            "relink is a local-only operation",
        ))))
    }

    fn list_sourced(&self) -> Result<Vec<Sourced<Workspace>>, RepositoryError> {
        let mut out: Vec<Sourced<Workspace>> =
            self.local.list()?.into_iter().map(Sourced::local).collect();
        for member in self.remotes.all() {
            match member.workspaces.list_sourced() {
                Ok(items) => out.extend(items),
                Err(err) => {
                    tracing::warn!(remote = member.name.as_str(), %err, "remote workspace list failed; skipping");
                }
            }
        }
        Ok(out)
    }
}

/// 组合服务器仓库：本地 + 全部远端
pub struct CombinedServerRepository {
    local: Arc<dyn ServerRepository>,
    remotes: RemoteMembers,
}

impl std::fmt::Debug for CombinedServerRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CombinedServerRepository")
            .field("remotes", &self.remotes)
            .finish_non_exhaustive()
    }
}

impl CombinedServerRepository {
    /// 装配
    #[must_use]
    pub fn new(local: Arc<dyn ServerRepository>, remotes: RemoteMembers) -> Self {
        Self { local, remotes }
    }
}

impl ServerRepository for CombinedServerRepository {
    /// 经由本仓库的创建就是本地创建（路由规则见 [`CombinedWorkspaceRepository::create`]）
    fn create(&self, meta: &ServerMeta) -> Result<(), RepositoryError> {
        self.local.create(meta)
    }

    fn get(&self, name: &ServerName) -> Result<ServerMeta, RepositoryError> {
        self.list()?
            .into_iter()
            .find(|meta| &meta.name == name)
            .ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))
    }

    fn list(&self) -> Result<Vec<ServerMeta>, RepositoryError> {
        let mut out = self.local.list()?;
        for member in self.remotes.all() {
            match member.servers.list() {
                Ok(items) => out.extend(items),
                Err(err) => {
                    tracing::warn!(remote = member.name.as_str(), %err, "remote server list failed; skipping");
                }
            }
        }
        Ok(out)
    }

    fn update_core(
        &self,
        name: &ServerName,
        version: &VersionId,
        jar: &InstalledJar,
        launch: Option<&LaunchSpec>,
        required_java_major: Option<JavaMajor>,
        updated_at: i64,
    ) -> Result<ServerMeta, RepositoryError> {
        // 注册表先 get 再 update_core：get 命中远端条目时 update 也属于远端，
        // 但远端核心安装走 agent 的 service（阶段 8 的路由层），这里只可能
        // 收到本地服务器的更新——get 已确认本地存在则委托，否则透传错误
        self.local.get(name)?;
        self.local
            .update_core(name, version, jar, launch, required_java_major, updated_at)
    }

    fn update_settings(
        &self,
        name: &ServerName,
        settings: &ServerSettings,
        updated_at: i64,
    ) -> Result<ServerMeta, RepositoryError> {
        self.local.get(name)?;
        self.local.update_settings(name, settings, updated_at)
    }

    fn remove(&self, name: &ServerName) -> Result<(), RepositoryError> {
        self.local.get(name)?;
        self.local.remove(name)
    }

    fn remove_by_workspace(&self, workspace_id: &WorkspaceId) -> Result<(), RepositoryError> {
        self.local.remove_by_workspace(workspace_id)
    }

    fn relink(
        &self,
        _name: &ServerName,
        _workspace_id: &WorkspaceId,
        _rel_dir: &std::path::Path,
        _updated_at: i64,
    ) -> Result<ServerMeta, RepositoryError> {
        Err(RepositoryError::Storage(Box::new(std::io::Error::other(
            "relink is a local-only operation",
        ))))
    }

    fn list_sourced(&self) -> Result<Vec<Sourced<ServerMeta>>, RepositoryError> {
        let mut out: Vec<Sourced<ServerMeta>> =
            self.local.list()?.into_iter().map(Sourced::local).collect();
        for member in self.remotes.all() {
            match member.servers.list_sourced() {
                Ok(items) => out.extend(items),
                Err(err) => {
                    tracing::warn!(remote = member.name.as_str(), %err, "remote server list failed; skipping");
                }
            }
        }
        Ok(out)
    }
}

/// 组合 Java 仓库：本地 + 全部远端
pub struct CombinedJavaRepository {
    local: Arc<dyn JavaRuntimeRepository>,
    remotes: RemoteMembers,
}

impl std::fmt::Debug for CombinedJavaRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CombinedJavaRepository")
            .field("remotes", &self.remotes)
            .finish_non_exhaustive()
    }
}

impl CombinedJavaRepository {
    /// 装配
    #[must_use]
    pub fn new(local: Arc<dyn JavaRuntimeRepository>, remotes: RemoteMembers) -> Self {
        Self { local, remotes }
    }
}

impl JavaRuntimeRepository for CombinedJavaRepository {
    /// 经由本仓库的登记就是本地登记（JavaService 的扫描 / 安装在本机发生）
    fn upsert(&self, runtime: &JavaRuntime) -> Result<(), RepositoryError> {
        self.local.upsert(runtime)
    }

    fn list(&self) -> Result<Vec<JavaRuntime>, RepositoryError> {
        let mut out = self.local.list()?;
        for member in self.remotes.all() {
            match member.java.list() {
                Ok(items) => out.extend(items),
                Err(err) => {
                    tracing::warn!(remote = member.name.as_str(), %err, "remote java list failed; skipping");
                }
            }
        }
        Ok(out)
    }

    fn get_by_path(&self, path: &std::path::Path) -> Result<JavaRuntime, RepositoryError> {
        self.list()?
            .into_iter()
            .find(|rt| rt.path == path)
            .ok_or_else(|| RepositoryError::NotFound(path.display().to_string()))
    }

    fn remove(&self, path: &std::path::Path) -> Result<(), RepositoryError> {
        self.local.remove(path)
    }

    fn scan_state(&self) -> Result<Option<i64>, RepositoryError> {
        self.local.scan_state()
    }

    fn mark_scanned(&self, at: i64) -> Result<(), RepositoryError> {
        self.local.mark_scanned(at)
    }

    fn list_sourced(&self) -> Result<Vec<Sourced<JavaRuntime>>, RepositoryError> {
        let mut out: Vec<Sourced<JavaRuntime>> =
            self.local.list()?.into_iter().map(Sourced::local).collect();
        for member in self.remotes.all() {
            match member.java.list_sourced() {
                Ok(items) => out.extend(items),
                Err(err) => {
                    tracing::warn!(remote = member.name.as_str(), %err, "remote java list failed; skipping");
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn test_fingerprint() -> transport::Fingerprint {
        transport::Fingerprint::of_der(b"test")
    }

    /// 内存版工作目录仓库：只为了验证组合层的透传行为
    struct MemWorkspaces(Mutex<Vec<Workspace>>);

    impl WorkspaceRepository for MemWorkspaces {
        fn create(&self, ws: &Workspace) -> Result<(), RepositoryError> {
            self.0.lock().expect("lock").push(ws.clone());
            Ok(())
        }

        fn get(&self, id: &WorkspaceId) -> Result<Workspace, RepositoryError> {
            self.list()?
                .into_iter()
                .find(|ws| &ws.id == id)
                .ok_or_else(|| RepositoryError::NotFound(id.as_str().to_owned()))
        }

        fn get_by_name(&self, name: &WorkspaceName) -> Result<Workspace, RepositoryError> {
            self.list()?
                .into_iter()
                .find(|ws| &ws.name == name)
                .ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))
        }

        fn list(&self) -> Result<Vec<Workspace>, RepositoryError> {
            Ok(self.0.lock().expect("lock").clone())
        }

        fn current(&self) -> Result<Option<Workspace>, RepositoryError> {
            Ok(self.list()?.into_iter().find(|ws| ws.is_current))
        }

        fn set_current(&self, id: &WorkspaceId) -> Result<(), RepositoryError> {
            let mut items = self.0.lock().expect("lock");
            for ws in items.iter_mut() {
                ws.is_current = &ws.id == id;
            }
            Ok(())
        }

        fn remove(&self, id: &WorkspaceId) -> Result<(), RepositoryError> {
            self.0.lock().expect("lock").retain(|ws| &ws.id != id);
            Ok(())
        }

        fn relink_path(
            &self,
            _id: &WorkspaceId,
            _path: &std::path::Path,
            _updated_at: i64,
        ) -> Result<(), RepositoryError> {
            Ok(())
        }
    }

    fn workspace(name: &str, current: bool) -> Workspace {
        Workspace {
            id: WorkspaceId::parse("550e8400-e29b-41d4-a716-446655440000").expect("id"),
            name: WorkspaceName::parse(name).expect("name"),
            path: std::path::PathBuf::from(format!("/srv/{name}")),
            is_current: current,
            created_at: 1,
            updated_at: 1,
        }
    }

    /// 空远端时组合仓库就是本地仓库的透传
    #[test]
    fn empty_remotes_passthrough_local() {
        let local = Arc::new(MemWorkspaces(Mutex::new(vec![workspace("w1", true)])));
        let combined = CombinedWorkspaceRepository::new(local, RemoteMembers::empty());
        let all = combined.list().expect("list");
        assert_eq!(all.len(), 1);
        let sourced = combined.list_sourced().expect("sourced");
        assert_eq!(sourced.len(), 1);
        assert_eq!(sourced[0].origin, Origin::Local);
        assert_eq!(
            combined
                .current()
                .expect("current")
                .map(|ws| ws.name.as_str().to_owned()),
            Some("w1".to_owned())
        );
    }

    /// service 的本地写（`create` / `set_current` / `remove`）委托给本地仓库：
    /// 有远端接入时本地命令语义不变
    #[test]
    fn local_writes_delegate_to_local_repo() {
        let mem = Arc::new(MemWorkspaces(Mutex::new(Vec::new())));
        let combined = CombinedWorkspaceRepository::new(mem.clone(), RemoteMembers::empty());

        let ws = workspace("w1", false);
        combined.create(&ws).expect("create delegates");
        assert_eq!(mem.list().expect("mem").len(), 1);

        let id = ws.id.clone();
        combined.set_current(&id).expect("set_current delegates");
        let current = combined.current().expect("current");
        assert_eq!(current.map(|ws| ws.id), Some(id.clone()));

        combined.remove(&id).expect("remove delegates");
        assert!(mem.list().expect("mem").is_empty());
    }

    /// 远端不可达时不拖垮列表：本地条目照常返回
    #[test]
    fn unreachable_remote_degrades_to_local_only() {
        // 一台指向不监听端口的远端
        let dead = crate::remote::RemoteLink::anonymous("127.0.0.1:1", test_fingerprint());
        let mut members = RemoteMembers::empty();
        members.push(RemoteMember::new(
            dead,
            RemoteName::parse("dead-box").expect("name"),
        ));
        let combined = CombinedServerRepository::new(
            // 服务器仓库用一个空实现即可：本测试只关心远端失败被吞掉
            Arc::new(EmptyServers),
            members,
        );
        let all = combined.list().expect("local list survives");
        assert!(all.is_empty());
    }

    struct EmptyServers;

    impl ServerRepository for EmptyServers {
        fn create(&self, _meta: &ServerMeta) -> Result<(), RepositoryError> {
            Ok(())
        }

        fn get(&self, name: &ServerName) -> Result<ServerMeta, RepositoryError> {
            Err(RepositoryError::NotFound(name.as_str().to_owned()))
        }

        fn list(&self) -> Result<Vec<ServerMeta>, RepositoryError> {
            Ok(Vec::new())
        }

        fn update_core(
            &self,
            _name: &ServerName,
            _version: &VersionId,
            _jar: &InstalledJar,
            _launch: Option<&LaunchSpec>,
            _required_java_major: Option<JavaMajor>,
            _updated_at: i64,
        ) -> Result<ServerMeta, RepositoryError> {
            Err(RepositoryError::NotFound(String::new()))
        }

        fn update_settings(
            &self,
            _name: &ServerName,
            _settings: &ServerSettings,
            _updated_at: i64,
        ) -> Result<ServerMeta, RepositoryError> {
            Err(RepositoryError::NotFound(String::new()))
        }

        fn remove(&self, _name: &ServerName) -> Result<(), RepositoryError> {
            Ok(())
        }

        fn remove_by_workspace(&self, _workspace_id: &WorkspaceId) -> Result<(), RepositoryError> {
            Ok(())
        }

        fn relink(
            &self,
            _name: &ServerName,
            _workspace_id: &WorkspaceId,
            _rel_dir: &std::path::Path,
            _updated_at: i64,
        ) -> Result<ServerMeta, RepositoryError> {
            Err(RepositoryError::NotFound(String::new()))
        }
    }
}
