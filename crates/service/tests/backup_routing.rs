//! 备份路由：远端条目整段转发到 `RemoteBackupPort`，本地照旧走本地存储
//!
//! 用最小夹具：`MemServerRepo` 外套一个「按名字标远端来源」的组合仓库 +
//! 记录调用的 mock 执行器。断言的是 `BackupService` 的分叉与透传，不是
//! mock 本身。

#![allow(clippy::expect_used)]

use domain::backup::{Page, RestoreTarget, RetentionPolicy};
use domain::server::meta::{ServerId, ServerName};
use port::{
    RemoteBackupCreate, RemoteBackupError, RemoteBackupPort, RemoteCreateOutcome,
    RemoteDeleteOutcome, RemotePruneOutcome, RemoteRestoreOutcome, RemoteSnapshotPage,
    RemoteVerifyOutcome, RepositoryError, Sourced,
};
use service::{BackupService, CreateOptions, ServerRegistry};
use std::sync::{Arc, Mutex};
use testkit::{ServiceFixture, server_meta};

/// 测试依赖：CAS / 快照 / 扫描的真实实现
use backup as _;

/// 远端条目在夹具里的固定身份（避免与本地夹具撞车）
const REMOTE_ID: &str = "110e8400-e29b-41d4-a716-446655440001";

/// 把「指定名字的服务器」标成远端来源的组合仓库
struct RemoteMarkedRepo {
    inner: Arc<dyn port::ServerRepository>,
    remote_names: Vec<String>,
}

impl RemoteMarkedRepo {
    fn new(inner: Arc<dyn port::ServerRepository>, remote_names: &[&str]) -> Self {
        Self {
            inner,
            remote_names: remote_names.iter().map(|name| (*name).to_owned()).collect(),
        }
    }
}

impl port::ServerRepository for RemoteMarkedRepo {
    fn create(&self, meta: &domain::server::meta::ServerMeta) -> Result<(), RepositoryError> {
        self.inner.create(meta)
    }
    fn get(&self, name: &ServerName) -> Result<domain::server::meta::ServerMeta, RepositoryError> {
        self.inner.get(name)
    }
    fn list(&self) -> Result<Vec<domain::server::meta::ServerMeta>, RepositoryError> {
        self.inner.list()
    }
    fn update_core(
        &self,
        name: &ServerName,
        version: &domain::server::version::VersionId,
        jar: &domain::server::meta::InstalledJar,
        launch: Option<&domain::server::launch::LaunchSpec>,
        required_java_major: Option<domain::java::JavaMajor>,
        updated_at: i64,
    ) -> Result<domain::server::meta::ServerMeta, RepositoryError> {
        self.inner
            .update_core(name, version, jar, launch, required_java_major, updated_at)
    }
    fn update_settings(
        &self,
        name: &ServerName,
        settings: &domain::server::settings::ServerSettings,
        updated_at: i64,
    ) -> Result<domain::server::meta::ServerMeta, RepositoryError> {
        self.inner.update_settings(name, settings, updated_at)
    }
    fn remove(&self, name: &ServerName) -> Result<(), RepositoryError> {
        self.inner.remove(name)
    }
    fn remove_by_workspace(
        &self,
        workspace_id: &domain::workspace::WorkspaceId,
    ) -> Result<(), RepositoryError> {
        self.inner.remove_by_workspace(workspace_id)
    }
    fn relink(
        &self,
        name: &ServerName,
        workspace_id: &domain::workspace::WorkspaceId,
        rel_dir: &std::path::Path,
        updated_at: i64,
    ) -> Result<domain::server::meta::ServerMeta, RepositoryError> {
        self.inner.relink(name, workspace_id, rel_dir, updated_at)
    }
    fn list_sourced(
        &self,
    ) -> Result<Vec<Sourced<domain::server::meta::ServerMeta>>, RepositoryError> {
        Ok(self
            .inner
            .list()?
            .into_iter()
            .map(|meta| {
                if self
                    .remote_names
                    .iter()
                    .any(|name| meta.name.as_str() == name)
                {
                    let remote = port::RemoteName::parse("edge").expect("remote name");
                    Sourced::new(port::Origin::Remote(remote), meta)
                } else {
                    Sourced::local(meta)
                }
            })
            .collect())
    }
}

/// 测试用执行位置：停机的 FakeSupervisor（本地路径不触 supervisor 业务）
struct LocalBackendFixture {
    supervisor: testkit::FakeSupervisor,
}

