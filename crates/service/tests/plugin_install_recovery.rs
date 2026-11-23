//! 安装失败后的盘面：孤儿版本目录与启动失败的版本目录
//!
//! 覆盖两条恢复路径（`docs/plugins.md` §15.6）：
//!
//! - 目录存在但记录不指向它（上次升级在该版本上失败）→ 重装必须能覆盖；
//! - 新版本启动失败 → 记录切回旧版本，且失败版本的目录不留。
//!
//! 后者留着的话，这一版再也装不回来：`install` 报 already exists，
//! `rollback` 又没有回滚目标。

#![allow(clippy::expect_used)]

use domain::plugin::PluginId;
use domain::plugin::state::PluginState;
use port::plugin::{
    ArtifactOrigin, HostError, PluginRuntimeStatus, PluginSupervisor, SessionInfo, StreamSink,
};
use serde_json::Value;
use service::PluginService;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use storage::plugin::{JsonlAuditSink, PluginConfigFileStore, PluginFileStore};
use tempfile::TempDir;

/// 最小可安装产物：只声明低风险能力，安装即自动授权
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
    std::fs::write(bin.join("demo.exe"), b"MZ\x90\x00payload").expect("entry exe");
}

/// 只回答「运行中 / 已停止」的宿主：`start` 是否失败由测试开关控制
#[derive(Default)]
struct FakeSupervisor {
    running: Mutex<bool>,
    fail_start: Mutex<bool>,
}

impl FakeSupervisor {
    fn session() -> SessionInfo {
        SessionInfo {
            session_id: "test-session".to_owned(),
            granted: BTreeSet::new(),
            pid: 1,
            started_at: 0,
        }
    }

    fn set_fail_start(&self, fail: bool) {
        *self.fail_start.lock().expect("lock") = fail;
    }
}

impl PluginSupervisor for FakeSupervisor {
    fn start(&self, _id: &PluginId) -> Result<SessionInfo, HostError> {
        if *self.fail_start.lock().expect("lock") {
            return Err(HostError::internal("handshake timeout"));
        }
        *self.running.lock().expect("lock") = true;
        Ok(Self::session())
    }

    fn stop(&self, _id: &PluginId, _grace: Duration) -> Result<(), HostError> {
        *self.running.lock().expect("lock") = false;
        Ok(())
    }

    fn status(&self, id: &PluginId) -> Result<PluginRuntimeStatus, HostError> {
        let running = *self.running.lock().expect("lock");
        Ok(PluginRuntimeStatus {
            id: id.clone(),
            state: if running {
                PluginState::Running
            } else {
                PluginState::Stopped
            },
            session: running.then(Self::session),
            last_error: None,
        })
    }

    fn call(&self, _id: &PluginId, _op: &str, _input: Value) -> Result<Value, HostError> {
        Err(HostError::unsupported("fake supervisor cannot call ops"))
    }

    fn stream(
        &self,
        _id: &PluginId,
        _op: &str,
        _input: Value,
        _sink: &mut dyn StreamSink,
    ) -> Result<(), HostError> {
        Err(HostError::unsupported("fake supervisor cannot stream ops"))
    }

    fn reload(&self) -> Result<(), HostError> {
        Ok(())
    }
}

struct Fixture {
    service: PluginService,
    supervisor: Arc<FakeSupervisor>,
    tmp: TempDir,
}

impl Fixture {
    fn new(with_supervisor: bool) -> Self {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path();
        let store = Arc::new(PluginFileStore::open(&root.join("db/plugins")).expect("store"));
        let supervisor = Arc::new(FakeSupervisor::default());
        let service = PluginService::new(
            store,
            Arc::new(PluginConfigFileStore::new(root.join("plugin-data"))),
            with_supervisor.then(|| Arc::clone(&supervisor) as Arc<dyn PluginSupervisor>),
            Some(Arc::new(provider::plugin::PluginArtifactSource::new())),
            Some(Arc::new(provider::plugin::PluginArtifactVerifier::new())),
            Arc::new(JsonlAuditSink::new(root.join("db/audit"))),
            root.join("plugins"),
            root.join("plugin-data"),
            root.join("logs"),
        );
        Self {
            service,
            supervisor,
            tmp,
        }
    }

