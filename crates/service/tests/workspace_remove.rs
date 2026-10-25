//! 注销工作目录与服务器登记的协作
//!
//! 真实 JSON 注册表：服务器项以工作目录 id 引用，工作目录被注销后
//! 其下的服务器必须一并清理，否则注册表里会留下悬空引用。

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
use storage::JsonStore;

fn setup(name: &str) -> (Arc<JsonStore>, WorkspaceService, tempfile::TempDir) {
    let store = Arc::new(JsonStore::open_in_memory().expect("open in-memory store"));
    let svc = WorkspaceService::new(store.clone(), store.clone());
    let tmp = tempfile::tempdir().expect("tmp");
    svc.init(&tmp.path().join("ws"), Some(name)).expect("init");
    (store, svc, tmp)
}

fn add_server(store: &JsonStore, workspace_id: &WorkspaceId) {
    let meta = ServerMeta {
        id: ServerId::parse("550e8400-e29b-41d4-a716-4466554400b1").expect("uuid"),
        name: ServerName::parse("test").expect("valid"),
        workspace_id: WorkspaceId::parse(workspace_id.as_str()).expect("uuid"),
        rel_dir: PathBuf::from("servers/test"),
        version: VersionId::new("1.21.1"),
        variant: ServerVariant::Vanilla,
        jar: None,
        settings: domain::server::settings::ServerSettings::default(),
        java_major: None,
        created_at: 1_700_000_000,
        updated_at: 1_700_000_000,
    };
    store.create(&meta).expect("register server");
}

/// 注册表里有服务器时，注销工作目录必须先清服务器登记（不删盘）
#[test]
fn remove_with_servers_without_purge_clears_fk() {
    let (store, svc, tmp) = setup("ws01");
    let ws = svc.resolve("ws01").expect("resolve");
    add_server(&store, &ws.id);

    svc.remove("ws01", false, tmp.path())
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
    let outside = tempfile::tempdir().expect("tmp outside");

    let out = svc
        .remove("ws01", true, outside.path())
        .expect("remove must not hit FOREIGN KEY");

    assert!(!dir.exists());
    assert!(out.disk_purged());
    assert!(store.list().expect("list servers").is_empty());
}