impl port::Backend for LocalBackendFixture {
    fn supervisor(&self) -> &dyn port::SupervisorPort {
        &self.supervisor
    }
    fn installer(&self) -> &dyn port::InstallerRunner {
        &NoopInstaller
    }
}

struct NoopInstaller;
impl port::InstallerRunner for NoopInstaller {
    fn run_installer(
        &self,
        _java_bin: &std::path::Path,
        _installer: &std::path::Path,
        _dir: &std::path::Path,
        _output: Option<&dyn port::InstallerOutputSink>,
        _cancel: &port::CancellationToken,
    ) -> Result<(), port::InstallerError> {
        unreachable!("not used")
    }
}

/// 真实本地备份能力：真实目录上的 CAS / 快照 / 扫描
struct RealStoreFixture {
    backups_root: std::path::PathBuf,
}

impl port::BackupStore for RealStoreFixture {
    fn scanner(&self) -> &dyn port::Scanner {
        &backup::DirectoryScanner
    }
    fn content_store(&self, _backups_root: &std::path::Path) -> Arc<dyn port::ContentStore> {
        Arc::new(backup::CasStore::new(&self.backups_root))
    }
    fn snapshot_repository(
        &self,
        _backups_root: &std::path::Path,
    ) -> Arc<dyn port::SnapshotRepository> {
        Arc::new(backup::SnapshotIndex::new(&self.backups_root))
    }
}

/// 记录调用的 mock 远端执行器（全部按 `NotFound` 失败，够验证转发与映射）
#[derive(Default)]
struct MockRemote {
    calls: Mutex<Vec<&'static str>>,
}

impl MockRemote {
    fn touch(&self, op: &'static str) {
        self.calls.lock().expect("lock").push(op);
    }

    fn taken(&self) -> Vec<&'static str> {
        self.calls.lock().expect("lock").clone()
    }
}

impl RemoteBackupPort for MockRemote {
    fn create(
        &self,
        _server_id: &ServerId,
        _options: &RemoteBackupCreate,
    ) -> Result<RemoteCreateOutcome, RemoteBackupError> {
        self.touch("create");
        Err(RemoteBackupError::NotFound("mock".to_owned()))
    }
    fn list(
        &self,
        _server_id: &ServerId,
        _page: Page,
    ) -> Result<RemoteSnapshotPage, RemoteBackupError> {
        self.touch("list");
        Err(RemoteBackupError::NotFound("mock".to_owned()))
    }
    fn restore(
        &self,
        _server_id: &ServerId,
        _snapshot_id: &str,
        _to_new: Option<&str>,
    ) -> Result<RemoteRestoreOutcome, RemoteBackupError> {
        self.touch("restore");
        Err(RemoteBackupError::NotFound("mock".to_owned()))
    }
    fn delete(
        &self,
        _server_id: &ServerId,
        _snapshot_id: &str,
    ) -> Result<RemoteDeleteOutcome, RemoteBackupError> {
        self.touch("delete");
        Err(RemoteBackupError::NotFound("mock".to_owned()))
    }
    fn verify(
        &self,
        _server_id: &ServerId,
        _snapshot_id: &str,
    ) -> Result<RemoteVerifyOutcome, RemoteBackupError> {
        self.touch("verify");
        Err(RemoteBackupError::NotFound("mock".to_owned()))
    }
    fn prune(
        &self,
        _server_id: &ServerId,
        _policy: Option<RetentionPolicy>,
        _dry_run: bool,
    ) -> Result<RemotePruneOutcome, RemoteBackupError> {
        self.touch("prune");
        Err(RemoteBackupError::NotFound("mock".to_owned()))
    }
}

/// 夹具：本地一台 + 远端一台（`bravo`，来源 `edge`）
///
/// 备份存储给真实本地 CAS（远端用例不会触到它；本地用例触到它走真路径）。
fn fixture_with_remote() -> (Arc<RemoteMarkedRepo>, ServiceFixture) {
    let mut fixture = ServiceFixture::new();
    fixture.add_server("alpha");
    fixture.server("alpha").write("world/level.dat", b"level");
    let servers = fixture.server_repo();
    // 远端条目：只进仓库列表，目录不存在也无所谓（远端路径不落本地盘）
    let mut meta = server_meta("bravo", fixture.workspace().id.as_str());
    meta.id = ServerId::parse(REMOTE_ID).expect("id");
    servers.create(&meta).expect("insert");
    let combined = Arc::new(RemoteMarkedRepo::new(servers, &["bravo"]));
    (combined, fixture)
}

