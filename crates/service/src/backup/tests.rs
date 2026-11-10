#![allow(
    clippy::unreachable,
    reason = "测试替身用 unreachable! 标记不该被调用的分支"
)]
use super::*;
use crate::registry::ServerRegistry;
use backup::CasStore;
use domain::server::meta::{ServerMeta, ServerName};
use domain::server::settings::SettingKey;
use domain::server::status::ServerStatus;
use domain::server::version::{ServerVariant, VersionId};
use domain::workspace::{Workspace, WorkspaceId, WorkspaceName};
use port::CancellationToken;
use port::{
    Backend, InstallerRunner, RepositoryError, Scanner, ServerRepository, StopOutcome, StopSpec,
    SupervisorError, SupervisorPort, SupervisorStartOutcome, SupervisorStartRequest,
    SupervisorStatus, WorkspaceRepository,
};
use std::sync::Mutex;

// ---- 测试替身 ----

struct MemServers {
    items: Mutex<Vec<ServerMeta>>,
}

impl MemServers {
    fn new() -> Self {
        Self {
            items: Mutex::new(Vec::new()),
        }
    }
}

impl ServerRepository for MemServers {
    fn create(&self, meta: &ServerMeta) -> Result<(), RepositoryError> {
        self.items.lock().expect("lock").push(meta.clone());
        Ok(())
    }
    fn get(&self, name: &ServerName) -> Result<ServerMeta, RepositoryError> {
        self.items
            .lock()
            .expect("lock")
            .iter()
            .find(|m| m.name == *name)
            .cloned()
            .ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))
    }
    fn list(&self) -> Result<Vec<ServerMeta>, RepositoryError> {
        Ok(self.items.lock().expect("lock").clone())
    }
    fn update_core(
        &self,
        name: &ServerName,
        version: &VersionId,
        jar: &domain::server::meta::InstalledJar,
        _launch: Option<&domain::server::launch::LaunchSpec>,
        required_java_major: Option<domain::java::JavaMajor>,
        updated_at: i64,
    ) -> Result<ServerMeta, RepositoryError> {
        let mut items = self.items.lock().expect("lock");
        let meta = items
            .iter_mut()
            .find(|m| m.name == *name)
            .ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))?;
        meta.version = version.clone();
        meta.jar = Some(jar.clone());
        meta.java_major = required_java_major;
        meta.updated_at = updated_at;
        Ok(meta.clone())
    }
    fn update_settings(
        &self,
        name: &ServerName,
        settings: &domain::server::settings::ServerSettings,
        updated_at: i64,
    ) -> Result<ServerMeta, RepositoryError> {
        let mut items = self.items.lock().expect("lock");
        let meta = items
            .iter_mut()
            .find(|m| m.name == *name)
            .ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))?;
        meta.settings = settings.clone();
        meta.updated_at = updated_at;
        Ok(meta.clone())
    }
    fn remove(&self, name: &ServerName) -> Result<(), RepositoryError> {
        let mut items = self.items.lock().expect("lock");
        let before = items.len();
        items.retain(|m| m.name != *name);
        if items.len() == before {
            return Err(RepositoryError::NotFound(name.as_str().to_owned()));
        }
        Ok(())
    }
    fn remove_by_workspace(&self, _id: &WorkspaceId) -> Result<(), RepositoryError> {
        Ok(())
    }
    fn relink(
        &self,
        _name: &ServerName,
        _workspace_id: &WorkspaceId,
        _rel_dir: &Path,
        _updated_at: i64,
    ) -> Result<ServerMeta, RepositoryError> {
        unreachable!("not used")
    }
}

struct MemWorkspaces {
    ws: Workspace,
}

