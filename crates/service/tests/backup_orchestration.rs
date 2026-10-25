//! 备份编排：`save-on` 兜底、故障路径的清理、跨服务器 GC
//!
//! 这一层测的是**时序与收场**，不是单个函数：每条用例都要求「无论第几步
//! 失败，现场都必须回到可继续运行的状态」。
//!
//! 用 `testkit` 的 `ServiceFixture`（真实目录布局）+ `FakeSupervisor`
//! （可注入 `send_line` 失败）+ `FailingContentStore`（包住真实 `CasStore`）。
//! 故障注入包在真实实现外面，所以断言的是「实现出错时上层怎么收场」，
//! 而不是「桩自己怎么出错」。

#![allow(clippy::expect_used)]

use backup::CasStore;
use domain::backup::{BackupScope, ContentHash, Page, RelPath, RetentionPolicy};
use port::ContentStore;
use service::{BackupError, BackupService, CreateOptions, CreateOutcome, ServerRegistry};
use std::path::Path;
use std::sync::Arc;
use testkit::{
    CommandLog, Failure, FakeSupervisor, ServiceFixture, assert_absent, assert_invariant,
    assert_present,
};

/// `save-off` / `save-all` / `save-on` 的线格式
const CMD_SAVE_OFF: &str = "save-off";
const CMD_SAVE_ON: &str = "save-on";

/// 一台「有内容」的服务器 + 可控的记录器
fn online_fixture() -> (ServiceFixture, CommandLog) {
    let fixture = ServiceFixture::with_server("alpha");
    fixture.server("alpha").write("world/level.dat", b"level");
    (fixture, CommandLog::running())
}

/// 把夹具接上 `BackupService`
fn service(fixture: &ServiceFixture, log: &CommandLog) -> BackupService {
    let registry = ServerRegistry::new(fixture.server_repo(), fixture.workspace_repo());
    BackupService::new(
        Arc::new(registry),
        Arc::new(FakeSupervisor::new(log.clone())),
    )
}

/// 一次不等待落盘的创建
fn create_now(service: &BackupService) -> Result<CreateOutcome, BackupError> {
    service.create(
        "alpha",
        &CreateOptions {
            flush_wait_ms: Some(0),
            ..CreateOptions::default()
        },
    )
}

/// 断言「最后一条发出的命令是 save-on」
fn assert_save_on_last(log: &CommandLog, scenario: &str) {
    let sent = log.sent();
    assert_invariant(sent.last().map(String::as_str) == Some(CMD_SAVE_ON), || {
        Failure::invariant("save-off 之后无论成功失败都必须补发 save-on")
            .expected(format!("最后一条是 {CMD_SAVE_ON}"))
            .actual(format!("{sent:?}"))
            .context("场景", scenario)
    });
}

// ---- save-on 兜底的四条路径 ----

/// 基准路径：运行中备份正常走完，三条命令顺序正确
#[test]
fn running_backup_toggles_save_around_scan() {
    let (fixture, log) = online_fixture();
    let service = service(&fixture, &log);

    let outcome = create_now(&service).expect("create");
    assert_invariant(outcome.snapshot.server_online, || {
        Failure::invariant("运行中备份必须记录 server_online")
    });
    assert_invariant(
        log.sent() == [CMD_SAVE_OFF, "save-all flush", CMD_SAVE_ON],
        || {
            Failure::invariant("运行中备份的命令顺序固定")
                .expected("[save-off, save-all flush, save-on]")
                .actual(format!("{:?}", log.sent()))
        },
    );
}

/// 范围为空（扫描阶段提前返回）：`save-on` 仍必须发出
#[test]
fn save_on_is_sent_when_scope_is_empty() {
    let (fixture, log) = online_fixture();
    let service = service(&fixture, &log);

    let err = service
        .create(
            "alpha",
            &CreateOptions {
                scope: Some(BackupScope::from_entries(["nonexistent"]).expect("scope")),
                flush_wait_ms: Some(0),
                ..CreateOptions::default()
            },
        )
        .expect_err("empty scope");
    assert_invariant(matches!(err, BackupError::EmptyScope), || {
        Failure::invariant("空范围必须报 EmptyScope").actual(err.to_string())
    });
    assert_invariant(
        log.sent().first().map(String::as_str) == Some(CMD_SAVE_OFF),
        || Failure::invariant("save-off 必须先于扫描").actual(format!("{:?}", log.sent())),
    );
    assert_save_on_last(&log, "范围为空");
    // 失败的快照不留占位目录
    let page = service.list("alpha", Page::new(1, 10)).expect("list");
    assert_invariant(page.total == 0, || {
        Failure::invariant("失败的创建不得留下快照记录").actual(page.total)
    });
}