/// 装配 `BackupService`；`remote` 为 `None` 表示不注入执行器
fn routed_service(
    combined: &Arc<RemoteMarkedRepo>,
    fixture: &ServiceFixture,
    remote: Option<Arc<dyn RemoteBackupPort>>,
) -> BackupService {
    let registry = ServerRegistry::new(
        Arc::clone(combined) as Arc<dyn port::ServerRepository>,
        fixture.workspace_repo(),
    );
    let backend: Arc<dyn port::Backend> = Arc::new(LocalBackendFixture {
        supervisor: testkit::FakeSupervisor::stopped(),
    });
    let store: Arc<dyn port::BackupStore> = Arc::new(RealStoreFixture {
        backups_root: fixture.backups_root(),
    });
    let svc = BackupService::new(Arc::new(registry), backend, store);
    match remote {
        Some(remote) => svc.with_remote_backups(remote),
        None => svc,
    }
}

#[test]
fn remote_entries_forward_to_executor() {
    let (combined, fixture) = fixture_with_remote();
    let mock = Arc::new(MockRemote::default());
    let svc = routed_service(
        &combined,
        &fixture,
        Some(Arc::clone(&mock) as Arc<dyn RemoteBackupPort>),
    );
    let id = ServerId::parse(REMOTE_ID).expect("id");

    let err = svc
        .create(
            "bravo",
            &CreateOptions {
                flush_wait_ms: Some(0),
                ..CreateOptions::default()
            },
        )
        .expect_err("mock always fails");
    assert!(err.to_string().contains("not found"), "{err}");
    assert_eq!(mock.taken(), ["create"]);
    let _ = id;

    let err = svc.list("bravo", Page::new(1, 10)).expect_err("mock");
    assert!(err.to_string().contains("not found"), "{err}");
    let err = svc
        .restore("bravo", "snap", &RestoreTarget::InPlace)
        .expect_err("mock");
    assert!(err.to_string().contains("not found"), "{err}");
    let err = svc.delete("bravo", "snap").expect_err("mock");
    assert!(err.to_string().contains("not found"), "{err}");
    let err = svc
        .verify("bravo", "snap", &mut |_, _| ())
        .expect_err("mock");
    assert!(err.to_string().contains("not found"), "{err}");
    let err = svc
        .prune("bravo", Some(RetentionPolicy::Keep(1)), true)
        .expect_err("mock");
    assert!(err.to_string().contains("not found"), "{err}");
    assert_eq!(
        mock.taken(),
        ["create", "list", "restore", "delete", "verify", "prune"]
    );
}

#[test]
fn remote_without_executor_is_unreachable() {
    let (combined, fixture) = fixture_with_remote();
    let svc = routed_service(&combined, &fixture, None);
    let err = svc
        .create("bravo", &CreateOptions::default())
        .expect_err("no executor");
    assert!(err.to_string().contains("unreachable"), "{err}");
    assert!(err.to_string().contains("'edge'"), "错误应带远端名：{err}");
}

#[test]
fn local_entries_never_touch_executor() {
    let (combined, fixture) = fixture_with_remote();
    let mock = Arc::new(MockRemote::default());
    let svc = routed_service(
        &combined,
        &fixture,
        Some(Arc::clone(&mock) as Arc<dyn RemoteBackupPort>),
    );
    // 本地条目：走本地存储路径（InMemoryCas + 真目录），绝不进执行器
    let _ = svc.create(
        "alpha",
        &CreateOptions {
            flush_wait_ms: Some(0),
            ..CreateOptions::default()
        },
    );
    assert!(mock.taken().is_empty(), "本地不得走远端执行器");
}

#[test]
fn unknown_server_fails_before_routing() {
    let (combined, fixture) = fixture_with_remote();
    let mock = Arc::new(MockRemote::default());
    let svc = routed_service(
        &combined,
        &fixture,
        Some(Arc::clone(&mock) as Arc<dyn RemoteBackupPort>),
    );
    let err = svc
        .create("ghost", &CreateOptions::default())
        .expect_err("ghost");
    assert!(
        err.to_string().contains("not found") || err.to_string().contains("ghost"),
        "{err}"
    );
    assert!(mock.taken().is_empty());
}