impl WorkspaceRepository for MemWorkspaces {
    fn create(&self, _ws: &Workspace) -> Result<(), RepositoryError> {
        Ok(())
    }
    fn get(&self, id: &WorkspaceId) -> Result<Workspace, RepositoryError> {
        if self.ws.id == *id {
            Ok(self.ws.clone())
        } else {
            Err(RepositoryError::NotFound(id.as_str().to_owned()))
        }
    }
    fn get_by_name(&self, name: &WorkspaceName) -> Result<Workspace, RepositoryError> {
        if self.ws.name == *name {
            Ok(self.ws.clone())
        } else {
            Err(RepositoryError::NotFound(name.as_str().to_owned()))
        }
    }
    fn list(&self) -> Result<Vec<Workspace>, RepositoryError> {
        Ok(vec![self.ws.clone()])
    }
    fn current(&self) -> Result<Option<Workspace>, RepositoryError> {
        Ok(Some(self.ws.clone()))
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

#[derive(Default)]
struct FakeSupervisor {
    status: Mutex<Option<SupervisorStatus>>,
    sent: Mutex<Vec<String>>,
    fail_save_on: Mutex<bool>,
}

impl FakeSupervisor {
    fn running() -> Self {
        let sup = Self::default();
        *sup.status.lock().expect("lock") = Some(SupervisorStatus {
            status: ServerStatus::Running,
            pid: Some(1),
            uptime: None,
            exit_code: None,
        });
        sup
    }

    fn stopped() -> Self {
        Self::default()
    }

    fn sent(&self) -> Vec<String> {
        self.sent.lock().expect("lock").clone()
    }
}

impl SupervisorPort for FakeSupervisor {
    fn start(
        &self,
        _request: SupervisorStartRequest,
    ) -> Result<SupervisorStartOutcome, SupervisorError> {
        unreachable!("not used")
    }
    fn stop(
        &self,
        _server: &port::ServerRef<'_>,
        _spec: StopSpec,
    ) -> Result<StopOutcome, SupervisorError> {
        unreachable!("not used")
    }
    fn status(
        &self,
        _server: &port::ServerRef<'_>,
    ) -> Result<Option<SupervisorStatus>, SupervisorError> {
        Ok(self.status.lock().expect("lock").clone())
    }
    fn send_line(&self, _server: &port::ServerRef<'_>, line: &str) -> Result<(), SupervisorError> {
        if line == CMD_SAVE_ON && *self.fail_save_on.lock().expect("lock") {
            return Err(SupervisorError::Unreachable("injected".to_owned()));
        }
        self.sent.lock().expect("lock").push(line.to_owned());
        Ok(())
    }
    fn shutdown(&self, _server: &port::ServerRef<'_>) -> Result<(), SupervisorError> {
        Ok(())
    }
}

// ---- 夹具 ----

/// 测试用执行位置：只承载假 supervisor
struct TestBackend {
    supervisor: Arc<FakeSupervisor>,
}

struct NoopInstaller;
impl InstallerRunner for NoopInstaller {
    fn run_installer(
        &self,
        _invocation: &port::InstallerInvocation<'_>,
        _output: Option<&dyn port::InstallerOutputSink>,
        _cancel: &CancellationToken,
    ) -> Result<(), port::InstallerError> {
        unreachable!("not used")
    }
}

impl Backend for TestBackend {
    fn supervisor(&self) -> &dyn SupervisorPort {
        self.supervisor.as_ref()
    }

    fn installer(&self) -> &dyn InstallerRunner {
        &NoopInstaller
    }
}

/// 测试用备份数据：真实 CAS / 快照 / 扫描
struct TestStore {
    cas: Arc<dyn ContentStore>,
}

impl port::BackupStore for TestStore {
    fn scanner(&self) -> &dyn Scanner {
        &backup::DirectoryScanner
    }
    fn content_store(&self, _backups_root: &Path) -> Arc<dyn ContentStore> {
        Arc::clone(&self.cas)
    }
    fn snapshot_repository(&self, backups_root: &Path) -> Arc<dyn SnapshotRepository> {
        Arc::new(backup::SnapshotIndex::new(backups_root))
    }
}

struct Fixture {
    service: BackupService,
    registry: Arc<ServerRegistry>,
    supervisor: Arc<FakeSupervisor>,
    server_dir: PathBuf,
    ws_path: PathBuf,
}

impl Fixture {
    fn new(supervisor: FakeSupervisor) -> Self {
        let tmp = Box::leak(Box::new(tempfile::tempdir().expect("tmp")));
        let ws_path = tmp.path().join("ws");
        crate::fs::create_dir_all(ws_path.join("servers")).expect("mkdir");
        let ws = Workspace {
            id: WorkspaceId::parse("550e8400-e29b-41d4-a716-446655440001").expect("uuid"),
            name: WorkspaceName::parse("ws01").expect("name"),
            path: ws_path.clone(),
            is_current: true,
            created_at: 1,
            updated_at: 1,
        };
        let servers: Arc<dyn ServerRepository> = Arc::new(MemServers::new());
        let workspaces: Arc<dyn WorkspaceRepository> = Arc::new(MemWorkspaces { ws });
        let registry = Arc::new(ServerRegistry::new(
            Arc::clone(&servers),
            Arc::clone(&workspaces),
        ));
        let server = registry
            .create(
                None,
                "alpha",
                &VersionId::new("1.21.1"),
                ServerVariant::Vanilla,
                None,
            )
            .expect("create");
        let supervisor = Arc::new(supervisor);
        let backend: Arc<dyn Backend> = Arc::new(TestBackend {
            supervisor: Arc::clone(&supervisor),
        });
        let store: Arc<dyn port::BackupStore> = Arc::new(TestStore {
            cas: Arc::new(CasStore::new(&ws_path.join("backups"))),
        });
        let service = BackupService::new(Arc::clone(&registry), backend, store);
        Self {
            service,
            registry,
            supervisor,
            server_dir: server.dir,
            ws_path,
        }
    }

    fn write(&self, rel: &str, bytes: &[u8]) {
        let path = self.server_dir.join(rel);
        if let Some(parent) = path.parent() {
            crate::fs::create_dir_all(parent).expect("mkdir");
        }
        crate::fs::write(path, bytes).expect("write");
    }

    fn read(&self, rel: &str) -> Vec<u8> {
        crate::fs::read(self.server_dir.join(rel)).expect("read")
    }

    fn backups_root(&self) -> PathBuf {
        self.ws_path.join("backups")
    }

    fn set_scope(&self, raw: &str) {
        let server = self.registry.get("alpha").expect("get");
        let mut settings = server.meta.settings.clone();
        settings.apply(SettingKey::BackupScope, raw).expect("scope");
        self.registry
            .update_settings("alpha", &settings)
            .expect("settings");
    }

    fn create(&self) -> CreateOutcome {
        self.service
            .create(
                "alpha",
                &CreateOptions::default(),
                &CancellationToken::new(),
            )
            .expect("create")
    }

    /// 从快照索引导出完整记录：结果类型只回摘要，清单仍在执行备份的机器上
    fn load(&self, id: &SnapshotId) -> Snapshot {
        use port::SnapshotRepository;
        backup::SnapshotIndex::new(&self.backups_root())
            .get(id)
            .expect("snapshot record")
    }
}

// ---- create ----

#[test]
fn create_backs_up_everything_by_default() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    fx.write("server.properties", b"motd=hi");
    fx.write("world/level.dat", b"level");
    fx.write("world/region/r.0.0.mca", b"region");
    // 运行期文件不进备份
    fx.write("console.log", b"log");
    fx.write("run/mcsup.pid", b"1");

    let outcome = fx.create();
    // 4 = 3 个业务文件 + mcsup.json（恢复为独立目录时要用）
    assert_eq!(outcome.snapshot.files, 4);
    assert_eq!(outcome.snapshot.added, 4);
    assert_eq!(outcome.snapshot.status, SnapshotStatus::Complete);
    assert_eq!(outcome.snapshot.server, "alpha");
    assert!(!outcome.snapshot.server_online);
    assert!(!outcome.snapshot.id.as_str().is_empty());
    // 运行期条目至少被跳过（`run/` 与 `console.log`）
    assert!(outcome.skipped >= 2, "skipped={}", outcome.skipped);
}

#[test]
fn create_rejects_empty_scope() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    // 范围指向一个不存在的顶层项：扫不到任何文件
    let err = fx
        .service
        .create(
            "alpha",
            &CreateOptions {
                scope: Some(BackupScope::from_entries(["nonexistent"]).expect("scope")),
                ..CreateOptions::default()
            },
            &CancellationToken::new(),
        )
        .expect_err("empty");
    assert!(matches!(err, BackupError::EmptyScope), "{err}");
    // 失败不应留下占位目录
    let page = fx.service.list("alpha", Page::new(1, 10)).expect("list");
    assert_eq!(page.total, 0);
}

