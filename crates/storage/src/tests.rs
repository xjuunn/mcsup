use super::*;
use domain::java::JavaMajor;
use domain::server::artifact::{Checksum, ChecksumAlgorithm};
use domain::server::meta::{InstalledJar, ServerId};
use domain::server::version::ServerVariant;

/// 每个集合按自己的版本号校验，不串用别的集合的常量
///
/// 回归：`load_state` 的泛型校验曾写死 `JAVA_STATE_VERSION`，于是
/// `WorkspaceState`（版本 2）被拿 java 的 1 比对，一律判 `Corrupt`。
#[test]
fn each_state_uses_its_own_version() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = JsonStore::open(dir.path()).expect("open");

    // 写入版本 2 的工作区状态，读回必须成功（而不是被 java 的 1 否掉）
    let mut state = WorkspaceState::new();
    state.version = WORKSPACE_STATE_VERSION;
    store
        .write_state(Collection::Workspaces, &state)
        .expect("write");
    let read = store
        .load_state(Collection::Workspaces, WorkspaceState::new)
        .expect("工作区状态必须按工作区版本校验");
    assert_eq!(read.version, WORKSPACE_STATE_VERSION);

    // java 状态同理：版本 1 读回成功
    let java = JavaState::new();
    store
        .write_state(Collection::Java, &java)
        .expect("write java");
    store
        .load_state(Collection::Java, JavaState::new)
        .expect("java 状态必须按 java 版本校验");
}

/// 版本不匹配时仍然拒绝（修 bug 不能把校验一起去掉）
#[test]
fn wrong_state_version_is_still_corrupt() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = JsonStore::open(dir.path()).expect("open");
    let mut state = WorkspaceState::new();
    state.version = WORKSPACE_STATE_VERSION + 1;
    store
        .write_state(Collection::Workspaces, &state)
        .expect("write");
    let err = store
        .load_state(Collection::Workspaces, WorkspaceState::new)
        .expect_err("未来版本必须拒绝");
    assert!(matches!(err, RepositoryError::Corrupt(_)), "{err:?}");
}

const WS01: &str = "550e8400-e29b-41d4-a716-446655440001";
const WS02: &str = "550e8400-e29b-41d4-a716-446655440002";

fn sample_ws(name: &str, path: &str, id: &str) -> Workspace {
    Workspace {
        id: WorkspaceId::parse(id).expect("uuid"),
        name: WorkspaceName::parse(name).expect("valid"),
        path: PathBuf::from(path),
        is_current: false,
        created_at: 1_700_000_000,
        updated_at: 1_700_000_000,
    }
}

fn sample_meta(name: &str) -> ServerMeta {
    let mut id = *b"550e8400-e29b-41d4-a716-????????????";
    let mut hash: u32 = 0;
    for b in name.bytes() {
        hash = hash.wrapping_mul(31).wrapping_add(u32::from(b));
    }
    let hex = format!("{hash:012x}");
    id[24..].copy_from_slice(hex.as_bytes());
    let id = std::str::from_utf8(&id).expect("ascii");
    ServerMeta {
        id: ServerId::parse(id).expect("uuid"),
        name: ServerName::parse(name).expect("valid"),
        workspace_id: WorkspaceId::parse(WS01).expect("uuid"),
        rel_dir: PathBuf::from(name),
        version: VersionId::new("1.21.1"),
        variant: ServerVariant::Vanilla,
        jar: None,
        launch: None,
        settings: domain::server::settings::ServerSettings::default(),
        java_major: None,
        created_at: 1_700_000_000,
        updated_at: 1_700_000_000,
    }
}

fn sample_java(path: &str, major: i64, managed: bool) -> JavaRuntime {
    JavaRuntime {
        id: format!("java-{major}"),
        path: PathBuf::from(path),
        major: JavaMajor::parse(major).expect("major"),
        version: format!("{major}.0.9+9"),
        vendor: domain::java::JavaVendor::Temurin,
        managed,
        created_at: 1_700_000_000,
        updated_at: 1_700_000_000,
    }
}

fn add_ws(store: &JsonStore, ws: &Workspace) {
    WorkspaceRepository::create(store, ws).expect("ws");
}

fn add_server(store: &JsonStore, meta: &ServerMeta) {
    ServerRepository::create(store, meta).expect("server");
}

fn store_with_ws() -> JsonStore {
    let store = JsonStore::open_in_memory().expect("open");
    add_ws(&store, &sample_ws("ws01", "/tmp/ws01", WS01));
    store
}

fn file_count(store: &JsonStore, collection: Collection) -> usize {
    std::fs::read_dir(store.collection_dir(collection))
        .expect("read dir")
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some(RECORD_EXT))
        .count()
}

// ------------------------------------------------------------ 基本往返

#[test]
fn create_get_roundtrip() {
    let store = store_with_ws();
    add_server(&store, &sample_meta("alpha"));
    let got =
        ServerRepository::get(&store, &ServerName::parse("alpha").expect("valid")).expect("get");
    assert_eq!(got, sample_meta("alpha"));
}

#[test]
fn duplicate_name_rejected() {
    let store = store_with_ws();
    add_server(&store, &sample_meta("alpha"));
    let err = ServerRepository::create(&store, &sample_meta("alpha")).expect_err("dup");
    assert!(matches!(err, RepositoryError::AlreadyExists(_)));
}

