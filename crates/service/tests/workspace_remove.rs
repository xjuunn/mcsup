//! 注销工作目录与服务器登记的外键协作
//!
//! 真实 SQLite（`PRAGMA foreign_keys = ON`）：服务器行以工作目录为外键，
//! 注册表里有服务器时 `workspace remove` 曾报 `FOREIGN KEY constraint failed`。

// clippy.toml 的 allow-expect-in-tests 只认 `#[test]` 及 cfg(test) 内的函数，
// 集成测试的 setup 辅助函数不在其中，这里按同一意图放行
#![allow(clippy::expect_used)]

use domain::server::meta::{ServerId, ServerMeta, ServerName};
use domain::server::version::{ServerVariant, VersionId};
use domain::workspace::WorkspaceId;
use port::ServerRepository;
use service::WorkspaceService;
use std::path::PathBuf;
use std::sync::Arc;
use storage::SqliteStore;

fn setup(name: &str) -> (Arc<SqliteStore>, WorkspaceService, tempfile::TempDir) {
    let store = Arc::new(SqliteStore::open_in_memory().expect("open in-memory store"));
    let svc = WorkspaceService::new(store.clone(), store.clone());
    let tmp = tempfile::tempdir().expect("tmp");
    svc.init(&tmp.path().join("ws"), Some(name)).expect("init");
    (store, svc, tmp)
}

fn add_server(store: &SqliteStore, workspace_id: &WorkspaceId) {
    let meta = ServerMeta {
        id: ServerId::parse("550e8400-e29b-41d4-a716-4466554400b1").expect("uuid"),
        name: ServerName::parse("test").expect("valid"),
        workspace_id: WorkspaceId::parse(workspace_id.as_str()).expect("uuid"),
        rel_dir: PathBuf::from("servers/test"),
        version: VersionId::new("1.21.1"),
        variant: ServerVariant::Vanilla,
        jar: None,
        settings: domain::server::settings::ServerSettings::default(),
        created_at: 1_700_000_000,
        updated_at: 1_700_000_000,
    };
    store.create(&meta).expect("register server");
}

/// 注册表里有服务器时，注销工作目录必须先清服务器登记（不删盘）
#[test]
fn remove_with_servers_without_purge_clears_fk() {
    let (store, svc, _tmp) = setup("ws01");
    let ws = svc.resolve("ws01").expect("resolve");
    add_server(&store, &ws.id);

    svc.remove("ws01", false)
        .expect("remove must not hit FOREIGN KEY");

    assert!(svc.resolve("ws01").is_err());
    assert!(store.list().expect("list servers").is_empty());
}

/// 注册表里有服务器时，删盘 + 注销同样不能报外键错误
#[test]
fn remove_with_servers_and_purge_clears_fk() {
    let (store, svc, tmp) = setup("ws01");
    let ws = svc.resolve("ws01").expect("resolve");
    add_server(&store, &ws.id);
    let dir = tmp.path().join("ws");

    svc.remove("ws01", true)
        .expect("remove must not hit FOREIGN KEY");

    assert!(!dir.exists());
    assert!(store.list().expect("list servers").is_empty());
}
