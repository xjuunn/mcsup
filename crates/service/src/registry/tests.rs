#![allow(
    clippy::unreachable,
    reason = "测试替身用 unreachable! 标记不该被调用的分支"
)]
use super::*;
use domain::workspace::{Workspace, WorkspaceId, WorkspaceName};
use port::{Origin, ProgressSink, RemoteName, ServerRepository, WorkspaceRepository};
use std::path::Path;

struct MemServers {
    items: std::sync::Mutex<Vec<ServerMeta>>,
    /// 「远端」记录：`list_sourced` 会把它们按另一来源一并返回，
    /// `list()` 则不含——与真实组合仓库的口径一致
    remote: std::sync::Mutex<Option<(Origin, Vec<ServerMeta>)>>,
    /// `list()` 被调用的次数（回归测试用：批量计数只能读一次）
    list_calls: std::sync::atomic::AtomicUsize,
}

impl MemServers {
    fn new() -> Self {
        Self {
            items: std::sync::Mutex::new(Vec::new()),
            remote: std::sync::Mutex::new(None),
            list_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// 挂一组远端记录；允许与本地同名，这正是限定名要消掉的歧义
    fn set_remote(&self, origin: Origin, items: Vec<ServerMeta>) {
        *self.remote.lock().expect("lock") = Some((origin, items));
    }

    fn list_calls(&self) -> usize {
        self.list_calls.load(std::sync::atomic::Ordering::Relaxed)
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
        self.list_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut items = self.items.lock().expect("lock").clone();
        items.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(items)
    }
    fn list_sourced(&self) -> Result<Vec<Sourced<ServerMeta>>, RepositoryError> {
        let mut out: Vec<Sourced<ServerMeta>> =
            self.list()?.into_iter().map(Sourced::local).collect();
        if let Some((origin, items)) = self.remote.lock().expect("lock").clone() {
            out.extend(
                items
                    .into_iter()
                    .map(|meta| Sourced::new(origin.clone(), meta)),
            );
        }
        Ok(out)
    }
    fn update_core(
        &self,
        name: &ServerName,
        version: &VersionId,
        jar: &InstalledJar,
        _launch: Option<&LaunchSpec>,
        required_java_major: Option<domain::java::JavaMajor>,
        updated_at: i64,
    ) -> Result<ServerMeta, RepositoryError> {
        let mut items = self.items.lock().expect("lock");
        let meta = items
            .iter_mut()
            .find(|m| &m.name == name)
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
        settings: &ServerSettings,
        updated_at: i64,
    ) -> Result<ServerMeta, RepositoryError> {
        let mut items = self.items.lock().expect("lock");
        let meta = items
            .iter_mut()
            .find(|m| &m.name == name)
            .ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))?;
        meta.settings = settings.clone();
        meta.updated_at = updated_at;
        Ok(meta.clone())
    }
    fn remove(&self, name: &ServerName) -> Result<(), RepositoryError> {
        let mut items = self.items.lock().expect("lock");
        let before = items.len();
        items.retain(|m| &m.name != name);
        if items.len() == before {
            return Err(RepositoryError::NotFound(name.as_str().to_owned()));
        }
        Ok(())
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
    fn current(&self) -> Result<Option<Workspace>, RepositoryError> {
        Ok(self
            .items
            .lock()
            .expect("lock")
            .iter()
            .find(|w| w.is_current)
            .cloned())
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

fn ws_fixture(root: &Path) -> Workspace {
    let path = root.join("ws");
    crate::fs::create_dir_all(path.join("servers")).expect("mkdir");
    Workspace {
        id: WorkspaceId::parse("550e8400-e29b-41d4-a716-446655440001").expect("uuid"),
        name: WorkspaceName::parse("ws01").expect("valid"),
        path,
        is_current: true,
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
            None,
        )
        .expect("create");
    assert_eq!(out.meta.name.as_str(), "alpha");
    assert!(out.dir.join("mcsup.json").is_file());
    let got = reg.get("alpha").expect("get");
    assert_eq!(got.meta.id, out.meta.id);
}

/// 限定名必须绕过「本地优先」：本地与远端同名时，`remote/name` 取远端那条
///
/// 回归：`get` 曾只认名字，重名时命中本地，导致
/// `remote/foo` 操作了本机的 `foo`——限定名不但没生效，还指错了机器。
#[test]
fn get_with_origin_prefix_picks_that_origin() {
    let tmp = tempfile::tempdir().expect("tmp");
    let ws = ws_fixture(tmp.path());
    let servers = Arc::new(MemServers::new());
    let reg = ServerRegistry::new(servers.clone(), Arc::new(MemWorkspaces::new(ws)));
    // 本地 alpha
    reg.create(
        None,
        "alpha",
        &VersionId::new("1.21.1"),
        ServerVariant::Vanilla,
        None,
    )
    .expect("create");
    // 远端也有一个 alpha，id 不同
    let remote = RemoteName::parse("r1").expect("remote name");
    let mut remote_meta = reg.get("alpha").expect("local alpha").meta.clone();
    remote_meta.id = ServerId::parse("550e8400-e29b-41d4-a716-4466554400ff").expect("uuid");
    remote_meta.version = VersionId::new("9.9.9");
    servers.set_remote(Origin::Remote(remote), vec![remote_meta]);

    // 裸名：本地优先
    let bare = reg.get("alpha").expect("bare");
    assert_eq!(bare.meta.version.as_str(), "1.21.1");

    // 限定远端：取远端那条，不能被本地同名挡住
    let picked = reg.get("r1/alpha").expect("qualified remote");
    assert_eq!(picked.meta.version.as_str(), "9.9.9");

    // 限定本地：取本地那条
    let local = reg.get("local/alpha").expect("qualified local");
    assert_eq!(local.meta.version.as_str(), "1.21.1");

    // 来源里没有这个名字 → NotFound，而不是回落本地
    assert!(matches!(
        reg.get("r1/beta"),
        Err(ServerRegistryError::NotFound(_))
    ));
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
        None,
    )
    .expect("first");
    let err = reg
        .create(
            None,
            "alpha",
            &VersionId::new("1.21.1"),
            ServerVariant::Vanilla,
            None,
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
        None,
    )
    .expect("b");
    reg.create(
        None,
        "alpha",
        &VersionId::new("1.21.1"),
        ServerVariant::Vanilla,
        None,
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

/// 工作区注册表 fixture：当前工作区 `ws01`，路径 `<root>/ws`
fn registry_with_ws(root: &Path) -> ServerRegistry {
    ServerRegistry::new(
        Arc::new(MemServers::new()),
        Arc::new(MemWorkspaces::new(ws_fixture(root))),
    )
}

/// 第二个工作区 fixture：非当前，路径 `<root>/ws2`
fn ws2_fixture(root: &Path) -> Workspace {
    let path = root.join("ws2");
    crate::fs::create_dir_all(path.join("servers")).expect("mkdir");
    Workspace {
        id: WorkspaceId::parse("550e8400-e29b-41d4-a716-446655440002").expect("uuid"),
        name: WorkspaceName::parse("ws02").expect("valid"),
        path,
        is_current: false,
        created_at: 1_700_000_000,
        updated_at: 1_700_000_000,
    }
}

/// 列表页用的批量计数：一次读取按工作区分组，结果与逐行 `count_in` 一致
#[test]
fn counts_by_workspace_matches_per_workspace_totals() {
    let tmp = tempfile::tempdir().expect("tmp");
    let servers = Arc::new(MemServers::new());
    let ws1 = ws_fixture(tmp.path());
    let ws2 = ws2_fixture(tmp.path());
    let reg = ServerRegistry::new(servers.clone(), Arc::new(MemWorkspaces::new(ws1.clone())));

    let meta = |name: &str, ws: &Workspace, n: u8| ServerMeta {
        id: ServerId::parse(&format!("550e8400-e29b-41d4-a716-44665544000{n}")).expect("id"),
        name: ServerName::parse(name).expect("name"),
        workspace_id: ws.id.clone(),
        rel_dir: PathBuf::from(name),
        version: VersionId::new("1.21.1"),
        variant: ServerVariant::Vanilla,
        jar: None,
        launch: None,
        settings: ServerSettings::default(),
        java_major: None,
        created_at: 1,
        updated_at: 1,
    };
    servers.create(&meta("a", &ws1, 1)).expect("a");
    servers.create(&meta("b", &ws1, 2)).expect("b");
    servers.create(&meta("c", &ws2, 3)).expect("c");

    let counts = reg.counts_by_workspace().expect("counts");
    assert_eq!(counts.get(&ws1.id).copied(), Some(2));
    assert_eq!(counts.get(&ws2.id).copied(), Some(1));
    // 批量计数只读一次服务器列表（回归：逐行 count_in 会读 M 次）
    assert_eq!(
        servers.list_calls(),
        1,
        "counts_by_workspace 只能拉一次列表"
    );
    // 与逐行口径一致
    assert_eq!(
        counts.get(&ws1.id).copied(),
        Some(reg.count_in(&ws1.id).expect("count_in"))
    );
}

#[cfg(unix)]
#[test]
fn link_creates_symlink_and_registers() {
    let tmp = tempfile::tempdir().expect("tmp");
    let reg = registry_with_ws(tmp.path());
    let external = tmp.path().join("external");
    crate::fs::create_dir_all(&external).expect("mkdir");

    let out = reg.link(&external, Some("ext"), None).expect("link");

    assert!(out.server.linked);
    assert!(
        out.server
            .dir
            .symlink_metadata()
            .expect("link")
            .file_type()
            .is_symlink()
    );
    assert_eq!(out.server.meta.variant, ServerVariant::Unknown);
    assert_eq!(out.server.meta.version.as_str(), "unknown");
    assert!(out.server.dir.join("mcsup.json").is_file());
}

/// 直接子目录：就地注册，不建符号链接
#[test]
fn link_registers_in_place_for_direct_child() {
    let tmp = tempfile::tempdir().expect("tmp");
    let reg = registry_with_ws(tmp.path());
    let dir = tmp.path().join("ws/servers/nested");
    crate::fs::create_dir_all(&dir).expect("mkdir");

    let out = reg.link(&dir, None, None).expect("in-place");

    assert_eq!(out.server.meta.name.as_str(), "nested");
    assert_eq!(out.server.meta.rel_dir, Path::new("nested"));
    assert!(!out.server.linked);
    assert_eq!(
        out.target,
        crate::fs::canonicalize(&dir).expect("canonical")
    );
    assert!(dir.join("mcsup.json").is_file());
    assert!(
        !dir.symlink_metadata()
            .expect("meta")
            .file_type()
            .is_symlink()
    );
}

/// 恢复场景：create → 注销（磁盘保留）→ link 补回注册并复用 marker 身份
#[test]
fn link_in_place_reuses_id_after_unregister() {
    let tmp = tempfile::tempdir().expect("tmp");
    let reg = registry_with_ws(tmp.path());
    let created = reg
        .create(
            None,
            "test",
            &VersionId::new("1.21.1"),
            ServerVariant::Vanilla,
            None,
        )
        .expect("create");
    reg.remove("test", false, tmp.path()).expect("unregister");

    let out = reg.link(&created.dir, None, None).expect("recover");

    assert_eq!(out.server.meta.id, created.meta.id);
    assert_eq!(out.server.meta.rel_dir, Path::new("test"));
    assert_eq!(out.server.meta.version.as_str(), "unknown");
    assert!(!out.server.linked);
    assert!(created.dir.is_dir());
}

/// 重复就地注册：名称已入库即冲突
#[test]
fn link_in_place_twice_is_already_exists() {
    let tmp = tempfile::tempdir().expect("tmp");
    let reg = registry_with_ws(tmp.path());
    let dir = tmp.path().join("ws/servers/nested");
    crate::fs::create_dir_all(&dir).expect("mkdir");
    reg.link(&dir, None, None).expect("first");

    let err = reg.link(&dir, None, None).expect_err("reject");
    assert!(matches!(err, ServerRegistryError::AlreadyExists(_)));
}

/// 目录归属不可改：--workspace 指向别的工作区时拒绝
#[test]
fn link_in_place_rejects_foreign_workspace_selector() {
    let tmp = tempfile::tempdir().expect("tmp");
    let ws01 = ws_fixture(tmp.path());
    let dir = ws01.path.join("servers").join("nested");
    crate::fs::create_dir_all(&dir).expect("mkdir");
    let store = Arc::new(MemWorkspaces::new(ws01));
    store.create(&ws2_fixture(tmp.path())).expect("second ws");
    let reg = ServerRegistry::new(Arc::new(MemServers::new()), store);

    let err = reg.link(&dir, None, Some("ws02")).expect_err("reject");
    assert!(matches!(err, ServerRegistryError::WorkspaceMismatch { .. }));
}

/// `--name` 与目录名不一致时拒绝：`rel_dir` 必须与磁盘一致
#[test]
fn link_in_place_rejects_name_not_matching_dir() {
    let tmp = tempfile::tempdir().expect("tmp");
    let reg = registry_with_ws(tmp.path());
    let dir = tmp.path().join("ws/servers/nested");
    crate::fs::create_dir_all(&dir).expect("mkdir");

    let err = reg.link(&dir, Some("other"), None).expect_err("reject");
    assert!(matches!(err, ServerRegistryError::Fs(_)));
    assert!(reg.list().expect("list").is_empty());
}

/// marker 名与目录名不一致时拒绝：不猜哪个是真实身份
#[test]
fn link_in_place_rejects_marker_name_mismatch() {
    let tmp = tempfile::tempdir().expect("tmp");
    let reg = registry_with_ws(tmp.path());
    let dir = tmp.path().join("ws/servers/nested");
    crate::fs::create_dir_all(&dir).expect("mkdir");
    crate::fs::write(
        dir.join("mcsup.json"),
        r#"{"schema":1,"id":"550e8400-e29b-41d4-a716-446655440009","name":"oldname"}"#,
    )
    .expect("marker");

    let err = reg.link(&dir, None, None).expect_err("reject");
    assert!(matches!(err, ServerRegistryError::Fs(_)));
    assert!(reg.list().expect("list").is_empty());
}

/// marker id 已被别的服务器占用时拒绝：一个 id 不能对应两个目录
#[test]
fn link_in_place_rejects_registered_id() {
    let tmp = tempfile::tempdir().expect("tmp");
    let reg = registry_with_ws(tmp.path());
    let taken = reg
        .create(
            None,
            "taken",
            &VersionId::new("1.21.1"),
            ServerVariant::Vanilla,
            None,
        )
        .expect("create");
    let dir = tmp.path().join("ws/servers/nested");
    crate::fs::create_dir_all(&dir).expect("mkdir");
    crate::fs::write(
        dir.join("mcsup.json"),
        format!(
            r#"{{"schema":1,"id":"{}","name":"nested"}}"#,
            taken.meta.id.as_str()
        ),
    )
    .expect("marker");

    let err = reg.link(&dir, None, None).expect_err("reject");
    assert!(matches!(err, ServerRegistryError::AlreadyExists(_)));
}

/// `servers/` 更深的子树仍拒绝：单层 `rel_dir` 表达不了，链接回去会成环
#[test]
fn link_rejects_target_below_servers_child() {
    let tmp = tempfile::tempdir().expect("tmp");
    let reg = registry_with_ws(tmp.path());
    let deep = tmp.path().join("ws/servers/nested/deep");
    crate::fs::create_dir_all(&deep).expect("mkdir");

    let err = reg.link(&deep, None, None).expect_err("reject");
    assert!(matches!(err, ServerRegistryError::TargetInsideWorkspace(_)));
    assert!(reg.list().expect("list").is_empty());
}

#[cfg(unix)]
#[test]
fn remove_purge_on_linked_refuses_and_keeps_target() {
    let tmp = tempfile::tempdir().expect("tmp");
    let reg = registry_with_ws(tmp.path());
    let external = tmp.path().join("external");
    crate::fs::create_dir_all(&external).expect("mkdir");
    crate::fs::write(external.join("keep.txt"), "data").expect("file");
    reg.link(&external, Some("ext"), None).expect("link");

    let err = reg.remove("ext", true, tmp.path()).expect_err("refuse");
    assert!(err.to_string().contains("linked"));
    assert!(external.join("keep.txt").is_file());
    assert!(reg.get("ext").is_ok());
}

/// `mcsup.json` 与记录 id 不一致（记录被手改 / 目录被换掉）时，
/// `--purge` 必须拒绝而不是照删
#[test]
fn remove_purge_rejects_dir_without_matching_marker() {
    let tmp = tempfile::tempdir().expect("tmp");
    let reg = registry_with_ws(tmp.path());
    let srv = reg
        .create(
            None,
            "alpha",
            &VersionId::new("1.21.1"),
            ServerVariant::Vanilla,
            None,
        )
        .expect("create");
    // 把 marker 换成别的 id：目录已不是这条记录对应的服务器
    crate::fs::write(
        srv.dir.join("mcsup.json"),
        r#"{"schema":1,"id":"550e8400-e29b-41d4-a716-4466554400aa","name":"other"}"#,
    )
    .expect("tamper");

    let err = reg.remove("alpha", true, tmp.path()).expect_err("必须拒绝");
    assert!(err.to_string().contains("refusing to purge"), "got {err}");
    assert!(srv.dir.is_dir(), "目录不能被删");
    assert!(reg.get("alpha").is_ok(), "记录必须保留");
}

/// 普通目录本就不存在时 `--purge`：注销照常成功，但不得上报成「删了磁盘」
///
/// 记录还在、目录已被人手工删掉是常见状态；此时既不能拒绝（记录才是要清理
/// 的东西），也不能报成删成功（用户会以为存档被删）。
#[test]
fn remove_purge_reports_not_deleted_when_dir_missing() {
    let tmp = tempfile::tempdir().expect("tmp");
    let reg = registry_with_ws(tmp.path());
    let srv = reg
        .create(
            None,
            "alpha",
            &VersionId::new("1.21.1"),
            ServerVariant::Vanilla,
            None,
        )
        .expect("create");
    crate::fs::remove_dir_all(&srv.dir).expect("rmdir");

    let out = reg
        .remove("alpha", true, tmp.path())
        .expect("record must still be cleared");
    assert!(!out.disk_purged(), "没有可删的对象时必须是 false");
    assert!(reg.get("alpha").is_err(), "登记必须清理");
}

/// 调用方工作目录在待删目录内时拒绝：与 `workspace remove --purge` 同一判定
#[test]
fn remove_purge_rejects_cwd_inside_server_dir() {
    let tmp = tempfile::tempdir().expect("tmp");
    let reg = registry_with_ws(tmp.path());
    let srv = reg
        .create(
            None,
            "alpha",
            &VersionId::new("1.21.1"),
            ServerVariant::Vanilla,
            None,
        )
        .expect("create");

    let err = reg
        .remove("alpha", true, &srv.dir)
        .expect_err("must reject");
    assert!(
        matches!(
            err,
            ServerRegistryError::Workspace(WorkspaceError::CwdInsideTarget { .. })
        ),
        "{err:?}"
    );
    assert!(srv.dir.join("mcsup.json").is_file(), "目录不能被删");
    assert!(reg.get("alpha").is_ok(), "记录必须保留");
}

/// 链接服务器带 `purge` 时前置校验就拒绝：与 `remove` 同一判定
#[cfg(unix)]
#[test]
fn ensure_purgeable_rejects_linked_server() {
    let tmp = tempfile::tempdir().expect("tmp");
    let reg = registry_with_ws(tmp.path());
    let external = tmp.path().join("external");
    crate::fs::create_dir_all(&external).expect("mkdir");
    let out = reg.link(&external, Some("ext"), None).expect("link");
    assert!(out.server.linked);

    assert!(reg.ensure_purgeable("ext", tmp.path()).is_err());
    assert!(reg.get("ext").is_ok(), "记录必须保留");
    assert!(external.is_dir(), "目标目录不能被删");
}

/// 删盘前置校验：cwd 在服务器目录内时拒绝，且不改任何状态
///
/// 入口靠它把拒绝提前到确认框之前，因此拒绝时必须什么都不动。
#[test]
fn ensure_purgeable_rejects_cwd_inside_server_dir() {
    let tmp = tempfile::tempdir().expect("tmp");
    let reg = registry_with_ws(tmp.path());
    let srv = reg
        .create(
            None,
            "alpha",
            &VersionId::new("1.21.1"),
            ServerVariant::Vanilla,
            None,
        )
        .expect("create");

    assert!(reg.ensure_purgeable("alpha", &srv.dir).is_err());
    assert!(srv.dir.join("mcsup.json").is_file(), "目录不能被删");
    assert!(reg.get("alpha").is_ok(), "记录必须保留");

    reg.ensure_purgeable("alpha", tmp.path()).expect("放行");
}

/// 删除链接后登记必须一起清掉，且目标目录原样保留
///
/// 回归：删除分支原先是 `crate::fs::remove_file`。Windows 的目录符号链接
/// 是目录重解析点，`DeleteFileW` 对它必然失败（`ERROR_ACCESS_DENIED`），
/// 于是 `server remove` 永远报 `io error on <link>`，链接与登记都清不掉。
/// Unix 上该分支一直是通的，所以这条用例必须在 Windows 上也能跑，不能
/// 像旧用例那样 `#[cfg(unix)]`——否则同一个缺陷不会在 CI 里暴露。
///
/// 没有创建目录符号链接的权限（Windows 需开发者模式）时跳过：那是环境
/// 限制，不是被测行为。
#[test]
fn remove_linked_drops_link_but_keeps_target() {
    let tmp = tempfile::tempdir().expect("tmp");
    let reg = registry_with_ws(tmp.path());
    let external = tmp.path().join("external");
    crate::fs::create_dir_all(&external).expect("mkdir");
    crate::fs::write(external.join("keep.txt"), "data").expect("file");
    let Ok(linked) = reg.link(&external, Some("ext"), None) else {
        eprintln!("跳过：本机不允许创建目录符号链接（Windows 需开发者模式或管理员权限）");
        return;
    };
    assert!(linked.server.linked);

    reg.remove("ext", false, tmp.path()).expect("remove");

    assert!(reg.get("ext").is_err());
    assert!(external.join("keep.txt").is_file(), "目标目录不能被删");
    assert!(
        tmp.path()
            .join("ws/servers/ext")
            .symlink_metadata()
            .is_err(),
        "链接本身必须被删掉"
    );
}

/// 链接被手工删掉之后，登记仍然要能清掉
///
/// `remove` 会重新从磁盘判定 `linked`（[`is_linked`]），链接已不在时它落到
/// 「普通目录、不 purge」分支：只清登记、不碰磁盘。这条用例锁住这个结果——
/// 用户手工 `rm` 过链接之后，登记不能变成清不掉的孤儿。
///
/// 注意它**不**经过 `remove_symlink` 的 `NotFound` 分支（那条路只有
/// 「判定与删除之间链接消失」的竞态才走得到，由
/// `remove_symlink_refuses_real_directory` 的末尾断言直接覆盖）。
#[test]
fn remove_after_link_deleted_manually_still_unregisters() {
    let tmp = tempfile::tempdir().expect("tmp");
    let reg = registry_with_ws(tmp.path());
    let external = tmp.path().join("external");
    crate::fs::create_dir_all(&external).expect("mkdir");
    let Ok(linked) = reg.link(&external, Some("ext"), None) else {
        eprintln!("跳过：本机不允许创建目录符号链接（Windows 需开发者模式或管理员权限）");
        return;
    };
    remove_symlink(&linked.server.dir).expect("手工删掉链接");

    reg.remove("ext", false, tmp.path()).expect("remove");

    assert!(reg.get("ext").is_err(), "登记必须清掉");
    assert!(external.is_dir(), "目标目录不能被删");
}

/// 链接被换成真实目录时只注销登记，绝不删目录
///
/// 登记的 `linked` 只是元数据里的一句话，磁盘上的实际形态可能已经变了
/// （用户把链接删掉、在原地建了真目录）。此时必须退回「普通目录、不
/// purge」的语义：清登记、留磁盘。删链接的方法一旦退化成认路径就删，
/// `server remove` 就成了删数据的入口。
#[test]
fn remove_keeps_real_directory_that_replaced_the_link() {
    let tmp = tempfile::tempdir().expect("tmp");
    let reg = registry_with_ws(tmp.path());
    let external = tmp.path().join("external");
    crate::fs::create_dir_all(&external).expect("mkdir");
    let Ok(linked) = reg.link(&external, Some("ext"), None) else {
        eprintln!("跳过：本机不允许创建目录符号链接（Windows 需开发者模式或管理员权限）");
        return;
    };
    // 把链接换成真实目录，里面放一个文件：删了就是丢数据
    remove_symlink(&linked.server.dir).expect("摘掉链接");
    crate::fs::create_dir_all(linked.server.dir.join("payload")).expect("mkdir");
    crate::fs::write(linked.server.dir.join("payload/precious.txt"), "data").expect("file");

    reg.remove("ext", false, tmp.path()).expect("remove");

    assert!(reg.get("ext").is_err(), "登记要清掉");
    assert!(
        linked.server.dir.join("payload/precious.txt").is_file(),
        "真实目录里的文件绝不能被动过"
    );
}

/// `remove_symlink` 只删链接：面对真实目录必须报错
///
/// 这是上一条的兜底层。`remove` 走的是「先 `get` 重判 `linked`」，
/// 而判定与删除之间存在窗口（TOCTOU）；删除函数自己也不能把真实目录
/// 当链接处理，否则窗口内一次并发替换就能删掉数据。
#[test]
fn remove_symlink_refuses_real_directory() {
    let tmp = tempfile::tempdir().expect("tmp");
    let dir = tmp.path().join("real");
    crate::fs::create_dir_all(&dir).expect("mkdir");
    crate::fs::write(dir.join("precious.txt"), "data").expect("file");

    assert!(remove_symlink(&dir).is_err(), "真实目录必须拒绝");
    assert!(dir.join("precious.txt").is_file(), "文件不能被删");
    assert!(
        remove_symlink(&dir.join("absent")).is_ok(),
        "不存在即已达成"
    );
}

#[cfg(unix)]
#[test]
fn create_refuses_dangling_symlink_path() {
    let tmp = tempfile::tempdir().expect("tmp");
    let reg = registry_with_ws(tmp.path());
    let ghost = tmp.path().join("ghost-target");
    std::os::unix::fs::symlink(&ghost, tmp.path().join("ws/servers/stale")).expect("dangling");

    let err = reg
        .create(
            None,
            "stale",
            &VersionId::new("1.21.1"),
            ServerVariant::Vanilla,
            None,
        )
        .expect_err("refuse");
    assert!(matches!(err, ServerRegistryError::AlreadyExists(_)));
}

struct FakeSource;

#[async_trait::async_trait]
impl port::ServerJarSource for FakeSource {
    fn variant(&self) -> ServerVariant {
        ServerVariant::Vanilla
    }

    async fn versions(
        &self,
    ) -> Result<Vec<domain::server::version::ServerVersion>, port::SourceError> {
        Ok(Vec::new())
    }

    async fn artifact(
        &self,
        version: &VersionId,
    ) -> Result<port::ResolvedArtifact, port::SourceError> {
        Ok(port::ResolvedArtifact {
            artifact: domain::server::artifact::JarArtifact {
                version: version.clone(),
                size: 3,
                checksum: domain::server::artifact::Checksum::new(
                    domain::server::artifact::ChecksumAlgorithm::Sha1,
                    "0".repeat(40),
                ),
            },
            location: url::Url::parse("https://example.invalid/server.jar").expect("url"),
            java_major: None,
            install: port::InstallKind::Direct,
            installer_args: Vec::new(),
            installer_args_dir: None,
            vanilla_jar: None,
        })
    }

    fn name(&self) -> &'static str {
        "fake"
    }
}

struct FakeDownloader;

#[async_trait::async_trait]
impl port::ArtifactDownloader for FakeDownloader {
    async fn download(
        &self,
        request: &port::DownloadRequest,
        _progress: Option<&dyn ProgressSink>,
        _cancel: &CancellationToken,
    ) -> Result<port::DownloadedArtifact, port::DownloadError> {
        crate::fs::write(&request.destination, b"new").expect("write");
        Ok(port::DownloadedArtifact {
            path: request.destination.clone(),
            size: 3,
        })
    }
}

struct FakeInspector {
    world: Option<domain::server::world::WorldInfo>,
    target: Option<DataVersion>,
}

impl CoreInspector for FakeInspector {
    fn world_info(
        &self,
        _server_dir: &Path,
    ) -> Result<Option<domain::server::world::WorldInfo>, port::InspectError> {
        Ok(self.world.clone())
    }

    fn jar_data_version(&self, _jar: &Path) -> Result<Option<DataVersion>, port::InspectError> {
        Ok(self.target)
    }
}

fn world_info(data_version: i32, name: &str) -> domain::server::world::WorldInfo {
    domain::server::world::WorldInfo {
        data_version: Some(DataVersion::new(data_version)),
        version_name: Some(name.to_owned()),
    }
}

/// 世界比目标新：拒绝替换，旧 `server.jar` 原样保留，暂存文件清理
#[tokio::test]
async fn downgrade_is_rejected_and_old_jar_kept() {
    let tmp = tempfile::tempdir().expect("tmp");
    let reg = registry_with_ws(tmp.path());
    let created = reg
        .create(
            None,
            "alpha",
            &VersionId::new("26.1"),
            ServerVariant::Vanilla,
            None,
        )
        .expect("create");
    crate::fs::write(created.dir.join(SERVER_JAR), b"old").expect("old jar");

    let installer = ServerInstaller::new(Arc::new(FakeSource), Arc::new(FakeDownloader));
    let inspector = FakeInspector {
        world: Some(world_info(4786, "26.1")),
        target: Some(DataVersion::new(4554)),
    };
    let err = reg
        .install_jar(
            "alpha",
            &VersionId::new("1.21.9"),
            &installer,
            &inspector,
            None,
            None,
            &CancellationToken::new(),
        )
        .await
        .expect_err("downgrade must fail");
    assert!(matches!(err, InstallJarError::Downgrade { .. }));
    assert_eq!(
        crate::fs::read(created.dir.join(SERVER_JAR)).expect("read"),
        b"old"
    );
    assert!(!created.dir.join(STAGING_JAR).exists(), "暂存文件要清理");
}

/// 目标不低：替换成功并回写版本
#[tokio::test]
async fn compatible_core_replaces_jar_and_version() {
    let tmp = tempfile::tempdir().expect("tmp");
    let reg = registry_with_ws(tmp.path());
    let created = reg
        .create(
            None,
            "alpha",
            &VersionId::new("26.1"),
            ServerVariant::Vanilla,
            None,
        )
        .expect("create");
    crate::fs::write(created.dir.join(SERVER_JAR), b"old").expect("old jar");

    let installer = ServerInstaller::new(Arc::new(FakeSource), Arc::new(FakeDownloader));
    let inspector = FakeInspector {
        world: Some(world_info(4786, "26.1")),
        target: Some(DataVersion::new(4786)),
    };
    let out = reg
        .install_jar(
            "alpha",
            &VersionId::new("26.1"),
            &installer,
            &inspector,
            None,
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("install");
    assert_eq!(out.server.meta.version.as_str(), "26.1");
    assert_eq!(
        crate::fs::read(created.dir.join(SERVER_JAR)).expect("read"),
        b"new"
    );
    assert!(!created.dir.join(STAGING_JAR).exists());
}

/// 平台参数文件目录名是完整 build 串，不是 MC 版本号
///
/// 回归：`install_via_installer` 曾用 `version.as_str()`（`26.3`）拼路径，
/// 而安装器把 `win_args.txt` 放在 `forge/26.3-66.1.0/`，结果只记录到
/// `user_jvm_args.txt`，启动时 JVM 少了平台参数文件、把 `nogui` 当成主类。
#[test]
fn forge_args_files_resolve_under_full_build_dir() {
    let tmp = tempfile::tempdir().expect("tmp");
    let dir = tmp.path();
    crate::fs::write(dir.join(domain::server::launch::USER_ARGS), b"#").expect("user args");
    let build_dir = dir.join("libraries/net/minecraftforge/forge/26.3-66.1.0");
    crate::fs::create_dir_all(&build_dir).expect("build dir");
    let platform = if cfg!(windows) {
        "win_args.txt"
    } else {
        "unix_args.txt"
    };
    crate::fs::write(build_dir.join(platform), b"#").expect("platform args");

    let files = loader_args_files(&ServerVariant::Forge, "26.3-66.1.0", dir);
    assert_eq!(
        files,
        vec![
            PathBuf::from(domain::server::launch::USER_ARGS),
            PathBuf::from(format!(
                "libraries/net/minecraftforge/forge/26.3-66.1.0/{platform}"
            )),
        ],
        "完整 build 串必须解析到平台参数文件"
    );

    // 只给 MC 版本（旧口径）时找不到平台参数文件——正是当初的 bug 形态
    let stale = loader_args_files(&ServerVariant::Forge, "26.3", dir);
    assert_eq!(
        stale,
        vec![PathBuf::from(domain::server::launch::USER_ARGS)]
    );
}

/// `NeoForge` 的参数文件在自己的 Maven 坐标下，不能套用 `Forge` 的路径
///
/// 回归：变体不参与判定时，`NeoForge` 去 `libraries/net/minecraftforge/forge/`
/// 找参数文件必然落空，症状与 `Forge` 当初一模一样（`nogui` 被当成主类）。
#[test]
fn neoforge_args_files_use_own_maven_coordinates() {
    let tmp = tempfile::tempdir().expect("tmp");
    let dir = tmp.path();
    crate::fs::write(dir.join(domain::server::launch::USER_ARGS), b"#").expect("user args");
    let build_dir = dir.join("libraries/net/neoforged/neoforge/26.3.0.99");
    crate::fs::create_dir_all(&build_dir).expect("build dir");
    let platform = if cfg!(windows) {
        "win_args.txt"
    } else {
        "unix_args.txt"
    };
    crate::fs::write(build_dir.join(platform), b"#").expect("platform args");

    let files = loader_args_files(&ServerVariant::NeoForge, "26.3.0.99", dir);
    assert_eq!(
        files,
        vec![
            PathBuf::from(domain::server::launch::USER_ARGS),
            PathBuf::from(format!(
                "libraries/net/neoforged/neoforge/26.3.0.99/{platform}"
            )),
        ],
        "NeoForge 必须解析到自己的坐标"
    );
}