/// `save-all` 发送失败：`save-on` 仍必须发出，且不再继续扫描
#[test]
fn save_on_is_sent_when_save_all_fails() {
    // 第 1 次（save-off）成功，第 2 次（save-all）失败
    let log = CommandLog::running().failing_send_on(2);
    let fixture = ServiceFixture::with_server("alpha");
    fixture.server("alpha").write("world/level.dat", b"level");
    let service = service(&fixture, &log);

    let err = create_now(&service).expect_err("save-all fails");
    assert_invariant(matches!(err, BackupError::Lifecycle(_)), || {
        Failure::invariant("save-all 失败必须原样报错")
            .expected("BackupError::Lifecycle")
            .actual(err.to_string())
    });
    // 记录里含尝试过的行：save-off、save-all（失败）、save-on
    assert_invariant(
        log.sent() == [CMD_SAVE_OFF, "save-all flush", CMD_SAVE_ON],
        || {
            Failure::invariant("失败后仍要补发 save-on")
                .expected("[save-off, save-all flush, save-on]")
                .actual(format!("{:?}", log.sent()))
        },
    );
    assert_save_on_last(&log, "save-all 失败");
    // 失败前没有落任何快照
    let page = service.list("alpha", Page::new(1, 10)).expect("list");
    assert_invariant(page.total == 0, || {
        Failure::invariant("save-all 失败时不应产生快照").actual(page.total)
    });
}

/// `save-on` 自身失败：错误不能吞掉原始失败，也不能 panic
#[test]
fn save_on_failure_does_not_mask_the_original_error() {
    let fixture = ServiceFixture::with_server("alpha");
    fixture.server("alpha").write("world/level.dat", b"level");

    // 第 3 次（save-on）失败，前两次成功：备份本身应当已经完成
    let log = CommandLog::running().failing_send_on(3);
    let registry = ServerRegistry::new(fixture.server_repo(), fixture.workspace_repo());
    let service = BackupService::new(
        Arc::new(registry),
        Arc::new(FakeSupervisor::new(log.clone())),
    );

    let outcome = create_now(&service).expect("backup itself succeeds");
    assert_invariant(!outcome.snapshot.id.as_str().is_empty(), || {
        Failure::invariant("备份本身应已完成")
    });
    // 尝试过 save-on，但没送达：`delivered` 里没有它
    assert_invariant(
        log.sent().last().map(String::as_str) == Some(CMD_SAVE_ON),
        || Failure::invariant("save-on 必须被尝试").actual(format!("{:?}", log.sent())),
    );
    assert_invariant(!log.delivered().contains(&CMD_SAVE_ON.to_owned()), || {
        Failure::invariant("注入失败的行不算送达").actual(format!("{:?}", log.delivered()))
    });
    // 快照仍然落盘且可校验：save-on 失败只影响服务器，不影响备份结果
    assert_invariant(
        service
            .verify("alpha", outcome.snapshot.id.as_str(), &mut |_, _| {})
            .expect("verify")
            .is_healthy(),
        || Failure::invariant("save-on 失败不应破坏已完成的快照"),
    );
}

/// 停服状态下不许发任何控制台命令
#[test]
fn stopped_server_gets_no_console_commands() {
    let log = CommandLog::stopped();
    let fixture = ServiceFixture::with_server("alpha");
    fixture.server("alpha").write("world/level.dat", b"level");
    let service = service(&fixture, &log);

    let outcome = create_now(&service).expect("create");
    assert_invariant(!outcome.snapshot.server_online, || {
        Failure::invariant("停服备份必须记录 server_online = false")
    });
    assert_invariant(log.sent().is_empty(), || {
        Failure::invariant("停服时不得发 save-off / save-on").actual(format!("{:?}", log.sent()))
    });
}

// ---- CAS 写入失败的收场 ----

/// 快照写完后不得残留 `.partial`：它是「写到一半」的墓碑
#[test]
fn completed_snapshot_leaves_no_partial_marker() {
    let (fixture, log) = online_fixture();
    let service = service(&fixture, &log);

    let outcome = create_now(&service).expect("create");
    let snapshot_dir = fixture.snapshots_root().join(outcome.snapshot.id.as_str());
    assert_absent(
        &snapshot_dir.join("snapshot.partial"),
        "快照写完后不得残留 .partial",
    );
    // 记录文件本体必须在
    assert_present(
        &snapshot_dir.join("snapshot.json"),
        "完成后的快照必须有记录文件",
    );
}

// ---- 跨服务器 GC ----