#[test]
fn create_uses_server_scope_setting() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    fx.write("world/level.dat", b"level");
    fx.write("plugins/a.jar", b"plugin");
    fx.set_scope("world");

    let outcome = fx.create();
    let loaded = fx.load(&outcome.snapshot.id);
    let paths: Vec<&str> = loaded
        .content
        .entries()
        .iter()
        .map(|(p, _)| p.as_str())
        .collect();
    assert_eq!(paths, ["world/level.dat"]);
    assert_eq!(outcome.snapshot.scope.entries(), ["world".to_owned()]);
}

#[test]
fn create_scope_option_overrides_setting() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    fx.write("world/level.dat", b"level");
    fx.write("plugins/a.jar", b"plugin");
    fx.set_scope("world");

    let outcome = fx
        .service
        .create(
            "alpha",
            &CreateOptions {
                scope: Some(BackupScope::all()),
                ..CreateOptions::default()
            },
            &CancellationToken::new(),
        )
        .expect("create");
    // world + plugins + mcsup.json
    assert_eq!(outcome.snapshot.files, 3);
}

/// 增量：未改动的文件走复用提示，第二次备份不新增对象
#[test]
fn second_backup_reuses_unchanged_files() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    fx.write("world/level.dat", b"level");
    fx.write("mods/big.jar", b"jar-bytes");
    let first = fx.create();
    let second = fx.create();

    assert_eq!(second.snapshot.parent.as_ref(), Some(&first.snapshot.id));
    assert_eq!(second.snapshot.added, 0);
    assert_eq!(second.snapshot.added_bytes, 0);
    // world/level.dat + mods/big.jar + mcsup.json 全部复用
    assert_eq!(second.snapshot.reused, 3);
    assert!(second.snapshot.is_redundant());
}

#[test]
fn changed_file_produces_new_object() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    fx.write("world/level.dat", b"v1");
    let first = fx.create();
    let first_loaded = fx.load(&first.snapshot.id);
    fx.write("world/level.dat", b"v2-different");

    // 复用提示依赖 (size, mtime)：显式确认这两项确实变了
    let before = first_loaded
        .content
        .entries()
        .iter()
        .find(|(path, _)| path.as_str() == "world/level.dat")
        .expect("level.dat before");
    let meta = crate::fs::symlink_metadata(fx.server_dir.join("world/level.dat")).expect("meta");
    assert_eq!(meta.len(), 12, "size must change so the hint misses");
    assert_eq!(before.1.as_str().len(), 64);

    let second = fx.create();
    assert_eq!(second.snapshot.added, 1);
    let second_loaded = fx.load(&second.snapshot.id);
    let changed = second_loaded
        .content
        .entries()
        .iter()
        .find(|(path, _)| path.as_str() == "world/level.dat")
        .expect("level.dat");
    assert_ne!(changed.1, before.1);
}

/// 增量必须记墓碑：删掉的文件不进新快照，但要被记录为已删除
#[test]
fn deleted_file_is_recorded_as_tombstone() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    fx.write("world/level.dat", b"level");
    fx.write("world/old.dat", b"old");
    let first = fx.create();
    crate::fs::remove_file(fx.server_dir.join("world/old.dat")).expect("rm");

    let second = fx.create();
    // level.dat + mcsup.json 仍在，old.dat 进墓碑
    assert_eq!(second.snapshot.files, 2);
    let second_loaded = fx.load(&second.snapshot.id);
    let deleted: Vec<&str> = second_loaded
        .content
        .deleted()
        .iter()
        .map(RelPath::as_str)
        .collect();
    assert_eq!(deleted, ["world/old.dat"]);
    assert_eq!(second.snapshot.parent.as_ref(), Some(&first.snapshot.id));
}