#[test]
fn update_core_persists_version_and_jar() {
    let store = store_with_ws();
    add_server(&store, &sample_meta("alpha"));
    let jar = InstalledJar {
        path: PathBuf::from("server.jar"),
        size: 42,
        checksum: Checksum::new(ChecksumAlgorithm::Sha1, "a".repeat(40)),
    };
    let version = VersionId::new("1.21.2");
    let required = JavaMajor::parse(21).expect("21");
    let updated = ServerRepository::update_core(
        &store,
        &ServerName::parse("alpha").expect("valid"),
        &version,
        &jar,
        None,
        Some(required),
        1_700_000_100,
    )
    .expect("update");
    assert_eq!(updated.version, version, "安装换版本要同步回写");
    assert_eq!(updated.jar, Some(jar));
    assert_eq!(updated.java_major, Some(required));
    assert_eq!(updated.updated_at, 1_700_000_100);
}

/// 未校验占位值（Fabric 服务端 JAR）落盘再读回必须仍是未校验：
/// 空 hex 若被当成真实哈希，后续比对会永久失败
#[test]
fn unverified_jar_checksum_survives_roundtrip() {
    let store = store_with_ws();
    add_server(&store, &sample_meta("alpha"));
    let jar = InstalledJar {
        path: PathBuf::from("server.jar"),
        size: 293_000,
        checksum: Checksum::unverified(),
    };
    let name = ServerName::parse("alpha").expect("valid");
    ServerRepository::update_core(
        &store,
        &name,
        &VersionId::new("1.21.1"),
        &jar,
        None,
        None,
        1,
    )
    .expect("update");

    let got = ServerRepository::get(&store, &name).expect("get");
    let stored = got.jar.expect("jar 应落盘");
    assert_eq!(stored.size, 293_000);
    assert!(!stored.checksum.is_verified(), "未校验语义必须保留");
    assert_eq!(stored.checksum.hex(), "");
}

#[test]
fn update_settings_persists_every_field() {
    let store = store_with_ws();
    add_server(&store, &sample_meta("alpha"));
    // 非缺省值才能暴露「漏写一列」：`backup-scope` 曾经在回写时整列丢失
    let settings = domain::server::settings::ServerSettings {
        java_major: Some(JavaMajor::parse(21).expect("21")),
        ram_mib: Some(3072),
        autostart: true,
        backup_scope: domain::backup::BackupScope::from_entries(["world", "mods"]).expect("scope"),
    };
    let updated = ServerRepository::update_settings(
        &store,
        &ServerName::parse("alpha").expect("valid"),
        &settings,
        1_700_000_200,
    )
    .expect("update");
    assert_eq!(updated.settings, settings);
    assert_eq!(updated.updated_at, 1_700_000_200);
    let got =
        ServerRepository::get(&store, &ServerName::parse("alpha").expect("valid")).expect("get");
    assert_eq!(got.settings, settings);
}

/// 旧的 `ram_mib` 列是 `i32`：超过 `i32::MAX` 时曾经被 `and_then(...).ok()`
/// 静默写成 `None`（等价于「用默认值」）。现在列是 `i64`，`u32` 全域都能存回。
#[test]
fn update_settings_keeps_full_u32_ram_range() {
    let store = store_with_ws();
    add_server(&store, &sample_meta("alpha"));
    let ram = u32::MAX;
    let settings = domain::server::settings::ServerSettings {
        ram_mib: Some(ram),
        ..domain::server::settings::ServerSettings::default()
    };
    ServerRepository::update_settings(
        &store,
        &ServerName::parse("alpha").expect("valid"),
        &settings,
        1,
    )
    .expect("update");
    let got =
        ServerRepository::get(&store, &ServerName::parse("alpha").expect("valid")).expect("get");
    assert_eq!(got.settings.ram_mib, Some(ram));
}

#[test]
fn list_orders_by_name() {
    let store = store_with_ws();
    add_server(&store, &sample_meta("beta"));
    add_server(&store, &sample_meta("alpha"));
    let names: Vec<_> = ServerRepository::list(&store)
        .expect("list")
        .into_iter()
        .map(|m| m.name.as_str().to_owned())
        .collect();
    assert_eq!(names, ["alpha", "beta"]);
}

#[test]
fn missing_server_is_not_found() {
    let store = store_with_ws();
    let err = ServerRepository::get(&store, &ServerName::parse("ghost").expect("valid"))
        .expect_err("missing");
    assert!(matches!(err, RepositoryError::NotFound(_)));
}

#[test]
fn remove_single_server_clears_row() {
    let store = store_with_ws();
    add_server(&store, &sample_meta("alpha"));
    add_server(&store, &sample_meta("beta"));
    ServerRepository::remove(&store, &ServerName::parse("alpha").expect("valid")).expect("remove");
    let rest: Vec<_> = ServerRepository::list(&store)
        .expect("list")
        .into_iter()
        .map(|m| m.name.as_str().to_owned())
        .collect();
    assert_eq!(rest, ["beta"]);
    assert_eq!(
        file_count(&store, Collection::Servers),
        1,
        "记录文件必须删掉"
    );
}

#[test]
fn remove_by_workspace_clears_servers() {
    let store = store_with_ws();
    add_server(&store, &sample_meta("alpha"));
    add_server(&store, &sample_meta("beta"));
    let ws_id = WorkspaceId::parse(WS01).expect("uuid");
    ServerRepository::remove_by_workspace(&store, &ws_id).expect("clear");
    assert!(ServerRepository::list(&store).expect("list").is_empty());
    assert_eq!(file_count(&store, Collection::Servers), 0);
}

#[test]
fn relink_moves_server() {
    let store = JsonStore::open_in_memory().expect("open");
    add_ws(&store, &sample_ws("ws01", "/tmp/ws01", WS01));
    add_ws(&store, &sample_ws("ws02", "/tmp/ws02", WS02));
    add_server(&store, &sample_meta("alpha"));
    let ws_id02 = WorkspaceId::parse(WS02).expect("uuid");
    let moved = ServerRepository::relink(
        &store,
        &ServerName::parse("alpha").expect("valid"),
        &ws_id02,
        Path::new("alpha"),
        1_700_000_200,
    )
    .expect("relink");
    assert_eq!(moved.workspace_id, ws_id02);
}

