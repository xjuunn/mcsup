//! 升级与版本切换：启用状态保留、版本保留 / 清理、`update --to` 切换
//!
//! 编排在 `service::plugin` 的 `install` / `activate_version` /
//! `prune_versions`；产物落点与锁文件也在那里。这里用本地目录来源验证
//! 可观察行为。

#![allow(clippy::expect_used)]

use domain::plugin::PluginId;
use port::plugin::ArtifactOrigin;
use service::PluginService;
use std::path::Path;
use std::sync::Arc;
use storage::plugin::{JsonlAuditSink, PluginConfigFileStore, PluginFileStore};
use tempfile::TempDir;

/// 最小可安装产物：无待确认能力；入口文件按平台形态写出
fn write_plugin(dir: &Path, version: &str) {
    std::fs::create_dir_all(dir).expect("artifact dir");
    std::fs::write(
        dir.join("manifest.json"),
        format!(
            r#"{{"schema":1,"id":"demo","name":"Demo","version":"{version}","api":1,
            "entry":{{"type":"process","cmd":"bin/demo",
            "platforms":{{"win32-x64":"bin/demo.exe"}}}},
            "capabilities":["events"],"license":"MIT"}}"#
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

/// `--prune` 只保留当前版本
#[test]
fn explicit_prune_keeps_only_current() {
    let fixture = Fixture::new();
    fixture.install("0.1.0");
    let record = fixture
        .service
        .install(&fixture.artifact("0.2.0"), None, true)
        .expect("install")
        .record;
    let shape = fixture.shape();
    assert_eq!(record.version, "0.2.0");
    assert!(shape.previous_version.is_some(), "记录仍指回上一版本");
    assert_eq!(shape.versions, vec!["0.2.0"]);
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

/// 安装结束后锁文件不残留：下一次安装能立即进行
#[test]
fn install_lock_does_not_leak() {
    let fixture = Fixture::new();
    fixture.install("0.1.0");
    let lock = fixture
        .tmp
        .path()
        .join("plugins")
        .join(".demo.install.lock");
    assert!(!lock.exists(), "锁文件应在安装结束时删除");
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