/// 运行中备份：save-off → save-all → save-on，顺序与次数都对
#[test]
fn running_backup_toggles_save_around_scan() {
    let fx = Fixture::new(FakeSupervisor::running());
    fx.write("world/level.dat", b"level");

    let outcome = fx
        .service
        .create(
            "alpha",
            &CreateOptions {
                flush_wait_ms: Some(0),
                ..CreateOptions::default()
            },
            &CancellationToken::new(),
        )
        .expect("create");
    assert!(outcome.snapshot.server_online);
    assert_eq!(
        fx.supervisor.sent(),
        vec![
            CMD_SAVE_OFF.to_owned(),
            CMD_SAVE_ALL.to_owned(),
            CMD_SAVE_ON.to_owned()
        ]
    );
}

/// 扫描失败时 `save-on` 必须仍然发出——否则服务器永久停写
#[test]
fn save_on_is_sent_even_when_backup_fails() {
    let fx = Fixture::new(FakeSupervisor::running());
    // 范围为空：走到 EmptyScope 提前返回
    let err = fx
        .service
        .create(
            "alpha",
            &CreateOptions {
                scope: Some(BackupScope::from_entries(["nonexistent"]).expect("scope")),
                flush_wait_ms: Some(0),
                ..CreateOptions::default()
            },
            &CancellationToken::new(),
        )
        .expect_err("empty");
    assert!(matches!(err, BackupError::EmptyScope), "{err}");
    let sent = fx.supervisor.sent();
    assert_eq!(sent.first().map(String::as_str), Some(CMD_SAVE_OFF));
    assert_eq!(
        sent.last().map(String::as_str),
        Some(CMD_SAVE_ON),
        "save-on must be sent on the failure path too: {sent:?}"
    );
}

#[test]
fn label_is_trimmed_and_validated() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    fx.write("a", b"1");
    let outcome = fx
        .service
        .create(
            "alpha",
            &CreateOptions {
                label: Some("  before-upgrade  ".to_owned()),
                ..CreateOptions::default()
            },
            &CancellationToken::new(),
        )
        .expect("create");
    assert_eq!(outcome.snapshot.label.as_deref(), Some("before-upgrade"));

    let too_long = "x".repeat(MAX_LABEL_LEN + 1);
    let err = fx
        .service
        .create(
            "alpha",
            &CreateOptions {
                label: Some(too_long),
                ..CreateOptions::default()
            },
            &CancellationToken::new(),
        )
        .expect_err("label");
    assert!(matches!(err, BackupError::InvalidLabel { .. }), "{err}");
}

#[test]
fn blank_label_is_none() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    fx.write("a", b"1");
    let outcome = fx
        .service
        .create(
            "alpha",
            &CreateOptions {
                label: Some("   ".to_owned()),
                ..CreateOptions::default()
            },
            &CancellationToken::new(),
        )
        .expect("create");
    assert!(outcome.snapshot.label.is_none());
}

#[test]
fn cas_is_shared_across_snapshots_of_the_workspace() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    fx.write("a", b"same");
    fx.write("b", b"same");
    fx.create();
    // 两个文件同内容 + mcsup.json：CAS 里只有 2 个对象
    let objects = crate::fs::read_dir(fx.backups_root().join("cas"))
        .expect("cas")
        .flat_map(|a| crate::fs::read_dir(a.expect("a").path()).expect("b"))
        .flat_map(|b| crate::fs::read_dir(b.expect("b").path()).expect("c"))
        .count();
    assert_eq!(objects, 2);
}

// ---- list ----

#[test]
fn list_is_newest_first_and_paginated() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    for i in 0..5 {
        fx.write("f", format!("v{i}").as_bytes());
        fx.create();
    }
    let page = fx.service.list("alpha", Page::new(1, 2)).expect("list");
    assert_eq!(page.total, 5);
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.pages(), 3);
    // 倒序：最新在前
    assert!(page.items[0].created_at >= page.items[1].created_at);

    let last = fx.service.list("alpha", Page::new(3, 2)).expect("list");
    assert_eq!(last.items.len(), 1);

    let beyond = fx.service.list("alpha", Page::new(9, 2)).expect("list");
    assert!(beyond.items.is_empty());
    assert_eq!(beyond.total, 5);
}

#[test]
fn list_unknown_server_fails() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    let err = fx
        .service
        .list("nope", Page::new(1, 10))
        .expect_err("missing");
    assert!(matches!(
        err,
        BackupError::Registry(ServerRegistryError::NotFound(_))
    ));
}

// ---- restore ----

#[test]
fn in_place_restore_refuses_running_server() {
    let fx = Fixture::new(FakeSupervisor::running());
    fx.write("world/level.dat", b"v1");
    let outcome = fx
        .service
        .create(
            "alpha",
            &CreateOptions {
                flush_wait_ms: Some(0),
                ..CreateOptions::default()
            },
            &CancellationToken::new(),
        )
        .expect("create");

    let err = fx
        .service
        .restore(
            "alpha",
            outcome.snapshot.id.as_str(),
            &RestoreTarget::InPlace,
            &CancellationToken::new(),
        )
        .expect_err("running");
    assert!(
        matches!(err, BackupError::RestoreWhileRunning { .. }),
        "{err}"
    );
}

#[test]
fn in_place_restore_rewrites_files_and_keeps_safety_snapshot() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    fx.write("world/level.dat", b"v1");
    fx.write("server.properties", b"motd=old");
    let outcome = fx.create();
    let id = outcome.snapshot.id.clone();

    // 改掉现场
    fx.write("world/level.dat", b"v2");
    fx.write("server.properties", b"motd=new");

    let restored = fx
        .service
        .restore(
            "alpha",
            id.as_str(),
            &RestoreTarget::InPlace,
            &CancellationToken::new(),
        )
        .expect("restore");
    assert_eq!(fx.read("world/level.dat"), b"v1");
    assert_eq!(fx.read("server.properties"), b"motd=old");
    // world/level.dat + server.properties + mcsup.json
    assert_eq!(restored.files, 3);
    assert!(restored.safety_snapshot.is_some());
    // 安全快照记录了恢复前的现场
    let page = fx.service.list("alpha", Page::new(1, 10)).expect("list");
    assert_eq!(page.total, 2);
    // 暂存区不应残留
    assert!(!fx.server_dir.join(".mcsup-restore").exists());
}

