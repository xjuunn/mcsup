//! `DoctorService` 的行为矩阵：每项检查的命中 / 不命中、不吞错、空状态不误报
//!
//! 用真实 `JsonStore` 而不是内存桩：doctor 的判定同时依赖注册表与磁盘，
//! 桩会把「记录与磁盘对不上」这类真实故障抹平。

#![allow(clippy::expect_used)]

use domain::server::meta::{ServerId, ServerMeta, ServerName};
use domain::server::settings::ServerSettings;
use domain::server::version::{ServerVariant, VersionId};
use domain::workspace::{Workspace, WorkspaceId, WorkspaceName};
use port::{ServerRepository, WorkspaceRepository};
use service::{DoctorService, Severity};
use std::path::PathBuf;
use std::sync::Arc;
use storage::JsonStore;

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    store: Arc<JsonStore>,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().to_path_buf();
        let store = Arc::new(JsonStore::open(&root.join("db")).expect("store"));
        Self {
            _tmp: tmp,
            root,
            store,
        }
    }

    fn doctor(&self) -> DoctorService {
        DoctorService::new(self.store.clone(), self.store.clone())
    }

    /// 建一个带 `workspace.toml` 与 `servers/` 的工作目录并注册为 current
    fn workspace(&self, name: &str) -> Workspace {
        let path = self.root.join(name);
        std::fs::create_dir_all(path.join("servers")).expect("mkdir servers");
        let id = WorkspaceId::parse(&uuid::Uuid::new_v4().to_string()).expect("uuid");
        std::fs::write(
            path.join("workspace.toml"),
            format!(
                "schema = 1\nid = \"{}\"\nname = \"{name}\"\ncreated_at = 0\n",
                id.as_str()
            ),
        )
        .expect("marker");
        let now = unix_now();
        let ws = Workspace {
            id,
            name: WorkspaceName::parse(name).expect("name"),
            path,
            is_current: true,
            created_at: now,
            updated_at: now,
        };
        WorkspaceRepository::create(self.store.as_ref(), &ws).expect("create workspace");
        ws
    }

    /// 写一条服务器登记（不建目录，调用方决定磁盘状态）
    fn server(&self, ws: &Workspace, name: &str, rel_dir: &str) -> ServerMeta {
        let now = unix_now();
        let meta = ServerMeta {
            id: ServerId::parse(&uuid::Uuid::new_v4().to_string()).expect("uuid"),
            name: ServerName::parse(name).expect("name"),
            workspace_id: ws.id.clone(),
            rel_dir: PathBuf::from(rel_dir),
            version: VersionId::new("1.21.1"),
            variant: ServerVariant::Vanilla,
            jar: None,
            settings: ServerSettings::default(),
            java_major: None,
            created_at: now,
            updated_at: now,
        };
        ServerRepository::create(self.store.as_ref(), &meta).expect("create server");
        meta
    }

    fn marker(ws: &Workspace) -> PathBuf {
        ws.path.join("workspace.toml")
    }
}

fn kinds(svc: &DoctorService) -> Vec<String> {
    svc.run()
        .expect("doctor")
        .findings()
        .iter()
        .map(|f| f.kind.as_str().to_owned())
        .collect()
}

#[test]
fn empty_registry_does_not_warn_about_missing_current() {
    let fx = Fixture::new();
    let report = fx.doctor().run().expect("doctor");
    assert!(report.is_empty(), "无工作区是合法首次状态");
}

#[test]
fn consistent_registry_reports_nothing() {
    let fx = Fixture::new();
    let ws = fx.workspace("ws1");
    let srv = fx.server(&ws, "alpha", "alpha");
    std::fs::create_dir_all(ws.path.join("servers").join(&srv.rel_dir)).expect("mkdir");

    let report = fx.doctor().run().expect("doctor");
    assert!(report.is_empty(), "{:?}", report.findings());
}

#[test]
fn single_workspace_without_current_reports_warning() {
    let fx = Fixture::new();
    let ws = fx.workspace("ws1");
    // 删登记再以非 current 身份写回：模拟「有工作区但没有当前」
    WorkspaceRepository::remove(fx.store.as_ref(), &ws.id).expect("remove");
    let mut orphan = ws.clone();
    orphan.is_current = false;
    WorkspaceRepository::create(fx.store.as_ref(), &orphan).expect("re-create");

    let report = fx.doctor().run().expect("doctor");
    assert!(!report.has_errors(), "{:?}", report.findings());
    assert!(
        report
            .findings()
            .iter()
            .any(|f| f.kind.as_str() == "no-current-workspace"),
        "{:?}",
        report.findings()
    );
    assert_eq!(report.count(Severity::Warning), 1);
}