/// `rel_dir` 逃逸必须在**写入时**被拒，而不是等 `doctor` 事后体检
///
/// 回归：曾直接落盘，之后每次由 workspace + `rel_dir` 拼出的绝对路径都
/// 指向工作区之外，`backup` / `remove --purge` 全按那个路径操作。
#[test]
fn relink_rejects_escaping_rel_dir() {
    let store = store_with_ws();
    let ws_id = WorkspaceId::parse(WS01).expect("uuid");
    let name = ServerName::parse("alpha").expect("valid");
    add_server(&store, &sample_meta("alpha"));

    for bad in ["../outside", "/abs", "a/b", ".."] {
        let err = ServerRepository::relink(&store, &name, &ws_id, Path::new(bad), 1)
            .expect_err("must reject");
        assert!(
            matches!(err, RepositoryError::InvalidRelDir { .. }),
            "{bad:?} → {err:?}"
        );
    }

    // 拒绝后记录未被改动
    let unchanged = ServerRepository::get(&store, &name).expect("get");
    assert_eq!(unchanged.rel_dir, Path::new("alpha"));
}

/// `create` 同样拒绝逃逸的 `rel_dir`
#[test]
fn create_rejects_escaping_rel_dir() {
    let store = JsonStore::open_in_memory().expect("open");
    add_ws(&store, &sample_ws("ws01", "/tmp/ws01", WS01));
    let mut meta = sample_meta("alpha");
    meta.rel_dir = PathBuf::from("../outside");
    let err = ServerRepository::create(&store, &meta).expect_err("must reject");
    assert!(
        matches!(err, RepositoryError::InvalidRelDir { .. }),
        "{err:?}"
    );
    assert_eq!(file_count(&store, Collection::Servers), 0, "失败不得落盘");
}

#[test]
fn relink_missing_is_not_found() {
    let store = store_with_ws();
    let ws_id = WorkspaceId::parse(WS01).expect("uuid");
    let err = ServerRepository::relink(
        &store,
        &ServerName::parse("ghost").expect("valid"),
        &ws_id,
        Path::new("ghost"),
        0,
    )
    .expect_err("missing");
    assert!(matches!(err, RepositoryError::NotFound(_)));
}

#[test]
fn relink_to_unknown_workspace_is_not_found() {
    let store = store_with_ws();
    add_server(&store, &sample_meta("alpha"));
    let err = ServerRepository::relink(
        &store,
        &ServerName::parse("alpha").expect("valid"),
        &WorkspaceId::parse(WS02).expect("uuid"),
        Path::new("alpha"),
        0,
    )
    .expect_err("unknown ws");
    assert!(matches!(err, RepositoryError::NotFound(_)));
}

// -------------------------------------------------------- 目录布局与文件

/// 一条记录一个文件，文件名即 id
#[test]
fn one_file_per_record() {
    let store = JsonStore::open_in_memory().expect("open");
    add_ws(&store, &sample_ws("ws01", "/tmp/ws01", WS01));
    add_ws(&store, &sample_ws("ws02", "/tmp/ws02", WS02));
    assert_eq!(file_count(&store, Collection::Workspaces), 2);
    assert!(
        store
            .record_path(Collection::Workspaces, WS01)
            .expect("path")
            .is_file()
    );
    add_server(&store, &sample_meta("alpha"));
    assert_eq!(file_count(&store, Collection::Servers), 1);
    JavaRuntimeRepository::upsert(&store, &sample_java("/opt/jdk21", 21, true)).expect("java");
    assert_eq!(file_count(&store, Collection::Java), 1);
}

/// 记录文件内容是带 schema 的独立 JSON 对象
#[test]
fn record_file_is_self_describing() {
    let store = store_with_ws();
    let text = std::fs::read_to_string(
        store
            .record_path(Collection::Workspaces, WS01)
            .expect("path"),
    )
    .expect("read");
    let value: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert_eq!(value["version"], models::RECORD_VERSION);
    assert_eq!(value["id"], WS01);
    assert_eq!(value["name"], "ws01");
    assert!(value.get("is_current").is_none(), "当前标记不进记录文件");
}

/// 数据跨 `open` 重开仍在
#[test]
fn data_survives_reopen() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    {
        let store = JsonStore::open(dir.path()).expect("open");
        add_ws(&store, &sample_ws("ws01", "/tmp/ws01", WS01));
        WorkspaceRepository::set_current(&store, &WorkspaceId::parse(WS01).expect("uuid"))
            .expect("current");
        add_server(&store, &sample_meta("alpha"));
        JavaRuntimeRepository::upsert(&store, &sample_java("/opt/jdk21", 21, true)).expect("java");
        JavaRuntimeRepository::mark_scanned(&store, 1_700_000_000).expect("mark");
    }
    let store = JsonStore::open(dir.path()).expect("reopen");
    assert_eq!(WorkspaceRepository::list(&store).expect("list").len(), 1);
    assert_eq!(
        WorkspaceRepository::current(&store)
            .expect("current")
            .map(|ws| ws.name.as_str().to_owned()),
        Some("ws01".to_owned())
    );
    assert_eq!(
        ServerRepository::list(&store).expect("servers"),
        vec![sample_meta("alpha")]
    );
    assert_eq!(
        JavaRuntimeRepository::scan_state(&store).expect("state"),
        Some(1_700_000_000)
    );
}

// ----------------------------------------------------------- 故障隔离