/// 墓碑：恢复后，快照里「当时不存在」的文件必须被删掉，不能复活
#[test]
fn restore_removes_files_deleted_after_snapshot() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    fx.write("world/level.dat", b"level");
    let first = fx.create();
    fx.write("world/new.dat", b"new");
    fx.create();

    let restored = fx
        .service
        .restore(
            "alpha",
            first.snapshot.id.as_str(),
            &RestoreTarget::InPlace,
            &CancellationToken::new(),
        )
        .expect("restore");
    assert_eq!(restored.deleted, 1);
    assert!(!fx.server_dir.join("world/new.dat").exists());
}

#[test]
fn restore_to_new_server_leaves_original_untouched() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    fx.write("world/level.dat", b"v1");
    let original_id = fx.registry.get("alpha").expect("get").meta.id;
    let outcome = fx.create();
    fx.write("world/level.dat", b"v2");

    let restored = fx
        .service
        .restore(
            "alpha",
            outcome.snapshot.id.as_str(),
            &RestoreTarget::NewServer {
                name: ServerName::parse("copy").expect("name"),
            },
            &CancellationToken::new(),
        )
        .expect("restore");
    // 原服务器不动
    assert_eq!(fx.read("world/level.dat"), b"v2");
    assert!(restored.safety_snapshot.is_none());
    let copy = restored.destination.join("world/level.dat");
    assert_eq!(crate::fs::read(copy).expect("read"), b"v1");

    // 身份不跟着内容复制：新目录必须拿到新 id，名称改成新服务名，
    // 否则 `server link` 会因 id 冲突一直失败
    let marker = read_marker(&restored.destination);
    assert_ne!(marker.0, original_id.as_str());
    assert_eq!(marker.1, "copy");

    // 验收点：恢复出来的目录必须真的能注册（快照里那份 marker 会让
    // `link` 撞上源服务器的 id）
    let linked = fx
        .registry
        .link(&restored.destination, None, Some("ws01"))
        .expect("link restored copy");
    assert_eq!(linked.server.meta.name.as_str(), "copy");
}

/// 读回 `mcsup.json` 的 (id, name)
fn read_marker(dir: &Path) -> (String, String) {
    let text = crate::fs::read_to_string(dir.join("mcsup.json")).expect("marker");
    let marker: serde_json::Value = serde_json::from_str(&text).expect("json");
    (
        marker["id"].as_str().expect("id").to_owned(),
        marker["name"].as_str().expect("name").to_owned(),
    )
}

#[test]
fn restore_to_existing_server_name_is_rejected() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    fx.write("a", b"1");
    let outcome = fx.create();
    let err = fx
        .service
        .restore(
            "alpha",
            outcome.snapshot.id.as_str(),
            &RestoreTarget::NewServer {
                name: ServerName::parse("alpha").expect("name"),
            },
            &CancellationToken::new(),
        )
        .expect_err("exists");
    assert!(matches!(err, BackupError::TargetExists { .. }), "{err}");
}

/// 目标目录存在但未注册时也拒绝：合并写入会把残留文件混进「快照副本」
#[test]
fn restore_to_leftover_directory_is_rejected() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    fx.write("a", b"1");
    let outcome = fx.create();
    let leftover = fx.ws_path.join("servers").join("copy");
    crate::fs::create_dir_all(&leftover).expect("mkdir");
    crate::fs::write(leftover.join("stale.txt"), b"x").expect("write");

    let err = fx
        .service
        .restore(
            "alpha",
            outcome.snapshot.id.as_str(),
            &RestoreTarget::NewServer {
                name: ServerName::parse("copy").expect("name"),
            },
            &CancellationToken::new(),
        )
        .expect_err("exists");
    assert!(matches!(err, BackupError::TargetExists { .. }), "{err}");
    // 残留目录原样不动
    assert_eq!(
        crate::fs::read(leftover.join("stale.txt")).expect("read"),
        b"x"
    );
}

/// 恢复落点被符号链接指到目录外时必须拒绝
///
/// 领域层挡得住 `..`，挡不住磁盘上已有的链接。只校验「拼出来的路径」不够，
/// 要把父目录规范化后再比一次。
#[cfg(unix)]
#[test]
fn in_place_restore_refuses_symlink_escape() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    fx.write("world/level.dat", b"v1");
    let outcome = fx.create();
    // 快照之后把 world 换成指向目录外的链接
    crate::fs::remove_dir_all(fx.server_dir.join("world")).expect("rm");
    let outside = fx.ws_path.join("outside");
    crate::fs::create_dir_all(&outside).expect("mkdir");
    std::os::unix::fs::symlink(&outside, fx.server_dir.join("world")).expect("symlink");

    let err = fx
        .service
        .restore(
            "alpha",
            outcome.snapshot.id.as_str(),
            &RestoreTarget::InPlace,
            &CancellationToken::new(),
        )
        .expect_err("escapes");
    assert!(
        err.to_string().contains("escapes the server directory"),
        "{err}"
    );
    // 目录外不能出现文件
    assert!(!outside.join("level.dat").exists());
}

