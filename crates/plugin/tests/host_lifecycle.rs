//! 插件宿主集成测试：真实子进程 + 真实端点
//!
//! fixture 是一个只实现协议的最小进程。测试把它复制进临时产物目录，用
//! [`PluginHost`] 的公开接口拉起、调用、读取流并停止，再通过
//! [`ControlServer`] / [`ControlClient`] 覆盖 CLI 使用的控制通道。
//!
//! 不访问真实网络、真实用户目录或真实 Minecraft 进程。

#![allow(clippy::expect_used, clippy::unwrap_used)]

use domain::plugin::PluginId;
use domain::plugin::PluginScope;
use domain::plugin::error::PluginErrorCode;
use domain::plugin::manifest::Manifest;
use domain::plugin::state::PluginState;
use plugin::api::HostServices;
use plugin::control::{ControlClient, ControlServer, HostControl};
use plugin::event::EventBus;
use plugin::host::PluginHost;
use port::RepositoryError;
use port::plugin::{
    HostError, NoopAuditSink, PluginRecord, PluginStore, PluginSupervisor, StreamSink,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
struct MemoryStore {
    records: Mutex<BTreeMap<String, PluginRecord>>,
}

impl PluginStore for MemoryStore {
    fn list(&self) -> Result<Vec<PluginRecord>, RepositoryError> {
        Ok(self
            .records
            .lock()
            .expect("lock")
            .values()
            .cloned()
            .collect())
    }

    fn get(&self, id: &PluginId) -> Result<PluginRecord, RepositoryError> {
        self.records
            .lock()
            .expect("lock")
            .get(id.as_str())
            .cloned()
            .ok_or_else(|| RepositoryError::NotFound(id.to_string()))
    }

    fn put(&self, record: &PluginRecord) -> Result<(), RepositoryError> {
        self.records
            .lock()
            .expect("lock")
            .insert(record.id.to_string(), record.clone());
        Ok(())
    }

    fn remove(&self, id: &PluginId) -> Result<(), RepositoryError> {
        self.records
            .lock()
            .expect("lock")
            .remove(id.as_str())
            .map(|_| ())
            .ok_or_else(|| RepositoryError::NotFound(id.to_string()))
    }
}

#[derive(Default)]
struct CollectSink {
    chunks: Vec<(Value, bool)>,
}

impl StreamSink for CollectSink {
    fn on_chunk(&mut self, chunk: &Value, done: bool) -> Result<(), HostError> {
        self.chunks.push((chunk.clone(), done));
        Ok(())
    }
}

fn fixture_id() -> PluginId {
    PluginId::parse("fixture").expect("id")
}

/// 把 fixture 二进制复制进 `artifacts/<id>/bin/`，返回产物根目录
fn install_fixture(data_dir: &Path, id: &PluginId) -> PathBuf {
    let artifact_dir = data_dir.join("artifacts").join(id.as_str());
    let bin_dir = artifact_dir.join("bin");
    std::fs::create_dir_all(&bin_dir).expect("create bin dir");
    let source = PathBuf::from(env!("CARGO_BIN_EXE_plugin-fixture"));
    let target = bin_dir.join("plugin-fixture");
    std::fs::copy(&source, &target).expect("copy fixture");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fixture");
    }
    artifact_dir
}

fn record(id: &PluginId, artifact_dir: PathBuf, version: &str) -> PluginRecord {
    let manifest = json!({
        "schema": 1,
        "id": id.as_str(),
        "name": "Fixture",
        "version": version,
        "api": domain::plugin::PLUGIN_API_VERSION,
        "entry": { "type": "process", "cmd": "bin/plugin-fixture" },
        "capabilities": [],
        "license": "MIT",
    });
    let raw = Manifest::from_json(&serde_json::to_vec(&manifest).expect("manifest json"))
        .expect("valid manifest")
        .raw;
    PluginRecord {
        id: id.clone(),
        version: version.to_owned(),
        api: domain::plugin::PLUGIN_API_VERSION,
        scope: PluginScope::Machine,
        server: None,
        enabled: true,
        approved: BTreeSet::default(),
        manifest: raw,
        artifact_dir,
        installed_at: 0,
        updated_at: 0,
        last_state: PluginState::Stopped,
        last_error: None,
        previous_version: None,
        source: None,
        sha256: None,
        signed: true,
    }
}

fn host(data_dir: &Path, store: &Arc<MemoryStore>) -> Arc<PluginHost> {
    PluginHost::new(
        data_dir.to_path_buf(),
        Arc::clone(store) as Arc<dyn PluginStore>,
        Arc::new(HostServices::default()),
        EventBus::new(),
        Arc::new(NoopAuditSink),
        "test-host",
    )
}

