//! Local / Remote 执行位置抽象
//!
//! 两个独立的能力对象：
//! - [`LocalBackend`]：执行位置（`port::Backend`）——进程生命周期 / 安装器
//! - [`LocalBackupStore`]：备份数据（`port::BackupStore`）——扫描 / CAS / 快照
//!
//! 分开是因为二者是不同的轴：「在哪里跑」与「存到哪里」。`service` 分别
//! 注入，互不牵连；任一可独立替换（Remote 执行 / 远端对象存储）。
//!
//! Remote 将来经 transport → RPC → agent 提供同一组 trait 的实现，业务
//! 判定不受影响（`Local / Remote 共享同一套业务逻辑`）。

use port::{
    Backend, BackupStore, ContentStore, InstallerRunner, Scanner, SnapshotRepository,
    SupervisorPort,
};
use std::path::Path;
use std::sync::Arc;

pub mod remote;

/// 本机执行位置
///
/// supervisor / installer 默认是 `runtime` 的实现，也可被宿主启动器替换
/// 以接管进程。
pub struct LocalBackend {
    supervisor: Arc<dyn SupervisorPort>,
    installer: Arc<dyn InstallerRunner>,
}

impl std::fmt::Debug for LocalBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalBackend").finish_non_exhaustive()
    }
}

impl LocalBackend {
    /// 用给定 supervisor / installer 装配
    #[must_use]
    pub fn new(supervisor: Arc<dyn SupervisorPort>, installer: Arc<dyn InstallerRunner>) -> Self {
        Self {
            supervisor,
            installer,
        }
    }

    /// 默认本机装配：`runtime` 的 supervisor / installer
    #[must_use]
    pub fn default_local() -> Self {
        Self::new(
            Arc::new(runtime::LocalSupervisor),
            Arc::new(runtime::LocalInstallerRunner::new()),
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
}

/// 本机备份数据：本地文件系统上的 CAS + 快照 + 扫描
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalBackupStore;

impl BackupStore for LocalBackupStore {
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

    /// 默认本机装配能给出可用的 CAS / 快照 / 扫描
    #[test]
    fn backup_store_builds_working_capabilities() {
        let store = LocalBackupStore;
        let root = Path::new("/tmp/ws");
        let cas = store.content_store(&workspace_backups_root(root));
        let repo = store.snapshot_repository(&workspace_backups_root(root));
        assert!(repo.list_all().expect("list").is_empty());
        let dir = tempfile::tempdir().expect("tmp");
        let outcome = store
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
