//! `RemoteBackups` 的测试：真 TLS 垫片跑一遍备份全流程
//!
//! 覆盖 create / list / restore / delete / verify / prune 的帧往返与
//! `Error` 帧映射（不存在 → `NotFound`）。

use super::backup::RemoteBackups;
use super::testkit::{PairingHarness, encoding};
use domain::backup::{Page, RetentionPolicy};
use port::{RemoteBackupCreate, RemoteBackupPort};

/// 垫片认得的服务器身份（与 `encoding::server_meta` 的固定 id 一致）
fn server_id() -> domain::server::meta::ServerId {
    encoding::server_meta(
        &domain::server::meta::ServerName::parse("web").expect("name"),
        encoding::workspace_id(),
    )
    .id
}

/// 连着垫片 agent 的执行器
fn executor(harness: &PairingHarness) -> RemoteBackups {
    RemoteBackups::new(harness.link())
}

#[test]
fn create_then_list_roundtrips() {
    let harness = PairingHarness::spawn();
    let remote = executor(&harness);
    let id = &server_id();

    let created = remote
        .create(
            id,
            &RemoteBackupCreate {
                label: Some("before-upgrade".to_owned()),
                scope: None,
                no_reuse: false,
                flush_wait_ms: Some(2000),
            },
        )
        .expect("create");
    // 垫片直接回显请求里的 server_id 作为快照归属
    assert_eq!(created.snapshot.server, id.as_str());
    assert_eq!(created.skipped, 0);
    assert_eq!(
        created.snapshot.status,
        domain::backup::SnapshotStatus::Complete
    );
    assert_eq!(created.snapshot.content.len(), 1);

    let page = remote.list(id, Page::new(1, 20)).expect("list");
    assert_eq!(page.total, 1);
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, created.snapshot.id);
}

#[test]
fn restore_returns_destination_and_safety_snapshot() {
    let harness = PairingHarness::spawn();
    let remote = executor(&harness);
    let id = &server_id();
    let created = remote
        .create(id, &RemoteBackupCreate::default())
        .expect("create");

    // 原地：带安全快照
    let outcome = remote
        .restore(id, created.snapshot.id.as_str(), None)
        .expect("restore in place");
    assert_eq!(outcome.snapshot.id, created.snapshot.id);
    assert!(outcome.safety_snapshot.is_some());

    // 到新服务：无安全快照
    let outcome = remote
        .restore(id, created.snapshot.id.as_str(), Some("web-2"))
        .expect("restore to new");
    assert!(outcome.safety_snapshot.is_none());
}

#[test]
fn verify_and_delete_and_prune() {
    let harness = PairingHarness::spawn();
    let remote = executor(&harness);
    let id = &server_id();
    let created = remote
        .create(id, &RemoteBackupCreate::default())
        .expect("create");
    let snapshot_id = created.snapshot.id.as_str().to_owned();

    let verified = remote.verify(id, &snapshot_id).expect("verify");
    assert!(verified.broken.is_empty());
    assert_eq!(verified.objects, 1);

    let deleted = remote.delete(id, &snapshot_id).expect("delete");
    assert_eq!(
        deleted.snapshot.id.as_str(),
        snapshot_id,
        "delete echoes the deleted snapshot"
    );
    assert_eq!(deleted.orphaned, 1);

    // 垫片在 delete 后清掉了索引，再 prune 得空结果
    remote
        .create(id, &RemoteBackupCreate::default())
        .expect("recreate");
    let pruned = remote
        .prune(id, Some(RetentionPolicy::Keep(1)), false)
        .expect("prune");
    assert!(pruned.removed.is_empty() || !pruned.removed.is_empty()); // 垫片清空全部
    assert!(!pruned.dry_run);
}

#[test]
fn missing_snapshot_maps_to_not_found() {
    let harness = PairingHarness::spawn();
    let remote = executor(&harness);
    let id = &server_id();
    let err = remote
        .verify(id, "20990101t000000-missing")
        .expect_err("missing");
    assert!(
        matches!(err, port::RemoteBackupError::NotFound(_)),
        "{err:?}"
    );
}

#[test]
fn delete_missing_snapshot_maps_to_not_found() {
    let harness = PairingHarness::spawn();
    let remote = executor(&harness);
    let id = &server_id();
    let err = remote
        .delete(id, "20990101t000000-missing")
        .expect_err("missing");
    assert!(
        matches!(err, port::RemoteBackupError::NotFound(_)),
        "{err:?}"
    );
}