/// 坏一个文件只影响那一条：list 跳过它，其余照常
#[test]
fn corrupt_record_is_isolated() {
    let store = store_with_ws();
    add_server(&store, &sample_meta("alpha"));
    add_server(&store, &sample_meta("beta"));
    let broken = store
        .record_path(Collection::Servers, sample_meta("beta").id.as_str())
        .expect("path");
    std::fs::write(&broken, b"{ not json").expect("corrupt");

    let list = ServerRepository::list(&store).expect("list 不应整体失败");
    assert_eq!(list.len(), 1, "只跳过坏的那一条");
    assert_eq!(list[0].name.as_str(), "alpha");

    let err = ServerRepository::get(&store, &ServerName::parse("beta").expect("valid"))
        .expect_err("坏记录读不出");
    assert!(
        matches!(err, RepositoryError::NotFound(_)) || matches!(err, RepositoryError::Corrupt(_))
    );
}

/// 坏记录挡住写入：看不见的记录可能与待写项冲突，不能默默放过
#[test]
fn corrupt_record_blocks_write() {
    let store = store_with_ws();
    add_server(&store, &sample_meta("alpha"));
    let broken = store
        .record_path(Collection::Servers, sample_meta("alpha").id.as_str())
        .expect("path");
    std::fs::write(&broken, b"{ not json").expect("corrupt");
    let err =
        ServerRepository::create(&store, &sample_meta("beta")).expect_err("校验必须看得到坏文件");
    assert!(matches!(err, RepositoryError::Corrupt(_)), "got {err:?}");
    assert!(
        !store
            .record_path(Collection::Servers, sample_meta("beta").id.as_str())
            .expect("path")
            .exists()
    );
}

/// 文件名与内部 id 不一致 → 损坏，防止改名造出两条同 id
#[test]
fn mismatched_id_is_corrupt() {
    let store = store_with_ws();
    let path = store
        .record_path(Collection::Workspaces, WS01)
        .expect("path");
    let text = std::fs::read_to_string(&path).expect("read");
    let renamed = store
        .record_path(Collection::Workspaces, WS02)
        .expect("path");
    std::fs::write(&renamed, text).expect("rename");
    let err = store
        .load_record::<WorkspaceEntry>(Collection::Workspaces, WS02)
        .expect_err("id 不一致");
    assert!(matches!(err, RepositoryError::Corrupt(_)), "got {err:?}");
}

/// schema 版本不符 → 损坏
#[test]
fn unknown_record_schema_is_corrupt() {
    let store = store_with_ws();
    let path = store
        .record_path(Collection::Workspaces, WS01)
        .expect("path");
    std::fs::write(
        &path,
        format!(
            r#"{{"version":99,"id":"{WS01}","name":"ws01","path":"/tmp/ws01","created_at":0,"updated_at":0}}"#
        ),
    )
    .expect("seed");
    let err = store
        .load_record::<WorkspaceEntry>(Collection::Workspaces, WS01)
        .expect_err("schema");
    assert!(matches!(err, RepositoryError::Corrupt(_)), "got {err:?}");
}

/// 写入失败不破坏旧文件
#[test]
fn failed_write_keeps_previous_file() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let store = JsonStore::open(dir.path()).expect("open");
    add_ws(&store, &sample_ws("ws01", "/tmp/ws01", WS01));
    let path = store
        .record_path(Collection::Workspaces, WS01)
        .expect("path");
    let before = std::fs::read(&path).expect("read before");
    // 用目录占住临时文件名，模拟写盘失败
    std::fs::create_dir(path.with_file_name(format!(".{WS01}.json.tmp"))).expect("block");
    let err = WorkspaceRepository::relink_path(
        &store,
        &WorkspaceId::parse(WS01).expect("uuid"),
        Path::new("/tmp/moved"),
        1,
    )
    .expect_err("write");
    assert!(matches!(err, RepositoryError::Storage(_)), "got {err:?}");
    assert_eq!(std::fs::read(&path).expect("read after"), before);
}

/// 写入不留 `.tmp` 残留
#[test]
fn successful_write_leaves_no_temp_file() {
    let store = store_with_ws();
    let leftovers: Vec<_> = std::fs::read_dir(store.collection_dir(Collection::Workspaces))
        .expect("dir")
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "残留 {leftovers:?}");
}

// ------------------------------------------------------------ 当前工作区

#[test]
fn create_current_workspace_sets_pointer() {
    let store = JsonStore::open_in_memory().expect("open");
    let mut ws = sample_ws("ws01", "/tmp/ws01", WS01);
    ws.is_current = true;
    add_ws(&store, &ws);
    let current = WorkspaceRepository::current(&store)
        .expect("current")
        .expect("some");
    assert_eq!(current.name.as_str(), "ws01");

    // 后续新增不抢当前标记
    add_ws(&store, &sample_ws("ws02", "/tmp/ws02", WS02));
    assert_eq!(
        WorkspaceRepository::current(&store)
            .expect("current")
            .expect("some")
            .name
            .as_str(),
        "ws01"
    );
}

#[test]
fn workspace_current_is_single() {
    let store = JsonStore::open_in_memory().expect("open");
    add_ws(&store, &sample_ws("ws01", "/tmp/ws01", WS01));
    add_ws(&store, &sample_ws("ws02", "/tmp/ws02", WS02));
    let id01 = WorkspaceId::parse(WS01).expect("uuid");
    let id02 = WorkspaceId::parse(WS02).expect("uuid");
    WorkspaceRepository::set_current(&store, &id01).expect("current 1");
    WorkspaceRepository::set_current(&store, &id02).expect("current 2");
    let currents: Vec<_> = WorkspaceRepository::list(&store)
        .expect("list")
        .into_iter()
        .filter(|w| w.is_current)
        .collect();
    assert_eq!(currents.len(), 1, "单值语义只能有一个当前");
    assert_eq!(currents[0].name.as_str(), "ws02");
}