#[test]
fn restore_to_traversal_name_is_rejected_before_writing() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    fx.write("a", b"1");
    // `RestoreTarget::NewServer` 只接受 `ServerName`，非法名字在入口
    // 就构造不出来——这里锁定「哪些名字必须被拒绝」这一层契约。
    for name in ["../escape", "..\\escape", "/abs", "C:\\abs", "a/b", ""] {
        assert!(
            ServerName::parse(name).is_err(),
            "{name} 必须被 ServerName 拒绝"
        );
    }
    assert!(
        !fx.ws_path.join("escape").exists(),
        "恢复目标逃出 servers/ 目录"
    );
}

/// 磁盘上已有但未注册的目录同样不能作为恢复目标：否则旧文件会混进恢复结果
#[test]
fn restore_to_existing_unregistered_dir_is_rejected() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    fx.write("a", b"1");
    let outcome = fx.create();
    crate::fs::create_dir(fx.ws_path.join("servers").join("copy")).expect("mkdir");
    let err = fx
        .service
        .restore(
            "alpha",
            outcome.snapshot.id.as_str(),
            &RestoreTarget::NewServer {
                name: ServerName::parse("copy").expect("name"),
            },
            &CancellationToken::new(),
        )
        .expect_err("dir exists");
    assert!(matches!(err, BackupError::TargetExists(_)), "{err}");
}

#[test]
fn restore_unknown_snapshot_fails() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    let err = fx
        .service
        .restore(
            "alpha",
            "20990101t000000-aaaaaa",
            &RestoreTarget::InPlace,
            &CancellationToken::new(),
        )
        .expect_err("missing");
    assert!(matches!(err, BackupError::SnapshotNotFound(_)), "{err}");

    // 非法 id 也按「不存在」处理，不做路径猜测
    let err = fx
        .service
        .restore(
            "alpha",
            "../../etc",
            &RestoreTarget::InPlace,
            &CancellationToken::new(),
        )
        .expect_err("invalid");
    assert!(matches!(err, BackupError::SnapshotNotFound(_)), "{err}");
}

// ---- delete ----

#[test]
fn delete_removes_snapshot_and_reports_orphans() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    fx.write("a", b"only-here");
    let first = fx.create();
    fx.write("a", b"changed");
    let second = fx.create();

    let outcome = fx
        .service
        .delete("alpha", first.snapshot.id.as_str())
        .expect("delete");
    assert_eq!(outcome.snapshot.id, first.snapshot.id);
    assert_eq!(outcome.orphaned, 1);

    let page = fx.service.list("alpha", Page::new(1, 10)).expect("list");
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].id, second.snapshot.id);
}

#[test]
fn delete_is_reported_for_missing_snapshot() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    let err = fx
        .service
        .delete("alpha", "20990101t000000-aaaaaa")
        .expect_err("missing");
    assert!(matches!(err, BackupError::SnapshotNotFound(_)), "{err}");
}

// ---- verify ----

#[test]
fn verify_reports_healthy_snapshot() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    fx.write("world/level.dat", b"level");
    fx.write("a", b"1");
    let outcome = fx.create();

    let mut last = 0;
    let report = fx
        .service
        .verify("alpha", outcome.snapshot.id.as_str(), &mut |done, total| {
            assert!(done <= total);
            last = done;
        })
        .expect("verify");
    assert!(report.is_healthy());
    // world/level.dat + a + mcsup.json
    assert_eq!(report.objects, 3);
    assert_eq!(last, 3);
    // 字节数 = 各对象大小之和（mcsup.json 大小随内容变化，不做硬编码）
    let mut expected = 0u64;
    for (_, hash) in fx.load(&outcome.snapshot.id).content.entries() {
        expected += crate::fs::metadata(CasStore::new(&fx.backups_root()).object_path(hash))
            .expect("object")
            .len();
    }
    assert_eq!(report.bytes, expected);
    assert!(report.bytes >= 6);
}

/// 对象被删 / 改：校验必须报出，且不把整次校验当失败
#[test]
fn verify_detects_missing_and_tampered_objects() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    fx.write("a", b"alpha");
    fx.write("b", b"bravo");
    let outcome = fx.create();
    let hashes: Vec<ContentHash> = fx
        .load(&outcome.snapshot.id)
        .content
        .referenced_hashes()
        .cloned()
        .collect();
    // a + b + mcsup.json
    assert_eq!(hashes.len(), 3);

    let cas = CasStore::new(&fx.backups_root());
    crate::fs::remove_file(cas.object_path(&hashes[0])).expect("rm");
    crate::fs::write(cas.object_path(&hashes[1]), b"tampered").expect("write");

    let report = fx
        .service
        .verify("alpha", outcome.snapshot.id.as_str(), &mut |_, _| {})
        .expect("verify");
    assert!(!report.is_healthy());
    assert_eq!(report.broken.len(), 2);
}

// ---- prune ----

#[test]
fn prune_keep_removes_oldest_and_gc_orphans() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    fx.write("f", b"v0");
    fx.create();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    fx.write("f", b"v1");
    fx.create();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    fx.write("f", b"v2");
    fx.create();

    let outcome = fx
        .service
        .prune(
            "alpha",
            RetentionInput {
                keep: Some(1),
                older_than_secs: None,
            },
            false,
        )
        .expect("prune");
    assert_eq!(outcome.removed.len(), 2);
    assert!(!outcome.dry_run);
    // 两份旧版本的内容各自成为孤儿对象
    assert_eq!(outcome.objects_reclaimed, 2);
    assert!(outcome.bytes_reclaimed > 0);

    let page = fx.service.list("alpha", Page::new(1, 10)).expect("list");
    assert_eq!(page.total, 1);
    // 列表只回摘要：清单留在对端 / 索引里，`files` 仍可核对（f + mcsup.json）
    assert_eq!(page.items[0].files, 2);
    assert_eq!(
        fx.load(&page.items[0].id).content.entries()[0]
            .1
            .as_str()
            .len(),
        64
    );
}