#[test]
fn workspace_dir_missing_is_an_error_and_stops_deeper_checks() {
    let fx = Fixture::new();
    let ws = fx.workspace("ws1");
    std::fs::remove_dir_all(&ws.path).expect("rmdir");

    let found = kinds(&fx.doctor());
    assert!(
        found.contains(&"workspace-dir-missing".to_owned()),
        "{found:?}"
    );
    // 目录都不在了，不该再报悬空链接 / 未登记
    assert!(!found.contains(&"dangling-link".to_owned()), "{found:?}");
    assert!(
        !found.contains(&"on-disk-not-registered".to_owned()),
        "{found:?}"
    );
}

#[test]
fn missing_marker_is_an_error() {
    let fx = Fixture::new();
    let ws = fx.workspace("ws1");
    std::fs::remove_file(Fixture::marker(&ws)).expect("rm marker");

    let found = kinds(&fx.doctor());
    assert!(
        found.contains(&"workspace-marker-missing".to_owned()),
        "{found:?}"
    );
}

#[test]
fn marker_id_mismatch_is_an_error() {
    let fx = Fixture::new();
    let ws = fx.workspace("ws1");
    std::fs::write(
        Fixture::marker(&ws),
        "schema = 1\nid = \"someone-else\"\nname = \"ws1\"\ncreated_at = 0\n",
    )
    .expect("tamper");

    let found = kinds(&fx.doctor());
    assert!(
        found.contains(&"workspace-marker-mismatch".to_owned()),
        "{found:?}"
    );
}

#[test]
fn registered_but_missing_is_an_error() {
    let fx = Fixture::new();
    let ws = fx.workspace("ws1");
    fx.server(&ws, "alpha", "alpha");

    let found = kinds(&fx.doctor());
    assert!(
        found.contains(&"registered-but-missing".to_owned()),
        "{found:?}"
    );
}

#[test]
fn on_disk_not_registered_is_a_warning() {
    let fx = Fixture::new();
    let ws = fx.workspace("ws1");
    std::fs::create_dir_all(ws.path.join("servers/orphan")).expect("mkdir");

    let report = fx.doctor().run().expect("doctor");
    assert_eq!(
        report.count(Severity::Warning),
        1,
        "{:?}",
        report.findings()
    );
    assert!(!report.has_errors());
    assert_eq!(report.findings()[0].kind.as_str(), "on-disk-not-registered");
}

#[cfg(unix)]
#[test]
fn dangling_symlink_is_an_error() {
    let fx = Fixture::new();
    let ws = fx.workspace("ws1");
    std::os::unix::fs::symlink(
        fx.root.join("no-such-target"),
        ws.path.join("servers/dangling"),
    )
    .expect("dangling");

    let report = fx.doctor().run().expect("doctor");
    let found: Vec<_> = report.findings().iter().map(|f| f.kind.as_str()).collect();
    assert!(found.contains(&"dangling-link"), "{found:?}");
    assert!(report.has_errors(), "悬空链接必须是 error");
}

#[test]
fn rel_dir_outside_servers_is_rejected() {
    let fx = Fixture::new();
    let ws = fx.workspace("ws1");
    // `..` 会把路径拼到工作区之外，必须先于「磁盘缺失」判出来
    fx.server(&ws, "alpha", "..");

    let found = kinds(&fx.doctor());
    assert!(
        found.contains(&"rel-dir-not-direct-child".to_owned()),
        "{found:?}"
    );
}

#[test]
fn server_record_pointing_to_unknown_workspace_is_reported() {
    let fx = Fixture::new();
    let ws = fx.workspace("ws1");
    let srv = fx.server(&ws, "alpha", "alpha");
    // 让记录指向一个不存在的工作区：`relink` 本身校验存在性，
    // 所以只能绕开写入校验——直接改记录文件，模拟外部损坏
    let dir = fx.root.join("db/servers");
    let mut patched = false;
    for entry in std::fs::read_dir(&dir).expect("read servers").flatten() {
        let text = std::fs::read_to_string(entry.path()).expect("read record");
        if text.contains(srv.name.as_str()) {
            let other = uuid::Uuid::new_v4().to_string();
            let replaced = text.replace(ws.id.as_str(), &other);
            assert_ne!(text, replaced, "记录里应含工作区 id");
            std::fs::write(entry.path(), replaced).expect("write record");
            patched = true;
        }
    }
    assert!(patched, "没找到服务器记录文件");

    let found = kinds(&fx.doctor());
    assert!(
        found.contains(&"orphan-server-record".to_owned()),
        "{found:?}"
    );
}

#[test]
fn findings_are_sorted_errors_first() {
    let fx = Fixture::new();
    let ws = fx.workspace("ws1");
    fx.server(&ws, "alpha", "alpha");
    std::fs::create_dir_all(ws.path.join("servers/orphan")).expect("mkdir");

    let findings = fx.doctor().run().expect("doctor").findings().to_vec();
    assert!(findings.len() >= 2, "{findings:?}");
    assert_eq!(findings[0].severity(), Severity::Error);
    assert_eq!(findings[findings.len() - 1].severity(), Severity::Warning);
}