/// 工作区里两台服务器共用同一份内容时，回收 alpha 的对象不能碰 beta 的
#[test]
fn prune_keeps_objects_referenced_by_other_servers() {
    let (mut fixture, log) = online_fixture();
    fixture.add_server("beta");
    // 两台服务器各写一份完全相同的内容：CAS 里只有一个对象
    fixture
        .server("beta")
        .write("shared.bin", b"identical-bytes");
    fixture
        .server("alpha")
        .write("shared.bin", b"identical-bytes");
    let service = service(&fixture, &log);

    let alpha = create_now(&service).expect("alpha backup");
    let beta = service
        .create("beta", &CreateOptions::default())
        .expect("beta backup");

    // 两台服务器的 mcsup.json 内容不同，shared.bin 相同
    assert_invariant(
        alpha.snapshot.added >= 1 && beta.snapshot.added >= 1,
        || {
            Failure::invariant("两台服务器都应写入至少一个对象").actual(format!(
                "alpha={} beta={}",
                alpha.snapshot.added, beta.snapshot.added
            ))
        },
    );

    service
        .prune("alpha", Some(RetentionPolicy::Keep(0)), false)
        .expect("prune alpha");

    // beta 的快照必须仍然完整可校验
    let report = service
        .verify("beta", beta.snapshot.id.as_str(), &mut |_, _| {})
        .expect("verify beta");
    assert_invariant(report.is_healthy(), || {
        Failure::invariant("别的服务器仍在引用的对象不得被回收")
            .expected("0 个损坏对象")
            .actual(format!("{:?}", report.broken))
            .tree(&fixture.backups_root())
    });
}

/// 无人引用的对象才被回收；被引用的原样保留
#[test]
fn prune_without_policy_only_collects_garbage() {
    let (fixture, log) = online_fixture();
    let service = service(&fixture, &log);
    let outcome = create_now(&service).expect("create");

    // 手工放入一个无人引用的对象
    let cas = CasStore::new(&fixture.backups_root());
    let mut cursor = std::io::Cursor::new(b"orphan".to_vec());
    let (orphan, _) = cas
        .put(&mut cursor, &RelPath::parse("orphan").expect("rel"))
        .expect("put");

    let pruned = service.prune("alpha", None, false).expect("prune");
    assert_invariant(
        pruned.removed.is_empty() && pruned.objects_reclaimed == 1,
        || {
            Failure::invariant("无策略时只回收垃圾，不删快照")
                .expected("removed=0, reclaimed=1")
                .actual(format!(
                    "removed={}, reclaimed={}",
                    pruned.removed.len(),
                    pruned.objects_reclaimed
                ))
        },
    );
    assert_invariant(cas.stat(&orphan).expect("stat").is_none(), || {
        Failure::invariant("孤儿对象必须被回收")
    });
    assert_invariant(
        service
            .verify("alpha", outcome.snapshot.id.as_str(), &mut |_, _| {})
            .expect("verify")
            .is_healthy(),
        || Failure::invariant("被引用的对象不得被回收"),
    );
}

/// 空快照失败后，`backups/snapshots` 里不得留下任何记录目录
#[test]
fn empty_scope_failure_cleans_placeholder() {
    let (fixture, log) = online_fixture();
    let service = service(&fixture, &log);
    let placeholder = fixture.snapshots_root();

    let err = service
        .create(
            "alpha",
            &CreateOptions {
                scope: Some(BackupScope::from_entries(["nonexistent"]).expect("scope")),
                flush_wait_ms: Some(0),
                ..CreateOptions::default()
            },
        )
        .expect_err("empty");
    assert_invariant(matches!(err, BackupError::EmptyScope), || {
        Failure::invariant("空范围必须报 EmptyScope").actual(err.to_string())
    });

    let leftovers = list_dirs(&placeholder);
    assert_invariant(leftovers.is_empty(), || {
        Failure::invariant("失败后不得留下占位快照目录")
            .expected("无目录")
            .actual(format!("{leftovers:?}"))
            .tree(&placeholder)
    });
}

/// 对象路径由 hash shard 决定：同一 hash 恒定同一路径
#[test]
fn object_path_is_stable_for_hash() {
    let (fixture, _log) = online_fixture();
    let cas = CasStore::new(&fixture.backups_root());
    let hash = ContentHash::parse(&"a".repeat(64)).expect("hash");
    assert_invariant(
        cas.object_path(&hash) == cas.object_path(&hash.clone()),
        || Failure::invariant("同一 hash 必须映射到同一路径"),
    );
}

fn list_dirs(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut dirs: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    dirs.sort();
    dirs
}