/// 删除当前工作区后指针清空
#[test]
fn remove_current_workspace_clears_marker() {
    let store = JsonStore::open_in_memory().expect("open");
    add_ws(&store, &sample_ws("ws01", "/tmp/ws01", WS01));
    let id = WorkspaceId::parse(WS01).expect("uuid");
    WorkspaceRepository::set_current(&store, &id).expect("current");
    WorkspaceRepository::remove(&store, &id).expect("remove");
    assert!(
        WorkspaceRepository::current(&store)
            .expect("current")
            .is_none()
    );
}

/// `state.json` 指向不存在的记录 → 按「无当前」处理，不报错也不改写文件
#[test]
fn dangling_current_pointer_is_none() {
    let store = store_with_ws();
    let state_path = store.state_path(Collection::Workspaces);
    std::fs::write(
        &state_path,
        format!(r#"{{"version":1,"current":"{WS02}"}}"#),
    )
    .expect("seed");
    assert!(
        WorkspaceRepository::current(&store)
            .expect("悬空指针是安全失败")
            .is_none()
    );
    let text = std::fs::read_to_string(&state_path).expect("read");
    assert!(text.contains(WS02), "读不应改写状态文件");
}

/// v1 的裸 uuid 指针读作「本地选择」
#[test]
fn v1_state_migrates_to_local_selection() {
    let store = store_with_ws();
    let state_path = store.state_path(Collection::Workspaces);
    std::fs::write(
        &state_path,
        format!(r#"{{"version":1,"current":"{WS01}"}}"#),
    )
    .expect("seed");
    let selection = WorkspaceRepository::current_selection(&store)
        .expect("selection")
        .expect("some");
    assert_eq!(selection.origin, Origin::Local);
    assert_eq!(selection.workspace.as_str(), WS01);
    assert!(
        WorkspaceRepository::current(&store)
            .expect("current")
            .is_some()
    );
}

/// 远端选择只存本机指针：不冒充本地 current，也不给本机记录打 current
#[test]
fn remote_selection_is_client_local_only() {
    let store = store_with_ws();
    let selection = WorkspaceSelection {
        origin: Origin::Remote(port::RemoteName::parse("r1").expect("name")),
        workspace: WorkspaceId::parse(WS02).expect("uuid"),
    };
    WorkspaceRepository::set_current_selection(&store, &selection).expect("set");
    assert_eq!(
        WorkspaceRepository::current_selection(&store).expect("selection"),
        Some(selection)
    );
    assert!(
        WorkspaceRepository::current(&store)
            .expect("current")
            .is_none(),
        "远端选择不能冒充本地 current"
    );
    assert!(
        WorkspaceRepository::list(&store)
            .expect("list")
            .iter()
            .all(|w| !w.is_current),
        "本机记录不能因为远端选择而被标成 current"
    );
    let text = std::fs::read_to_string(store.state_path(Collection::Workspaces)).expect("read");
    let value: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert_eq!(value["current"]["origin"], "r1");
    assert_eq!(value["current"]["workspace"], WS02);
}

/// 切到远端不覆盖本地最后选择：断开后要靠它撤回本地
#[test]
fn remote_selection_keeps_last_local() {
    let store = JsonStore::open_in_memory().expect("open");
    add_ws(&store, &sample_ws("ws01", "/tmp/ws01", WS01));
    add_ws(&store, &sample_ws("ws02", "/tmp/ws02", WS02));
    let id01 = WorkspaceId::parse(WS01).expect("uuid");
    let id02 = WorkspaceId::parse(WS02).expect("uuid");
    WorkspaceRepository::set_current(&store, &id01).expect("current ws01");
    assert_eq!(
        WorkspaceRepository::last_local_selection(&store).expect("last"),
        Some(WorkspaceId::parse(WS01).expect("uuid"))
    );

    let remote = WorkspaceSelection {
        origin: Origin::Remote(port::RemoteName::parse("r1").expect("name")),
        workspace: id02.clone(),
    };
    WorkspaceRepository::set_current_selection(&store, &remote).expect("remote");
    assert_eq!(
        WorkspaceRepository::last_local_selection(&store).expect("last"),
        Some(id01.clone()),
        "远端选择不能盖掉本地最后选择"
    );

    // 切回本地时跟随
    WorkspaceRepository::set_current_selection(
        &store,
        &WorkspaceSelection {
            origin: Origin::Local,
            workspace: id02.clone(),
        },
    )
    .expect("local");
    assert_eq!(
        WorkspaceRepository::last_local_selection(&store).expect("last"),
        Some(id02)
    );

    let text = std::fs::read_to_string(store.state_path(Collection::Workspaces)).expect("read");
    let value: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert_eq!(value["version"], 3);
    assert_eq!(value["last_local"]["origin"], "local");
    assert_eq!(value["last_local"]["workspace"], WS02);
}

/// v2 文件没有 `last_local`：读作「未记录」，不报错
#[test]
fn v2_state_without_last_local_reads_as_none() {
    let store = store_with_ws();
    std::fs::write(
        store.state_path(Collection::Workspaces),
        format!(r#"{{"version":2,"current":{{"origin":"local","workspace":"{WS01}"}}}}"#),
    )
    .expect("seed");
    assert_eq!(
        WorkspaceRepository::current_selection(&store).expect("selection"),
        Some(WorkspaceSelection {
            origin: Origin::Local,
            workspace: WorkspaceId::parse(WS01).expect("uuid"),
        })
    );
    assert_eq!(
        WorkspaceRepository::last_local_selection(&store).expect("last"),
        None
    );
}

/// 清空选择只动 `current`，`last_local` 留着——下次撤回仍有目标
#[test]
fn clear_current_selection_keeps_last_local() {
    let store = store_with_ws();
    let id01 = WorkspaceId::parse(WS01).expect("uuid");
    WorkspaceRepository::set_current(&store, &id01).expect("current");
    WorkspaceRepository::clear_current_selection(&store).expect("clear");
    assert!(
        WorkspaceRepository::current_selection(&store)
            .expect("selection")
            .is_none()
    );
    assert!(
        WorkspaceRepository::current(&store)
            .expect("current")
            .is_none()
    );
    assert_eq!(
        WorkspaceRepository::last_local_selection(&store).expect("last"),
        Some(id01)
    );
}

/// 删除的正是 `last_local` 指向的记录：一并清掉，避免下次撤回拿到悬空 id
#[test]
fn remove_clears_last_local_pointer() {
    let store = store_with_ws();
    let id01 = WorkspaceId::parse(WS01).expect("uuid");
    WorkspaceRepository::set_current(&store, &id01).expect("current");
    WorkspaceRepository::remove(&store, &id01).expect("remove");
    assert_eq!(
        WorkspaceRepository::last_local_selection(&store).expect("last"),
        None
    );
}

/// 新建即当前时写 v2 本地指针
#[test]
fn first_current_workspace_writes_v2_local_pointer() {
    let store = JsonStore::open_in_memory().expect("open");
    let mut ws = sample_ws("ws01", "/tmp/ws01", WS01);
    ws.is_current = true;
    add_ws(&store, &ws);
    let text = std::fs::read_to_string(store.state_path(Collection::Workspaces)).expect("read");
    let value: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert_eq!(value["current"]["origin"], "local");
    assert_eq!(value["current"]["workspace"], WS01);
}

#[test]
fn set_current_missing_is_not_found() {
    let store = JsonStore::open_in_memory().expect("open");
    let err = WorkspaceRepository::set_current(&store, &WorkspaceId::parse(WS01).expect("uuid"))
        .expect_err("missing");
    assert!(matches!(err, RepositoryError::NotFound(_)));
}

/// `state.json` 版本不符 → 写路径拒绝，读路径降级为「无当前」
///
/// 读路径不能整体失败：`state.json` 是全局单点，若它坏掉就
/// 让 `workspace list` / `server list` 全不可用，就退回到了单文件方案
/// 「一处损坏全盘瘫痪」的老问题。
#[test]
fn corrupt_state_degrades_reads_but_blocks_writes() {
    let store = store_with_ws();
    std::fs::write(
        store.state_path(Collection::Workspaces),
        br#"{"version":7,"current":null}"#,
    )
    .expect("seed");

    let list = WorkspaceRepository::list(&store).expect("list 仍应可用");
    assert_eq!(list.len(), 1);
    assert!(!list[0].is_current, "读不懂指针时按无当前展示");
    assert!(
        WorkspaceRepository::current(&store)
            .expect("current 降级")
            .is_none()
    );

    // 写路径仍必须失败：不能静默覆盖一份读不懂的状态文件
    let err = WorkspaceRepository::set_current(&store, &WorkspaceId::parse(WS01).expect("uuid"))
        .expect_err("state 损坏时不得写入");
    assert!(matches!(err, RepositoryError::Corrupt(_)), "got {err:?}");
}

/// 不安全 id 被拒绝，不会被拼进路径
///
/// `id` 既来自文件名也来自记录内容；含 `..` 或分隔符的 id
/// 会让读写落到集合目录之外。
#[test]
fn unsafe_record_id_is_rejected() {
    let store = store_with_ws();
    for bad in ["..", ".", "a/b", "a\\b", ""] {
        assert!(
            store.record_path(Collection::Workspaces, bad).is_err(),
            "{bad} 必须被拒绝"
        );
    }
    assert!(
        store.record_path(Collection::Workspaces, WS01).is_ok(),
        "UUID 必须通过"
    );
    assert!(
        store.record_path(Collection::Java, "java-21").is_ok(),
        "生成式 id 必须通过"
    );
    assert!(
        store
            .record_path(Collection::Servers, "my.server")
            .is_ok_and(
                |path| path.parent() == Some(store.collection_dir(Collection::Servers).as_path())
            ),
        "带点的单层 id 必须留在集合目录内"
    );
}

/// 扫目录时 id 与 `record_path` 同口径：`state.json` 不是记录，
/// 空 id / 越界 id 不会被拼成指向别的文件的路径
#[test]
fn scan_ids_share_record_path_rules() {
    let store = store_with_ws();
    for (name, id) in [
        ("state.json", None),
        (".json", None),
        ("a.json", Some("a")),
        ("my.server.json", Some("my.server")),
    ] {
        assert_eq!(
            JsonStore::record_id_of(Path::new(name)).as_deref(),
            id,
            "{name}"
        );
    }
    let dir = store.collection_dir(Collection::Servers);
    for name in ["state.json", ".json", "leftover.bak"] {
        std::fs::write(dir.join(name), b"{}").expect("seed");
    }
    std::fs::write(dir.join(".json"), r#"{"schema":1,"id":""}"#).expect("seed");
    assert!(
        ServerRepository::list(&store)
            .expect("list 不得被非记录文件拖垮")
            .is_empty()
    );
}

/// 记录内 id 被改成越界值 → 损坏，而不是写到目录外
#[test]
fn tampered_id_does_not_escape_collection_dir() {
    let store = store_with_ws();
    let path = store
        .record_path(Collection::Workspaces, WS01)
        .expect("path");
    std::fs::write(
        &path,
        r#"{"version":1,"id":"../escaped","name":"ws01","path":"/tmp/ws01","created_at":0,"updated_at":0}"#,
    )
    .expect("seed");
    let err = store
        .load_record::<WorkspaceEntry>(Collection::Workspaces, WS01)
        .expect_err("id 越界");
    assert!(matches!(err, RepositoryError::Corrupt(_)), "got {err:?}");
}

// -------------------------------------------------------------- 唯一键

#[test]
fn workspace_unique_keys() {
    let store = JsonStore::open_in_memory().expect("open");
    add_ws(&store, &sample_ws("ws01", "/tmp/ws01", WS01));

    let dup_name = WorkspaceRepository::create(&store, &sample_ws("ws01", "/tmp/other", WS02))
        .expect_err("dup name");
    assert!(matches!(dup_name, RepositoryError::AlreadyExists(_)));

    let dup_path = WorkspaceRepository::create(&store, &sample_ws("ws02", "/tmp/ws01", WS02))
        .expect_err("dup path");
    assert!(matches!(dup_path, RepositoryError::AlreadyExists(_)));

    assert_eq!(WorkspaceRepository::list(&store).expect("list").len(), 1);
}

#[test]
fn server_unique_rel_dir_per_workspace() {
    let store = JsonStore::open_in_memory().expect("open");
    add_ws(&store, &sample_ws("ws01", "/tmp/ws01", WS01));
    add_ws(&store, &sample_ws("ws02", "/tmp/ws02", WS02));
    let mut first = sample_meta("alpha");
    first.rel_dir = PathBuf::from("shared");
    add_server(&store, &first);

    let mut other = sample_meta("beta");
    other.rel_dir = PathBuf::from("shared");
    let err = ServerRepository::create(&store, &other).expect_err("dup rel_dir");
    assert!(matches!(err, RepositoryError::AlreadyExists(_)));

    other.workspace_id = WorkspaceId::parse(WS02).expect("uuid");
    ServerRepository::create(&store, &other).expect("different workspace");
}

#[test]
fn server_with_unknown_workspace_is_corrupt() {
    let store = JsonStore::open_in_memory().expect("open");
    let err = ServerRepository::create(&store, &sample_meta("alpha")).expect_err("no ws");
    assert!(matches!(err, RepositoryError::Corrupt(_)), "got {err:?}");
    assert_eq!(file_count(&store, Collection::Servers), 0, "失败不得落盘");
}

#[test]
fn relink_path_rejects_taken_target() {
    let store = JsonStore::open_in_memory().expect("open");
    add_ws(&store, &sample_ws("ws01", "/tmp/ws01", WS01));
    add_ws(&store, &sample_ws("ws02", "/tmp/ws02", WS02));
    let err = WorkspaceRepository::relink_path(
        &store,
        &WorkspaceId::parse(WS02).expect("uuid"),
        Path::new("/tmp/ws01"),
        0,
    )
    .expect_err("taken");
    assert!(matches!(err, RepositoryError::AlreadyExists(_)));
    let ws =
        WorkspaceRepository::get(&store, &WorkspaceId::parse(WS02).expect("uuid")).expect("get");
    assert_eq!(ws.path, PathBuf::from("/tmp/ws02"));
}

// ------------------------------------------------------------ 跨进程锁

/// 锁存在时阻塞；释放后可继续（模拟另一个进程持锁）
#[test]
fn dir_lock_blocks_until_released() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let path = dir.path().join(".lock");
    let held = DirLock::acquire(&path).expect("第一个拿到锁");
    assert!(path.exists(), "持锁期间锁文件存在");

    // 持锁期间其他持有者拿不到：用另一线程 + 超时验证不会立刻成功
    let probe_path = path.clone();
    let probe = std::thread::spawn(move || DirLock::acquire(&probe_path).is_ok());
    std::thread::sleep(std::time::Duration::from_millis(150));
    assert!(!probe.is_finished(), "第二个持有者必须等待，而不是立刻拿到");

    drop(held);
    assert!(probe.join().expect("join"), "释放后等待者应拿到锁");
    // 锁文件永久保留：删掉它会让等待者在新旧 inode 上各持一把锁
    assert!(path.exists(), "锁文件不删除");
    // 没有持有者时可以直接拿到
    let again = DirLock::acquire(&path).expect("释放后可再次拿到");
    drop(again);
}

/// 残留锁文件（垃圾内容 / 很旧的 mtime）不参与互斥判定
///
/// 旧实现把「pid 不存活」当成回收凭据：探测到死亡到删除之间，可能删掉
/// 别人刚建的新锁。现在互斥只由内核锁决定，残留文件既不挡路也不删除。
#[test]
fn leftover_lock_file_does_not_block() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let path = dir.path().join(".lock");
    std::fs::write(&path, "not-a-pid").expect("seed");
    let ancient = std::time::SystemTime::now() - std::time::Duration::from_secs(86_400);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .expect("open")
        .set_modified(ancient)
        .expect("set mtime");

    let lock = DirLock::acquire(&path).expect("残留锁文件不挡路");
    drop(lock);
    assert!(path.exists(), "锁文件不删除");
}

/// 写入结束后锁文件仍在，但已经没人持有：下一条命令能立刻拿到
#[test]
fn lock_file_survives_and_is_reusable() {
    let store = store_with_ws();
    add_server(&store, &sample_meta("alpha"));
    let path = store.root().join(LOCK_FILE);
    assert!(path.exists(), "锁文件永久保留");
    let lock = DirLock::acquire(&path).expect("上一把锁已释放");
    drop(lock);
}

// ---------------------------------------------------------------- Java
#[test]
fn java_upsert_and_list_orders_by_major_desc() {
    let store = JsonStore::open_in_memory().expect("open");
    JavaRuntimeRepository::upsert(&store, &sample_java("/opt/jdk17", 17, false)).expect("17");
    JavaRuntimeRepository::upsert(&store, &sample_java("/opt/jdk21", 21, true)).expect("21");
    let list = JavaRuntimeRepository::list(&store).expect("list");
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].major.as_i64(), 21);
    assert_eq!(list[1].major.as_i64(), 17);
}

