//! Local / Remote 执行位置抽象
//!
//! [`LocalBackend`] 把本机实现（runtime / storage / backup）聚合成一个
//! [`port::Backend`]；`service` 只依赖 trait，不依赖任何具体实现 crate。
//!
//! Remote 将来经 transport → RPC → agent 提供同一 trait 的实现，业务判定
//! 不受影响（`Local / Remote 共享同一套业务逻辑`）。

use port::{
    Backend, ContentStore, InstallerRunner, Scanner, ServerRepository, SnapshotRepository,
    SupervisorPort, WorkspaceRepository,
};
use std::path::Path;
use std::sync::Arc;
use storage::JsonStore;

/// 本机执行位置
///
/// 由组合根（`mcsup::bootstrap`）装配：三个仓库默认共用同一个 [`JsonStore`]，
/// supervisor / installer 默认是 `runtime` 的实现。任一项都可被替换，便于
/// 宿主启动器接管进程或改用自定义存储。
pub struct LocalBackend {
    supervisor: Arc<dyn SupervisorPort>,
    installer: Arc<dyn InstallerRunner>,
    servers: Arc<dyn ServerRepository>,
    workspaces: Arc<dyn WorkspaceRepository>,
}

impl std::fmt::Debug for LocalBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalBackend").finish_non_exhaustive()
    }
}

impl LocalBackend {
    /// 用给定仓库 / supervisor / installer 装配
    #[must_use]
    pub fn new(
        supervisor: Arc<dyn SupervisorPort>,
        installer: Arc<dyn InstallerRunner>,
        servers: Arc<dyn ServerRepository>,
        workspaces: Arc<dyn WorkspaceRepository>,
    ) -> Self {
        Self {
            supervisor,
            installer,
            servers,
            workspaces,
        }
    }

    /// 默认本机装配：`runtime` 的 supervisor / installer，仓库取 `store`
    #[must_use]
    pub fn with_store(store: Arc<JsonStore>) -> Self {
        Self::new(
            Arc::new(runtime::LocalSupervisor),
            Arc::new(runtime::LocalInstallerRunner::new()),
            store.clone(),
            store,
        )
    }
}

impl Backend for LocalBackend {
    fn supervisor(&self) -> &dyn SupervisorPort {
        self.supervisor.as_ref()
    }

    fn installer(&self) -> &dyn InstallerRunner {
        self.installer.as_ref()
    }

    fn servers(&self) -> &dyn ServerRepository {
        self.servers.as_ref()
    }

    fn workspaces(&self) -> &dyn WorkspaceRepository {
        self.workspaces.as_ref()
    }

    fn scanner(&self) -> &dyn Scanner {
        &backup::DirectoryScanner
    }

    fn content_store(&self, backups_root: &Path) -> Arc<dyn ContentStore> {
        Arc::new(backup::CasStore::new(backups_root))
    }

    fn snapshot_repository(&self, backups_root: &Path) -> Arc<dyn SnapshotRepository> {
        Arc::new(backup::SnapshotIndex::new(backups_root))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use port::workspace_backups_root;

    /// 默认本机装配能给出可用的 CAS / 快照 / 扫描，且三者按同一 root 落地
    #[test]
    fn with_store_builds_working_capabilities() {
        let store = Arc::new(JsonStore::open_in_memory().expect("memory store"));
        let backend = LocalBackend::with_store(store);
        let root = Path::new("/tmp/ws");
        let cas = backend.content_store(&workspace_backups_root(root));
        let repo = backend.snapshot_repository(&workspace_backups_root(root));
        // 空索引可列出（无快照）
        assert!(repo.list_all().expect("list").is_empty());
        // 扫描空目录得到空结果
        let dir = tempfile::tempdir().expect("tmp");
        let outcome = backend
            .scanner()
            .scan(
                dir.path(),
                cas.as_ref(),
                &port::ScanRules::default(),
                &mut |_| {},
            )
            .expect("scan");
        assert!(outcome.entries.is_empty());
    }
}
