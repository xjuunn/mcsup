//! 撤销列表：命中版本的启用 / 启动判定
//!
//! 判定规则在 `domain::plugin::revocation`，缓存的读写与权限在
//! `provider::plugin_revoke`。这里只验证编排位置：`enable` / `start` 必须在
//! 任何状态变更之前拒绝，列表读不懂时不得阻断。

// 集成测试的 setup 辅助函数不被 clippy 的测试豁免覆盖
#![allow(clippy::expect_used)]

use domain::plugin::revocation::RevocationList;
use port::plugin::{ArtifactOrigin, PluginRecord};
use service::PluginService;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use storage::plugin::{JsonlAuditSink, PluginConfigFileStore, PluginFileStore};
use tempfile::TempDir;

/// 最小可安装产物：无待确认能力，安装后即可启用
fn write_plugin(dir: &Path) {
    std::fs::create_dir_all(dir).expect("artifact dir");
    std::fs::write(
        dir.join("manifest.json"),
        r#"{"schema":1,"id":"demo","name":"Demo","version":"0.1.0","api":1,
            "entry":{"type":"process","cmd":"bin/demo"},
            "capabilities":["events"],"license":"MIT"}"#,
    )
    .expect("manifest");
    std::fs::write(
        dir.join("descriptor.json"),
        r#"{"schema":1,"plugin_id":"demo","api":1,"ops":[]}"#,
    )
    .expect("descriptor");
}

struct Fixture {
    service: PluginService,
    tmp: TempDir,
    revocation_path: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path();
        let store = Arc::new(PluginFileStore::open(&root.join("db/plugins")).expect("store"));
        let revocation_path = root.join("plugins/revoked.json");
        let revocations = Arc::new(provider::plugin_revoke_file::FileRevocations::new(
            revocation_path.clone(),
        ));
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
        )
        .with_revocations(revocations);
        Self {
            service,
            tmp,
            revocation_path,
        }
    }

    fn installed(&self) -> PluginRecord {
        let dir = self.tmp.path().join("demo");
        write_plugin(&dir);
        self.service
            .install(&ArtifactOrigin::Local(dir), None)
            .expect("install")
            .record
    }

    fn revoke(&self, json: &str) {
        let list = RevocationList::from_json(json.as_bytes()).expect("parse list");
        self.service.store_revocations(&list).expect("store");
    }
}

/// SEC-REV-001：撤销命中的版本不能启用
#[test]
fn revoked_version_cannot_be_enabled() {
    let fixture = Fixture::new();
    let record = fixture.installed();
    fixture.revoke(r#"{"schema":1,"entries":[{"id":"demo","versions":["0.1.0"]}]}"#);

    let message = fixture
        .service
        .revocation(&record)
        .expect("check")
        .expect("hit");
    assert!(message.contains("0.1.0"));

    let err = fixture
        .service
        .enable(&record.id)
        .expect_err("revoked must not enable");
    assert_eq!(err.category(), port::ErrorCategory::Unauthorized);
    assert!(!fixture.service.record(&record.id).expect("record").enabled);
}

/// 撤销不命中当前版本时照常启用
#[test]
fn other_version_is_not_affected() {
    let fixture = Fixture::new();
    let record = fixture.installed();
    fixture.revoke(r#"{"schema":1,"entries":[{"id":"demo","versions":["0.0.9"]}]}"#);
    assert!(
        fixture
            .service
            .revocation(&record)
            .expect("check")
            .is_none()
    );
    fixture.service.enable(&record.id).expect("enable");
    assert!(fixture.service.record(&record.id).expect("record").enabled);
}

/// 空版本列表表示整个插件下架
#[test]
fn empty_versions_revokes_every_version() {
    let fixture = Fixture::new();
    let record = fixture.installed();
    fixture.revoke(r#"{"schema":1,"entries":[{"id":"demo","versions":[],"reason":"malware"}]}"#);
    let err = fixture
        .service
        .enable(&record.id)
        .expect_err("revoked must not enable");
    assert!(err.to_string().contains("malware"));
}

/// SEC-REV-002：列表读不懂时不阻断已安装插件，只记录
#[test]
fn unreadable_list_does_not_block() {
    let fixture = Fixture::new();
    let record = fixture.installed();
    std::fs::create_dir_all(fixture.revocation_path.parent().expect("parent")).expect("dir");
    std::fs::write(&fixture.revocation_path, b"not json").expect("write garbage");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            &fixture.revocation_path,
            std::fs::Permissions::from_mode(0o600),
        )
        .expect("chmod");
    }

    assert!(fixture.service.revocation_list_check().is_err());
    fixture
        .service
        .enable(&record.id)
        .expect("unavailable list must not block");
}

/// 未装配撤销检查时不读取任何文件，也不阻断
#[test]
fn without_revocation_store_nothing_is_blocked() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();
    let store = Arc::new(PluginFileStore::open(&root.join("db/plugins")).expect("store"));
    let service = PluginService::new(
        store,
        Arc::new(PluginConfigFileStore::new(root.join("plugin-data"))),
        None,
        None,
        None,
        Arc::new(JsonlAuditSink::new(root.join("db/audit"))),
        root.join("plugins"),
        root.join("plugin-data"),
        root.join("logs"),
    );
    assert!(service.list_revocations().expect("list").is_empty());
    let err = service
        .store_revocations(&RevocationList::default())
        .expect_err("not configured");
    assert_eq!(err.category(), port::ErrorCategory::Usage);
}