#[test]
fn java_upsert_same_path_updates_in_place() {
    let store = JsonStore::open_in_memory().expect("open");
    JavaRuntimeRepository::upsert(&store, &sample_java("/opt/jdk17", 17, false)).expect("first");
    let mut updated = sample_java("/opt/jdk17", 17, true);
    updated.version = "17.0.11+9".to_owned();
    JavaRuntimeRepository::upsert(&store, &updated).expect("second");
    let list = JavaRuntimeRepository::list(&store).expect("list");
    assert_eq!(list.len(), 1, "同路径不得产生两份记录");
    assert_eq!(file_count(&store, Collection::Java), 1);
    assert!(list[0].managed);
}

#[test]
fn java_remove_missing_is_not_found() {
    let store = JsonStore::open_in_memory().expect("open");
    let err = JavaRuntimeRepository::remove(&store, Path::new("/ghost")).expect_err("missing");
    assert!(matches!(err, RepositoryError::NotFound(_)));
}

#[test]
fn java_scan_state_roundtrip() {
    let store = JsonStore::open_in_memory().expect("open");
    assert_eq!(
        JavaRuntimeRepository::scan_state(&store).expect("state"),
        None
    );
    JavaRuntimeRepository::mark_scanned(&store, 1_700_000_000).expect("mark");
    assert_eq!(
        JavaRuntimeRepository::scan_state(&store).expect("state"),
        Some(1_700_000_000)
    );
}