    fn artifact(&self, version: &str) -> ArtifactOrigin {
        let dir = self.tmp.path().join(format!("demo-{version}"));
        write_plugin(&dir, version);
        ArtifactOrigin::Local(dir)
    }

    /// 期望失败的安装：把错误交给调用方断言
    fn install_err(&self, version: &str) -> service::PluginError {
        let label = format!("install {version} should have been rejected");
        self.service
            .install(&self.artifact(version), None, false)
            .expect_err(&label)
    }

    fn install_ok(&self, version: &str) {
        self.service
            .install(&self.artifact(version), None, false)
            .expect("install");
    }

    fn version_dir(&self, version: &str) -> PathBuf {
        self.tmp.path().join("plugins/demo").join(version)
    }

    fn versions(&self) -> Vec<String> {
        let mut versions: Vec<String> = std::fs::read_dir(self.tmp.path().join("plugins/demo"))
            .expect("plugin root")
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        versions.sort();
        versions
    }
}

fn demo_id() -> PluginId {
    PluginId::parse("demo").expect("id")
}

/// 记录不指向的版本目录：重装必须覆盖，不能报 already exists
#[test]
fn install_replaces_a_version_dir_no_record_references() {
    let fixture = Fixture::new(false);
    fixture.install_ok("0.1.0");
    // 上一次升级在 0.2.0 上启动失败、目录留在盘上的样子
    let orphan = fixture.version_dir("0.2.0");
    std::fs::create_dir_all(orphan.join("bin")).expect("orphan dir");
    std::fs::write(orphan.join("stale.txt"), b"left over").expect("stale file");

    fixture.install_ok("0.2.0");

    let record = fixture.service.record(&demo_id()).expect("record");
    assert_eq!(record.version, "0.2.0");
    assert_eq!(fixture.versions(), vec!["0.1.0", "0.2.0"]);
    assert!(
        !orphan.join("stale.txt").exists(),
        "孤儿目录的内容必须被新产物替换"
    );
    assert!(orphan.join("manifest.json").is_file());
}

/// 判重只针对「记录正在用的那一版」：同版本重装仍然是 already exists
#[test]
fn reinstall_of_the_active_version_is_still_rejected() {
    let fixture = Fixture::new(false);
    fixture.install_ok("0.1.0");
    let err = fixture.install_err("0.1.0");
    assert!(
        matches!(err, service::PluginError::AlreadyExists(_)),
        "同版本重装应报 AlreadyExists，实际：{err:?}"
    );
    assert!(fixture.version_dir("0.1.0").is_dir(), "在用的目录不能被删");
}

/// 新版本启动失败：记录回滚到旧版本，失败版本的目录不留
#[test]
fn failed_start_removes_the_version_dir_that_could_not_start() {
    let fixture = Fixture::new(true);
    fixture.install_ok("0.1.0");
    fixture.service.enable(&demo_id()).expect("enable");
    fixture.service.start(&demo_id()).expect("start");
    assert!(fixture.service.is_running(&demo_id()));

    fixture.supervisor.set_fail_start(true);
    let err = fixture.install_err("0.2.0");

    // 调用方看到的是首个错误（启动失败），不是回滚过程的错误
    assert!(err.to_string().contains("handshake"), "{err}");
    let record = fixture.service.record(&demo_id()).expect("record");
    assert_eq!(record.version, "0.1.0", "记录必须切回旧版本");
    assert_eq!(record.last_state, PluginState::Failed);
    assert!(
        !fixture.version_dir("0.2.0").exists(),
        "起不来的版本目录必须删掉，否则这一版再也装不回来"
    );
    assert_eq!(fixture.versions(), vec!["0.1.0"]);

    // 同一个版本修好之后能重装（真正的缺陷场景：目录还在就会报 already exists）
    fixture.supervisor.set_fail_start(false);
    fixture.install_ok("0.2.0");
    let record = fixture.service.record(&demo_id()).expect("record");
    assert_eq!(record.version, "0.2.0");
    assert_eq!(fixture.versions(), vec!["0.1.0", "0.2.0"]);
}