#[test]
fn prune_rejects_conflicting_retention() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    fx.write("f", b"v0");
    fx.create();

    // 两个都给：必须报冲突，且**一份都不删**（不能静默择一执行）
    let err = fx
        .service
        .prune(
            "alpha",
            RetentionInput {
                keep: Some(1),
                older_than_secs: Some(3600),
            },
            false,
        )
        .expect_err("conflict must be rejected");
    assert!(matches!(err, BackupError::ConflictingRetention), "{err:?}");
    assert!(err.kind().is_some(), "入口需要 kind() 才能本地化文案");

    let page = fx.service.list("alpha", Page::new(1, 10)).expect("list");
    assert_eq!(page.total, 1, "冲突时不得删除任何快照");
}

#[test]
fn prune_dry_run_changes_nothing() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    fx.write("f", b"v0");
    fx.create();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    fx.write("f", b"v1");
    fx.create();

    let outcome = fx
        .service
        .prune(
            "alpha",
            RetentionInput {
                keep: Some(1),
                older_than_secs: None,
            },
            true,
        )
        .expect("prune");
    assert!(outcome.dry_run);
    assert_eq!(outcome.removed.len(), 1);
    assert_eq!(outcome.objects_reclaimed, 1);

    // 实际什么都没删；CAS 里是 f(v0) + f(v1) + mcsup.json
    let page = fx.service.list("alpha", Page::new(1, 10)).expect("list");
    assert_eq!(page.total, 2);
    assert_eq!(
        CasStore::new(&fx.backups_root())
            .list()
            .expect("list")
            .len(),
        3
    );
}

/// 别的服务器的快照仍在引用时不能回收：CAS 按工作区共享
#[test]
fn prune_keeps_objects_referenced_by_other_servers() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    fx.registry
        .create(
            None,
            "beta",
            &VersionId::new("1.21.1"),
            ServerVariant::Vanilla,
            None,
        )
        .expect("create beta");
    let beta_dir = fx.ws_path.join("servers").join("beta");
    crate::fs::create_dir_all(&beta_dir).expect("mkdir");
    crate::fs::write(beta_dir.join("shared.bin"), b"identical-bytes").expect("write");

    // alpha 备份同样内容 → 与 beta 共用同一个 CAS 对象
    fx.write("shared.bin", b"identical-bytes");
    fx.create();

    let beta = fx
        .service
        .create("beta", &CreateOptions::default(), &CancellationToken::new())
        .expect("beta backup");
    // beta 读到 shared.bin + 自己的 mcsup.json（内容与 alpha 的不同）
    assert_eq!(beta.snapshot.added, 2);

    // 删掉 alpha 的全部快照并 GC
    fx.service
        .prune(
            "alpha",
            RetentionInput {
                keep: Some(0),
                older_than_secs: None,
            },
            false,
        )
        .expect("prune");
    // beta 的对象必须还在，且 beta 的快照仍可校验通过
    let report = fx
        .service
        .verify("beta", beta.snapshot.id.as_str(), &mut |_, _| {})
        .expect("verify");
    assert!(report.is_healthy(), "broken: {:?}", report.broken);
}

#[test]
fn prune_without_policy_only_collects_garbage() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    fx.write("a", b"1");
    let outcome = fx.create();
    // 手工放入一个无人引用的对象
    let cas = CasStore::new(&fx.backups_root());
    let mut cursor = std::io::Cursor::new(b"orphan".to_vec());
    let (orphan, _) = cas
        .put(&mut cursor, &RelPath::parse("orphan").expect("rel"))
        .expect("put");

    let pruned = fx
        .service
        .prune("alpha", RetentionInput::default(), false)
        .expect("prune");
    assert!(pruned.removed.is_empty());
    assert_eq!(pruned.objects_reclaimed, 1);
    assert!(cas.stat(&orphan).expect("stat").is_none());
    // 被引用的对象还在
    let page = fx.service.list("alpha", Page::new(1, 10)).expect("list");
    assert_eq!(page.total, 1);
    assert!(
        fx.service
            .verify("alpha", outcome.snapshot.id.as_str(), &mut |_, _| {})
            .expect("verify")
            .is_healthy()
    );
}

#[test]
fn prune_fails_closed_when_snapshot_record_is_corrupt() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    fx.write("a", b"only-here");
    let outcome = fx.create();
    let hash = fx
        .load(&outcome.snapshot.id)
        .content
        .referenced_hashes()
        .next()
        .expect("hash")
        .clone();
    let record =
        backup::snapshot_dir(&fx.ws_path, &outcome.snapshot.id).join(backup::SNAPSHOT_FILE);
    crate::fs::write(&record, b"{not json").expect("corrupt");

    let err = fx
        .service
        .prune(
            "alpha",
            RetentionInput {
                keep: Some(0),
                older_than_secs: None,
            },
            false,
        )
        .expect_err("损坏记录必须让 prune fail closed");
    assert!(matches!(err, BackupError::Store(_)), "{err}");
    let cas = CasStore::new(&fx.backups_root());
    assert!(
        cas.stat(&hash).expect("stat").is_some(),
        "GC 不得删掉仍被引用的对象"
    );
}