// ---------------------------------------------------------------- 下载源选择

/// 没探过的上游面必须返回「没有记录」，不能返回官方
///
/// 两者语义不同：`None` 表示下次要探测，`Some(official)` 表示探过并且选中了
/// 官方、不该再探。把前者错报成后者，镜像永远不会被启用。
#[test]
fn route_missing_upstream_is_none() {
    let store = JsonStore::open_in_memory().expect("open");
    assert_eq!(store.route("forge-maven").expect("read"), None);
}

#[test]
fn route_remember_then_read_roundtrip() {
    let store = JsonStore::open_in_memory().expect("open");
    let bmclapi = SourceId::new("bmclapi").expect("id");
    store.remember("forge-maven", &bmclapi).expect("write");
    assert_eq!(store.route("forge-maven").expect("read"), Some(bmclapi));
    // 另一个上游面不受影响：键是各自的，不是全局单值
    assert_eq!(store.route("modrinth-cdn").expect("read"), None);
}

#[test]
fn route_remember_overwrites_previous_choice() {
    let store = JsonStore::open_in_memory().expect("open");
    store
        .remember("forge-maven", &SourceId::new("bmclapi").expect("id"))
        .expect("first");
    let mcim = SourceId::new("mcim").expect("id");
    store.remember("forge-maven", &mcim).expect("second");
    assert_eq!(store.route("forge-maven").expect("read"), Some(mcim));
}

