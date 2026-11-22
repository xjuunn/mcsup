//! 升级与版本切换：启用状态保留、版本保留 / 清理、`update --to` 切换
//!
//! 编排在 `service::plugin` 的 `install` / `activate_version` /
//! `prune_versions`；产物落点与锁文件也在那里。这里用本地目录来源验证
//! 可观察行为。

#![allow(clippy::expect_used)]

use domain::plugin::PluginId;
use domain::plugin::state::PluginState;
use port::plugin::ArtifactOrigin;
use service::PluginService;
use std::path::Path;
use std::sync::Arc;
use storage::plugin::{JsonlAuditSink, PluginConfigFileStore, PluginFileStore};
use tempfile::TempDir;

/// 最小可安装产物：无待确认能力；入口文件按平台形态写出
fn write_plugin(dir: &Path, version: &str) {
    write_plugin_with_caps(dir, version, r#"["events"]"#);
}

/// 同上，但显式给出 `capabilities`
fn write_plugin_with_caps(dir: &Path, version: &str, capabilities: &str) {
    std::fs::create_dir_all(dir).expect("artifact dir");
    std::fs::write(
        dir.join("manifest.json"),
        format!(
            r#"{{"schema":1,"id":"demo","name":"Demo","version":"{version}","api":1,
            "entry":{{"type":"process","cmd":"bin/demo",
            "platforms":{{"win32-x64":"bin/demo.exe"}}}},
            "capabilities":{capabilities},"license":"MIT"}}"#
        ),
    )
    .expect("manifest");
    std::fs::write(
        dir.join("descriptor.json"),
        r#"{"schema":1,"plugin_id":"demo","api":1,"ops":[]}"#,
    )
    .expect("descriptor");
    // 入口文件：安装时的存在性 / 形态检查要求它真实存在
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).expect("bin dir");
    let unix_entry = bin.join("demo");
    std::fs::write(&unix_entry, b"#!/bin/sh\nexit 0\n").expect("entry");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&unix_entry, std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
    }
    // Windows 的入口带 PE 头（`MZ`），内容无所谓
    std::fs::write(bin.join("demo.exe"), b"MZ\x90\x00payload").expect("entry exe");
}

fn demo_id() -> PluginId {
    PluginId::parse("demo").expect("id")
}

struct Fixture {
    service: PluginService,
    tmp: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path();
        let store = Arc::new(PluginFileStore::open(&root.join("db/plugins")).expect("store"));
        let service = PluginService::new(
            store,
            Arc::new(PluginConfigFileStore::new(root.join("plugin-data"))),
            None,
            Some(Arc::new(provider::plugin::PluginArtifactSource::new())),
            Some(Arc::new(provider::plugin::PluginArtifactVerifier::new())),
            Arc::new(JsonlAuditSink::new(root.join("db/audit"))),
            root.join("plugins"),
            root.join("plugin-data"),
            root.join("logs"),
        );
        Self { service, tmp }
    }

    fn artifact(&self, version: &str) -> ArtifactOrigin {
        let dir = self.tmp.path().join(format!("demo-{version}"));
        write_plugin(&dir, version);
        ArtifactOrigin::Local(dir)
    }

    fn install(&self, version: &str) {
        self.service
            .install(&self.artifact(version), None, false)
            .expect("install");
    }

    /// 断言用的形状：记录字段与版本目录集合
    fn shape(&self) -> Shape {
        let record = self.service.record(&demo_id()).expect("record");
        let root = self.tmp.path().join("plugins/demo");
        let mut versions: Vec<String> = std::fs::read_dir(&root)
            .expect("plugin root")
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        versions.sort();
        Shape {
            version: record.version,
            previous_version: record.previous_version,
            enabled: record.enabled,
            versions,
        }
    }
}

struct Shape {
    version: String,
    previous_version: Option<String>,
    enabled: bool,
    versions: Vec<String>,
}

/// 升级保留启用状态与上一版本；默认版本目录剩 2 个
#[test]
fn upgrade_keeps_enabled_and_previous_version() {
    let fixture = Fixture::new();
    fixture.install("0.1.0");
    fixture.service.enable(&demo_id()).expect("enable");

    fixture.install("0.2.0");
    let shape = fixture.shape();
    assert_eq!(shape.version, "0.2.0");
    assert_eq!(shape.previous_version.as_deref(), Some("0.1.0"));
    assert!(shape.enabled, "升级应保留启用状态");
    assert_eq!(shape.versions, vec!["0.1.0", "0.2.0"], "默认保留 2 个版本");
}

