//! 插件安装的签名与未签名策略
//!
//! 策略只写在 `service`：本地来源允许 `--allow-unsigned`，远端来源必须在
//! 拉取阶段拿到有效签名。签名本身（minisign 解析与验签）的用例在
//! `provider::plugin_sig`，这里只验证编排位置的判定。

// 集成测试的 setup 辅助函数不被 clippy 的测试豁免覆盖
#![allow(clippy::expect_used)]

use port::plugin::ArtifactOrigin;
use service::PluginService;
use std::path::Path;
use std::sync::Arc;
use storage::plugin::{JsonlAuditSink, PluginConfigFileStore, PluginFileStore};
use tempfile::TempDir;

/// 最小可安装产物：manifest + descriptor
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

    fn artifact(&self) -> ArtifactOrigin {
        let dir = self.tmp.path().join("demo");
        write_plugin(&dir);
        ArtifactOrigin::Local(dir)
    }
}

/// INST-SIG-009：本地未签名默认拒绝（`Unauthorized` / 退出码 6），
/// `--allow-unsigned` 才可安装，且记录保持未签名
#[test]
fn unsigned_local_artifact_requires_allow_unsigned() {
    let fixture = Fixture::new();
    let origin = fixture.artifact();

    let err = fixture
        .service
        .install(&origin, None, false)
        .expect_err("unsigned must be rejected");
    assert_eq!(err.category(), port::ErrorCategory::Unauthorized);

    let outcome = fixture
        .service
        .install(&origin, None, true)
        .expect("allow-unsigned installs");
    assert_eq!(outcome.record.id.as_str(), "demo");
    assert!(!outcome.record.signed);
    assert_eq!(outcome.record.sha256.as_deref().map(str::len), Some(64));
    assert!(
        outcome
            .record
            .source
            .as_deref()
            .is_some_and(|source| source.starts_with("local:"))
    );
}

/// 注册了 verifier 时，未签名策略仍由 service 执行：verifier 不能把
/// 未签名产物变成 `trusted`
#[test]
fn verifier_does_not_upgrade_unsigned_artifact() {
    let fixture = Fixture::new();
    let origin = fixture.artifact();
    let fetched = fixture.service.fetch_artifact(&origin).expect("fetch only");
    assert!(!fetched.artifact.signed);
    assert!(!fetched.artifact.trusted);
}

/// 没有装配来源时 `fetch_artifact` 是用法错误，不是静默成功
#[test]
fn fetch_without_source_is_usage_error() {
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
    let err = service
        .fetch_artifact(&ArtifactOrigin::Local(root.join("missing.zip")))
        .expect_err("no source");
    assert_eq!(err.category(), port::ErrorCategory::Usage);
}