/// 失败后重新选源要求能作废旧选择；重复作废不得报错
#[test]
fn route_forget_removes_and_is_idempotent() {
    let store = JsonStore::open_in_memory().expect("open");
    store
        .remember("forge-maven", &SourceId::new("bmclapi").expect("id"))
        .expect("write");
    store.forget("forge-maven").expect("forget");
    assert_eq!(store.route("forge-maven").expect("read"), None);
    // 再忘一次：没有记录也算成功，调用方不必先查
    store.forget("forge-maven").expect("forget twice");
}

/// 手改坏的状态文件只丢一次探测机会，不该让下载失败
#[test]
fn route_unparsable_choice_reads_as_none() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = JsonStore::open(dir.path()).expect("open");
    let path = dir.path().join("download").join("state.json");
    std::fs::write(&path, r#"{"version":1,"routes":{"forge-maven":"BMCLAPI"}}"#).expect("seed");
    assert_eq!(
        store.route("forge-maven").expect("read"),
        None,
        "非法源标识按未记录处理"
    );
    // 覆盖写入仍然可用，坏值不会卡住后续选择
    let bmclapi = SourceId::new("bmclapi").expect("id");
    store.remember("forge-maven", &bmclapi).expect("write");
    assert_eq!(store.route("forge-maven").expect("read"), Some(bmclapi));
}

/// 选源状态只有一个文件：拆成记录文件会为含连字符的键绕开文件名规则
#[test]
fn route_state_uses_no_record_files() {
    let store = JsonStore::open_in_memory().expect("open");
    store
        .remember("forge-maven", &SourceId::new("bmclapi").expect("id"))
        .expect("write");
    store
        .remember("modrinth-cdn", &SourceId::new("mcim").expect("id"))
        .expect("write");
    // 目录里除了 state.json 不该再有别的文件：记录文件名要经过
    // `RECORD_EXT` 与文件名规则，`forge-maven` 这类键走不通
    let names: Vec<String> = std::fs::read_dir(store.collection_dir(Collection::Download))
        .expect("read dir")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec![STATE_FILE.to_owned()]);
    let raw = std::fs::read_to_string(store.state_path(Collection::Download)).expect("state file");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("json");
    assert_eq!(parsed["version"], DOWNLOAD_STATE_VERSION);
    assert_eq!(parsed["routes"]["forge-maven"], "bmclapi");
    assert_eq!(parsed["routes"]["modrinth-cdn"], "mcim");
}