#[test]
fn prune_older_than_uses_age() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    fx.write("a", b"1");
    fx.create();
    // 一小时前之前的所有快照（刚刚创建的不会被删）
    let outcome = fx
        .service
        .prune(
            "alpha",
            RetentionInput {
                keep: None,
                older_than_secs: Some(3600),
            },
            false,
        )
        .expect("prune");
    assert!(outcome.removed.is_empty());
}

// ---- 辅助函数 ----

#[test]
fn snapshot_id_is_sortable_and_lowercase() {
    let id = next_snapshot_id(1_700_000_000).expect("id");
    assert!(id.as_str().starts_with("2023111"), "{}", id.as_str());
    assert!(id.as_str().contains('t'));
    assert!(SnapshotId::parse(id.as_str()).is_ok());
    // 同秒不同 id：随机后缀避免撞名
    assert_ne!(id, next_snapshot_id(1_700_000_000).expect("id"));
}

#[test]
fn utc_stamp_matches_known_epochs() {
    assert_eq!(utc_stamp(0), "19700101t000000");
    assert_eq!(utc_stamp(1_700_000_000), "20231114t221320");
}

#[test]
fn server_workspace_walks_up_two_levels() {
    let dir = Path::new("/ws/servers/alpha");
    assert_eq!(server_workspace(dir, Path::new("alpha")), Path::new("/ws"));
    // 层级不足时退回自身
    let shallow = Path::new("/alpha");
    assert_eq!(
        server_workspace(shallow, Path::new("alpha")),
        Path::new("/alpha")
    );
}

#[test]
fn error_kinds_wrap_and_expose() {
    let err = BackupError::SnapshotNotFound("x".to_owned());
    assert!(matches!(
        err.kind(),
        Some(BackupErrorKind::SnapshotNotFound { id: "x" })
    ));
    let err = BackupError::RestoreWhileRunning {
        name: "alpha".to_owned(),
    };
    assert!(matches!(
        err.kind(),
        Some(BackupErrorKind::RestoreWhileRunning { name: "alpha" })
    ));
    // 包装变体不暴露 kind，由入口沿错误链继续找
    let err = BackupError::Registry(ServerRegistryError::NotFound("x".to_owned()));
    assert!(err.kind().is_none());
}

#[test]
fn service_debug_is_opaque() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    assert!(format!("{:?}", fx.service).contains("BackupService"));
}

#[test]
fn scope_of_reads_server_settings() {
    let fx = Fixture::new(FakeSupervisor::stopped());
    assert!(fx.service.scope_of("alpha").expect("scope").is_all());
    fx.set_scope("world,mods");
    let scope = fx.service.scope_of("alpha").expect("scope");
    assert_eq!(scope.entries(), ["mods".to_owned(), "world".to_owned()]);
    assert!(fx.service.scope_of("nope").is_none());
}

/// 失败也是事实：create 失败必须发 `BackupFailed`
#[test]
fn failed_create_publishes_backup_failed_event() {
    #[derive(Default)]
    struct Sink {
        events: Mutex<Vec<domain::server::event::DomainEvent>>,
    }
    impl port::EventSink for Sink {
        fn publish(&self, event: domain::server::event::DomainEvent) {
            self.events.lock().expect("lock").push(event);
        }
    }

    let supervisor = Arc::new(FakeSupervisor::stopped());
    let tmp = Box::leak(Box::new(tempfile::tempdir().expect("tmp")));
    let ws_path = tmp.path().join("ws");
    crate::fs::create_dir_all(ws_path.join("servers")).expect("mkdir");
    let ws = Workspace {
        id: WorkspaceId::parse("550e8400-e29b-41d4-a716-446655440001").expect("uuid"),
        name: WorkspaceName::parse("ws01").expect("name"),
        path: ws_path.clone(),
        is_current: true,
        created_at: 1,
        updated_at: 1,
    };
    let servers: Arc<dyn ServerRepository> = Arc::new(MemServers::new());
    let workspaces: Arc<dyn WorkspaceRepository> = Arc::new(MemWorkspaces { ws });
    let registry = Arc::new(ServerRegistry::new(
        Arc::clone(&servers),
        Arc::clone(&workspaces),
    ));
    registry
        .create(
            None,
            "alpha",
            &VersionId::new("1.21.1"),
            ServerVariant::Vanilla,
            None,
        )
        .expect("create server");
    let backend: Arc<dyn Backend> = Arc::new(TestBackend {
        supervisor: Arc::clone(&supervisor),
    });
    let store: Arc<dyn port::BackupStore> = Arc::new(TestStore {
        cas: Arc::new(CasStore::new(&ws_path.join("backups"))),
    });
    let sink = Arc::new(Sink::default());
    let service =
        BackupService::new(Arc::clone(&registry), backend, store).with_events(sink.clone());

    // 范围指向不存在的顶层项 → EmptyScope
    let err = service
        .create(
            "alpha",
            &CreateOptions {
                scope: Some(BackupScope::from_entries(["nonexistent"]).expect("scope")),
                ..CreateOptions::default()
            },
            &CancellationToken::new(),
        )
        .expect_err("empty");
    assert!(matches!(err, BackupError::EmptyScope), "{err}");
    let events = sink.events.lock().expect("lock").clone();
    assert!(
        events.iter().any(|event| matches!(
            event,
            domain::server::event::DomainEvent::BackupFailed { name, .. } if name == "alpha"
        )),
        "{events:?}"
    );
}