/// 再升一级：默认仍只留 2 个版本，最旧的被清掉
#[test]
fn third_install_prunes_the_oldest_by_default() {
    let fixture = Fixture::new();
    fixture.install("0.1.0");
    fixture.install("0.2.0");
    fixture.install("0.3.0");
    let shape = fixture.shape();
    assert_eq!(shape.version, "0.3.0");
    assert_eq!(shape.previous_version.as_deref(), Some("0.2.0"));
    assert_eq!(shape.versions, vec!["0.2.0", "0.3.0"]);
}

/// `--prune` 清得更旧，但**必须**留下回滚目标（`previous_version`）
///
/// 只剩当前版本时 `rollback` 无目标可用：刚升级上去的版本一旦启动失败，
/// 用户手上就没有任何能退回去的目录了。所以 `--prune` 的语义是「只留
/// 当前 + 上一版」，不是「只留当前」。
#[test]
fn explicit_prune_keeps_the_current_and_rollback_target() {
    let fixture = Fixture::new();
    fixture.install("0.1.0");
    let record = fixture
        .service
        .install(&fixture.artifact("0.2.0"), None, true)
        .expect("install")
        .record;
    let shape = fixture.shape();
    assert_eq!(record.version, "0.2.0");
    assert_eq!(
        shape.previous_version.as_deref(),
        Some("0.1.0"),
        "记录指回上一版本，回滚目标存在"
    );
    assert_eq!(shape.versions, vec!["0.1.0", "0.2.0"]);
}

/// `--prune` 在三级升级后清掉当前与上一版之外的版本
#[test]
fn explicit_prune_drops_versions_older_than_the_rollback_target() {
    let fixture = Fixture::new();
    fixture.install("0.1.0");
    fixture.install("0.2.0");
    let record = fixture
        .service
        .install(&fixture.artifact("0.3.0"), None, true)
        .expect("install")
        .record;
    let shape = fixture.shape();
    assert_eq!(record.version, "0.3.0");
    assert_eq!(shape.previous_version.as_deref(), Some("0.2.0"));
    assert_eq!(shape.versions, vec!["0.2.0", "0.3.0"]);
}

/// `update --to`：切回已安装的版本
#[test]
fn activate_version_switches_back() {
    let fixture = Fixture::new();
    fixture.install("0.1.0");
    fixture.install("0.2.0");

    let record = fixture
        .service
        .activate_version(&demo_id(), "0.1.0", false)
        .expect("activate");
    assert_eq!(record.version, "0.1.0");
    let shape = fixture.shape();
    assert_eq!(shape.version, "0.1.0");
    assert_eq!(shape.previous_version.as_deref(), Some("0.2.0"));
    assert_eq!(shape.versions, vec!["0.1.0", "0.2.0"]);
}

/// `activate_version` 对不存在的版本报错，不改记录
#[test]
fn activate_version_rejects_missing_artifact() {
    let fixture = Fixture::new();
    fixture.install("0.1.0");
    let err = fixture
        .service
        .activate_version(&demo_id(), "9.9.9", false)
        .expect_err("missing");
    assert!(err.to_string().contains("9.9.9"), "{err}");
    let shape = fixture.shape();
    assert_eq!(shape.version, "0.1.0");
}

/// 安装结束后锁已释放（锁文件按设计保留，第二次安装能立即进行）
///
/// 内核锁挂在文件句柄上，不再用「文件存在即持锁」——那种做法在持有者崩溃后
/// 只能靠时间戳回收，且回收窗口里两个进程可能各自认为拿到了锁。锁文件因此
/// 永不删除，这里只能验证「释放后立刻能再装」。
#[test]
fn install_lock_is_released_after_install() {
    let fixture = Fixture::new();
    fixture.install("0.1.0");
    fixture.install("0.2.0");
    let record = fixture.service.record(&demo_id()).expect("record");
    assert_eq!(record.version, "0.2.0");
}