#[test]
fn host_starts_calls_streams_and_stops_fixture() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data_dir = dir.path().to_path_buf();
    let id = fixture_id();
    let artifact = install_fixture(&data_dir, &id);
    let store = Arc::new(MemoryStore::default());
    let fixture = record(&id, artifact, env!("CARGO_PKG_VERSION"));
    store.put(&fixture).expect("put record");
    let host = host(&data_dir, &store);

    let info = host.start(&id).expect("start fixture");
    assert!(info.pid > 0, "handshake must report a live pid");
    assert_eq!(
        host.status(&id).expect("status").state,
        PluginState::Running
    );

    let output = host
        .call(&id, "echo", json!({ "x": 1 }))
        .expect("call echo");
    assert_eq!(output["op"], "echo");
    assert_eq!(output["input"]["x"], 1);
    assert_eq!(output["pid"], info.pid);

    let mut sink = CollectSink::default();
    host.stream(&id, "stream", json!({}), &mut sink)
        .expect("stream");
    assert_eq!(sink.chunks.len(), 3);
    assert_eq!(sink.chunks[0].0["index"], 0);
    assert_eq!(sink.chunks[2].0["index"], 2);
    assert!(sink.chunks[2].1, "last chunk must carry done=true");

    let err = host
        .call(&id, "fail", json!({}))
        .expect_err("fixture fails this op");
    assert_eq!(err.code, PluginErrorCode::InvalidInput);

    host.stop(&id, Duration::from_secs(2))
        .expect("stop fixture");
    let status = host.status(&id).expect("status after stop");
    assert_eq!(status.state, PluginState::Stopped);
    assert!(status.session.is_none(), "session must be gone after stop");

    // 惰性启动：停止后再次调用会重新拉起进程。
    let second = host.start(&id).expect("start again");
    assert_ne!(second.session_id, info.session_id);
    host.shutdown_all(Duration::from_secs(2));
}

#[test]
fn disabled_plugin_is_rejected_before_spawn() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data_dir = dir.path().to_path_buf();
    let id = fixture_id();
    let artifact = install_fixture(&data_dir, &id);
    let store = Arc::new(MemoryStore::default());
    let mut fixture = record(&id, artifact, env!("CARGO_PKG_VERSION"));
    fixture.enabled = false;
    store.put(&fixture).expect("put record");
    let host = host(&data_dir, &store);

    let err = host.start(&id).expect_err("disabled plugin must not start");
    assert_eq!(err.code, PluginErrorCode::Conflict);
}

#[test]
fn handshake_identity_mismatch_is_reported_as_crashed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data_dir = dir.path().to_path_buf();
    let id = fixture_id();
    let artifact = install_fixture(&data_dir, &id);
    let store = Arc::new(MemoryStore::default());
    let fixture = record(&id, artifact, "9.9.9");
    store.put(&fixture).expect("put record");
    let host = host(&data_dir, &store);

    let err = host.start(&id).expect_err("identity mismatch must fail");
    assert_eq!(err.code, PluginErrorCode::Crashed);
    assert!(err.message.contains("identity mismatch"), "{}", err.message);
    assert_eq!(host.status(&id).expect("status").state, PluginState::Failed);
}

#[test]
fn control_channel_lists_calls_and_shuts_down() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data_dir = dir.path().to_path_buf();
    let id = fixture_id();
    let artifact = install_fixture(&data_dir, &id);
    let store = Arc::new(MemoryStore::default());
    store
        .put(&record(&id, artifact, env!("CARGO_PKG_VERSION")))
        .expect("put record");
    let host = host(&data_dir, &store);

    let server = ControlServer::bind(&data_dir).expect("bind control");
    let control = Arc::new(HostControl::new(
        Arc::clone(&host) as Arc<dyn PluginSupervisor>,
        Arc::clone(&store) as Arc<dyn PluginStore>,
        data_dir.join("logs").join("plugins"),
        Arc::new(NoopAuditSink),
    ));
    let serve = std::thread::spawn(move || server.serve(control));

    let client = ControlClient::new(&data_dir);
    let list = client.list().expect("list");
    let items = list["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], "fixture");
    assert_eq!(items[0]["state"], "stopped");

    let info = client.start(&id).expect("control start");
    assert!(info.pid > 0);

    // 日志采集是后台线程：轮询到 fixture 的启动行，避免固定 sleep。
    let mut captured = false;
    for _ in 0..40 {
        let logs = client.logs(&id, None, 50).expect("control logs");
        if logs["lines"].to_string().contains("fixture ready") {
            captured = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(captured, "plugin log must capture fixture stderr");

    let status = client.status(&id).expect("control status");
    assert_eq!(status.state, PluginState::Running);

    let output = client
        .call(&id, "echo", json!({ "via": "control" }))
        .expect("control call");
    assert_eq!(output["input"]["via"], "control");

    let mut sink = CollectSink::default();
    client
        .stream(&id, "stream", json!({}), &mut sink)
        .expect("control stream");
    assert_eq!(sink.chunks.len(), 3);

    client
        .stop(&id, Duration::from_secs(2))
        .expect("control stop");
    client.shutdown().expect("control shutdown");
    serve.join().expect("join serve").expect("serve result");
    host.shutdown_all(Duration::from_secs(1));
}