/// 记录里带来源的插件可以按来源升级（`update` 不带 `--to` 的路径）
#[test]
fn upgrade_from_recorded_source_installs_latest() {
    let fixture = Fixture::new();
    fixture.install("0.1.0");
    let record = fixture.service.record(&demo_id()).expect("record");
    assert!(record.source.is_some(), "本地来源也应写进安装记录");

    // 来源目录复用同一份 0.2.0 产物：update 沿记录来源重装
    let record = fixture
        .service
        .install(&fixture.artifact("0.2.0"), None, false)
        .expect("reinstall")
        .record;
    assert_eq!(record.version, "0.2.0");
}

/// `update --to <当前版本>` 被拒绝：否则会把真正的回滚目标覆盖成同一版本
#[test]
fn activate_version_rejects_the_current_version() {
    let fixture = Fixture::new();
    fixture.install("0.1.0");
    fixture.install("0.2.0");
    let err = fixture
        .service
        .activate_version(&demo_id(), "0.2.0", false)
        .expect_err("same version");
    assert!(err.to_string().contains("already on version"), "{err}");
    let shape = fixture.shape();
    assert_eq!(
        shape.previous_version.as_deref(),
        Some("0.1.0"),
        "回滚目标不能被覆盖"
    );
}

/// 停用的插件切版本后仍可 `enable`
///
/// 无条件写 `last_state = stopped` 时，`stopped + Enabled` 不是合法迁移，
/// 插件会卡在「已启用但既不能 start 也不能 enable」的死角。
#[test]
fn activate_version_keeps_disabled_plugins_enableable() {
    let fixture = Fixture::new();
    fixture.install("0.1.0");
    fixture.install("0.2.0");
    fixture
        .service
        .activate_version(&demo_id(), "0.1.0", false)
        .expect("activate");
    let record = fixture.service.record(&demo_id()).expect("record");
    assert!(!record.enabled, "停用的插件切版本不该被顺手启用");
    assert_eq!(record.last_state, PluginState::Disabled);

    let enabled = fixture.service.enable(&demo_id()).expect("enable");
    assert!(enabled.enabled);
    assert_eq!(enabled.last_state, PluginState::Stopped);
}

/// 升级引入待授权能力时 `enabled` 必须清零，否则授权之后无路可走
#[test]
fn upgrade_needing_approval_clears_enabled() {
    let fixture = Fixture::new();
    fixture.install("0.1.0");
    fixture.service.enable(&demo_id()).expect("enable");
    // 0.2.0 多声明一个中风险能力（`console:read` 需要用户确认）
    let dir = fixture.tmp.path().join("demo-0.2.0-risk");
    write_plugin_with_caps(&dir, "0.2.0", r#"["events","console:read"]"#);
    let outcome = fixture
        .service
        .install(&ArtifactOrigin::Local(dir), None, false)
        .expect("upgrade");

    assert!(!outcome.pending.is_empty(), "新能力应待授权");
    let record = fixture.service.record(&demo_id()).expect("record");
    assert!(!record.enabled, "待审批与 enabled 互斥");
    assert_eq!(record.last_state, PluginState::PendingApproval);

    // 授权后回到 `disabled`，再 `enable` 才能进 `stopped`
    fixture
        .service
        .grant(
            &demo_id(),
            domain::plugin::capability::Capability::ConsoleRead,
        )
        .expect("grant");
    let enabled = fixture.service.enable(&demo_id()).expect("enable");
    assert_eq!(enabled.last_state, PluginState::Stopped);
}

/// 崩溃残留的临时解包目录在下一次安装时按年龄清掉
#[test]
fn install_sweeps_stale_temp_dirs() {
    let fixture = Fixture::new();
    let root = fixture.tmp.path().join("plugins");
    std::fs::create_dir_all(&root).expect("plugins root");
    // 目录名尾部就是创建时刻：两小时前 > 一小时的阈值
    let two_hours_ago = now_secs() - 7200;
    let stale = root.join(format!(".install-fetch-1-{two_hours_ago}-1"));
    std::fs::create_dir_all(stale.join("half-unpacked")).expect("stale dir");
    // 正在进行中的目录必须保住
    let fresh = root.join(format!(".install-fetch-1-{}-2", now_secs()));
    std::fs::create_dir_all(&fresh).expect("fresh dir");

    fixture.install("0.1.0");
    assert!(!stale.exists(), "崩溃残留应被清理");
    assert!(fresh.exists(), "本次的临时目录不能被误删");
}

fn now_secs() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs(),
    )
    .expect("fits")
}
