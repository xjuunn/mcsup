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
use domain::plugin::capability::Capability;
use domain::plugin::error::PluginErrorCode;
use domain::plugin::manifest::Manifest;
use domain::plugin::state::PluginState;
use plugin::Heartbeat;
use plugin::api::HostServices;
use plugin::control::{ControlClient, ControlServer, HostControl};
use plugin::event::EventBus;
use plugin::host::PluginHost;
use port::RepositoryError;
use port::plugin::{
    ConfigOps, HostError, NoopAuditSink, PluginConfigStore, PluginRecord, PluginStore,
    PluginSupervisor, StoredConfig, StreamSink,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
struct MemoryConfigStore {
    configs: Mutex<BTreeMap<String, StoredConfig>>,
}

impl PluginConfigStore for MemoryConfigStore {
    fn get(&self, id: &PluginId) -> Result<Option<StoredConfig>, RepositoryError> {
        Ok(self.configs.lock().expect("lock").get(id.as_str()).cloned())
    }

    fn put(&self, id: &PluginId, config: &StoredConfig) -> Result<(), RepositoryError> {
        self.configs
            .lock()
            .expect("lock")
            .insert(id.to_string(), config.clone());
        Ok(())
    }
}

/// 控制端点测试不覆盖配置读写；显式返回 `unsupported` 而不是静默成功。
struct StubConfig;

impl ConfigOps for StubConfig {
    fn get(&self, _id: &PluginId) -> Result<Value, HostError> {
        Err(HostError::unsupported("config is not wired in this test"))
    }

    fn set(&self, _id: &PluginId, _values: &Value) -> Result<Value, HostError> {
        Err(HostError::unsupported("config is not wired in this test"))
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

#[derive(Default)]
struct MemoryAudit {
    events: Mutex<Vec<port::plugin::AuditEvent>>,
}

impl port::plugin::AuditSink for MemoryAudit {
    fn record(&self, event: &port::plugin::AuditEvent) {
        self.events.lock().expect("lock").push(event.clone());
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
    // 优先硬链接：同一 inode 的首次执行只会被系统校验一次，整轮测试不必
    // 为每份拷贝重复付这笔开销。跨文件系统（或 Windows 权限不足）时回退到
    // 拷贝，行为不变。
    if std::fs::hard_link(&source, &target).is_err() {
        std::fs::copy(&source, &target).expect("copy fixture");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
                .expect("chmod fixture");
        }
    }
    warm_up_fixture(&target, &artifact_dir.join("warmup-missing.sock"));
    write_fixture_descriptor(&artifact_dir, id);
    artifact_dir
}

/// fixture 插件不经过 `service::install`，`descriptor.json` 由测试补齐
///
/// `PluginHost::call` / `stream` 执行前按已安装的 descriptor 校验输入与超时，
/// 缺文件时按安装损坏处理。这里声明 fixture 会应答的全部 op。
fn write_fixture_descriptor(artifact_dir: &Path, id: &PluginId) {
    let object = || json!({ "type": "object" });
    let op = |id: &str, kind: &str, risk: &str| {
        json!({
            "id": id,
            "kind": kind,
            "title": id,
            "risk": risk,
            "input": object(),
            "output": object(),
        })
    };
    let descriptor = json!({
        "schema": domain::plugin::DESCRIPTOR_SCHEMA_VERSION,
        "plugin_id": id.as_str(),
        "api": domain::plugin::PLUGIN_API_VERSION,
        "ops": [
            op("echo", "query", "read"),
            op("stream", "stream", "read"),
            op("stream_gap", "stream", "read"),
            op("fail", "action", "mutating"),
            op("stream_fail", "stream", "read"),
            op("mute_pong", "action", "mutating"),
            op("call_contract", "action", "mutating"),
        ],
    });
    std::fs::write(
        artifact_dir.join("descriptor.json"),
        serde_json::to_vec(&descriptor).expect("encode descriptor"),
    )
    .expect("write descriptor");
}

/// 预热 fixture，把首次执行的开销挪到握手计时之外
///
/// macOS 要校验刚写入的 Mach-O；`check.sh` 刚链接完几十个测试二进制时，
/// 校验与它们的启动挤在一起，拷贝出来的 fixture 可能在 `execve` 里停十几秒。
/// 这段是操作系统的启动成本，不是插件握手行为，不该占用宿主的 10 s 期限。
/// 先执行一次（端点不存在，必然立刻失败退出），之后的拉起复用已校验的
/// inode，握手计时才只覆盖插件自己的行为。
fn warm_up_fixture(target: &Path, missing_endpoint: &Path) {
    let output = std::process::Command::new(target)
        .arg("--endpoint")
        .arg(missing_endpoint)
        .env("MCSUP_PLUGIN_TOKEN", "warmup")
        .env("MCSUP_PLUGIN_ID", "warmup")
        .env("MCSUP_PLUGIN_SCOPE", "machine")
        .output()
        .expect("run fixture warm-up");
    assert!(
        !output.status.success(),
        "warm-up must fail: the endpoint does not exist"
    );
}

fn record(id: &PluginId, artifact_dir: PathBuf, version: &str) -> PluginRecord {
    record_with(id, artifact_dir, version, &[], &[], &[])
}

/// 带能力、契约声明与确认记录的安装记录
fn record_with(
    id: &PluginId,
    artifact_dir: PathBuf,
    version: &str,
    capabilities: &[&str],
    provides: &[&str],
    consumes: &[&str],
) -> PluginRecord {
    let manifest = json!({
        "schema": 1,
        "id": id.as_str(),
        "name": "Fixture",
        "version": version,
        "api": domain::plugin::PLUGIN_API_VERSION,
        "entry": { "type": "process", "cmd": "bin/plugin-fixture" },
        "capabilities": capabilities,
        "provides": provides,
        "consumes": consumes,
        "license": "MIT",
    });
    let raw = Manifest::from_json(&serde_json::to_vec(&manifest).expect("manifest json"))
        .expect("valid manifest")
        .raw;
    let approved = capabilities
        .iter()
        .filter_map(|raw| Capability::parse(raw).ok())
        .collect();
    PluginRecord {
        id: id.clone(),
        version: version.to_owned(),
        api: domain::plugin::PLUGIN_API_VERSION,
        scope: PluginScope::Machine,
        server: None,
        enabled: true,
        approved,
        manifest: raw,
        artifact_dir,
        installed_at: 0,
        updated_at: 0,
        last_state: PluginState::Stopped,
        last_error: None,
        previous_version: None,
        source: None,
        sha256: None,
        blocked: false,
        quarantined: false,
    }
}

fn host(data_dir: &Path, store: &Arc<MemoryStore>) -> Arc<PluginHost> {
    host_with_config(data_dir, store, Arc::new(MemoryConfigStore::default()))
}

fn host_with_config(
    data_dir: &Path,
    store: &Arc<MemoryStore>,
    configs: Arc<MemoryConfigStore>,
) -> Arc<PluginHost> {
    host_with_audit(data_dir, store, configs, Arc::new(NoopAuditSink))
}

fn host_with_audit(
    data_dir: &Path,
    store: &Arc<MemoryStore>,
    configs: Arc<MemoryConfigStore>,
    audit: Arc<dyn port::plugin::AuditSink>,
) -> Arc<PluginHost> {
    PluginHost::new(
        data_dir.to_path_buf(),
        Arc::clone(store) as Arc<dyn PluginStore>,
        configs,
        &Arc::new(HostServices::default()),
        EventBus::new(),
        audit,
        "test-host",
    )
}

#[test]
fn host_prc_001_starts_calls_streams_and_stops_fixture() {
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

/// 心跳丢失：插件不回 pong 时，宿主在 `miss_limit` 个窗口后终止会话
///
/// 宿主侧职责（plugins.md §6.7：心跳超时 → 关闭端点 + 强杀）。fixture 用
/// `mute_pong` 置位后静默，这里确认会话被摘除且不再应答调用。
#[test]
fn host_hb_002_muted_plugin_is_closed_by_heartbeat_timeout() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data_dir = dir.path().to_path_buf();
    let id = fixture_id();
    let artifact = install_fixture(&data_dir, &id);
    let store = Arc::new(MemoryStore::default());
    let fixture = record(&id, artifact, env!("CARGO_PKG_VERSION"));
    store.put(&fixture).expect("put record");
    let host = host(&data_dir, &store).with_heartbeat(Heartbeat {
        interval: Duration::from_millis(100),
        miss_limit: 2,
    });

    let info = host.start(&id).expect("start fixture");
    host.call(&id, "mute_pong", json!({}))
        .expect("mute the fixture");

    // 2 个 100 ms 窗口 + 监管线程 250 ms 轮询；给足余量
    let mut torn_down = false;
    let mut exit = None;
    for _ in 0..60 {
        std::thread::sleep(Duration::from_millis(50));
        let status = host.status(&id).expect("status");
        if status.session.as_ref().map(|s| s.session_id.as_str()) != Some(info.session_id.as_str())
        {
            torn_down = true;
            // 摘除会话与写状态是两步，等 `last_error` 落地
            if status.last_error.is_some() {
                exit = status.last_error;
                break;
            }
        }
    }
    assert!(
        torn_down,
        "muted plugin must be torn down after miss_limit windows"
    );
    let exit = exit.expect("torn-down session must record why it was closed");
    assert!(
        exit.contains("heartbeat timeout"),
        "close reason must be the heartbeat timeout, got: {exit}"
    );
    host.shutdown_all(Duration::from_secs(2));
}

/// 流式缺口：seq 不连续时宿主终止流，不把残缺序列当作成功
#[test]
fn proto_str_004_stream_seq_gap_terminates_the_stream() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data_dir = dir.path().to_path_buf();
    let id = fixture_id();
    let artifact = install_fixture(&data_dir, &id);
    let store = Arc::new(MemoryStore::default());
    let fixture = record(&id, artifact, env!("CARGO_PKG_VERSION"));
    store.put(&fixture).expect("put record");
    let host = host(&data_dir, &store);

    host.start(&id).expect("start fixture");
    let mut sink = CollectSink::default();
    let err = host
        .stream(&id, "stream_gap", json!({}), &mut sink)
        .expect_err("seq gap must fail the stream");
    assert!(
        err.message.contains("seq"),
        "error must mention the seq gap, got: {}",
        err.message
    );
    host.stop(&id, Duration::from_secs(2)).expect("stop");
}

/// 流式调用命中 `Result` 错误：错误必须交给消费者，不能退化成通道断开
///
/// 回归：宿主曾在 `Frame::Result` 命中 `Pending::Stream` 时丢掉该帧，
/// 调用方只能等到超时后看到 `unavailable`，无法得知插件拒绝的原因。
#[test]
fn proto_str_005_stream_error_result_is_forwarded_to_the_consumer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data_dir = dir.path().to_path_buf();
    let id = fixture_id();
    let artifact = install_fixture(&data_dir, &id);
    let store = Arc::new(MemoryStore::default());
    store
        .put(&record(&id, artifact, env!("CARGO_PKG_VERSION")))
        .expect("put record");
    let host = host(&data_dir, &store);

    host.start(&id).expect("start fixture");
    let mut sink = CollectSink::default();
    let err = host
        .stream(&id, "stream_fail", json!({}), &mut sink)
        .expect_err("plugin-side failure must surface");
    assert_eq!(err.code, PluginErrorCode::InvalidInput, "{}", err.message);
    assert!(
        err.message.contains("fixture refused"),
        "error must carry the plugin's reason, got: {}",
        err.message
    );
    assert!(sink.chunks.is_empty(), "no chunk may be delivered");

    host.stop(&id, Duration::from_secs(2)).expect("stop");
}

/// 终止插件进程（模拟崩溃，不经过 `stop`）
#[cfg(windows)]
fn kill_process(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// 终止插件进程（模拟崩溃，不经过 `stop`）
#[cfg(unix)]
fn kill_process(pid: u32) {
    let _ = std::process::Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status();
}

/// 崩溃退避期 `stop` 必须取消待执行的重启
///
/// 回归：退避线程按固定时长睡到点就重启，`stop` 因为没有会话返回
/// `not running`，随后插件又被拉起来——用户明确停机的意图被覆盖。
#[test]
fn host_prc_012_stop_cancels_restart_during_crash_backoff() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data_dir = dir.path().to_path_buf();
    let id = fixture_id();
    let artifact = install_fixture(&data_dir, &id);
    let store = Arc::new(MemoryStore::default());
    store
        .put(&record(&id, artifact, env!("CARGO_PKG_VERSION")))
        .expect("put record");
    let host = host(&data_dir, &store);

    let info = host.start(&id).expect("start fixture");
    kill_process(info.pid);

    // 崩溃后进入 1 s 退避：记录写明「restarting in」，且会话表里没有会话。
    // 状态不能用来判断：离线态一律折叠成 `Stopped`（`offline_state`），
    // 退避窗口只能靠 `last_error` 认出来。
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = host.status(&id).expect("status");
        let backing_off = status
            .last_error
            .as_deref()
            .is_some_and(|err| err.contains("restarting in"));
        if backing_off && status.session.is_none() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "plugin did not enter crash backoff in time: {status:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    host.stop(&id, Duration::from_secs(1))
        .expect("stop during backoff must cancel the pending restart");
    std::thread::sleep(Duration::from_millis(1600));
    let status = host.status(&id).expect("status after stop");
    assert_eq!(status.state, PluginState::Stopped);
    assert_eq!(host.session_count(), 0, "no process may be restarted");
}

/// 撤销列表命中时拒绝启动，且不拉起进程
#[test]
fn host_prc_013_revoked_version_is_rejected_before_spawn() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data_dir = dir.path().to_path_buf();
    let id = fixture_id();
    let artifact = install_fixture(&data_dir, &id);
    let store = Arc::new(MemoryStore::default());
    store
        .put(&record(&id, artifact, env!("CARGO_PKG_VERSION")))
        .expect("put record");
    let list = domain::plugin::revocation::RevocationList {
        schema: domain::plugin::revocation::REVOCATION_SCHEMA,
        updated_at: 0,
        entries: vec![domain::plugin::revocation::RevocationEntry {
            id: id.to_string(),
            versions: vec![env!("CARGO_PKG_VERSION").to_owned()],
            reason: Some("security advisory".to_owned()),
        }],
    };
    let revocations = Arc::new(StaticRevocations { list });
    let host = host(&data_dir, &store).with_revocations(revocations);

    let err = host.start(&id).expect_err("revoked version must not start");
    assert_eq!(err.code, PluginErrorCode::Denied, "{}", err.message);
    assert!(
        err.message.contains("revoked"),
        "error must explain the revocation, got: {}",
        err.message
    );
    assert_eq!(host.session_count(), 0, "no process may be spawned");
}

/// 固定内容的撤销列表
struct StaticRevocations {
    list: domain::plugin::revocation::RevocationList,
}

impl port::plugin::PluginRevocations for StaticRevocations {
    fn load(&self) -> Result<domain::plugin::revocation::RevocationList, HostError> {
        Ok(self.list.clone())
    }

    fn store(&self, _list: &domain::plugin::revocation::RevocationList) -> Result<(), HostError> {
        Ok(())
    }
}

/// 回归：`stop` 之后不得被监管线程当作崩溃自动重启
///
/// 监管线程每 250 ms 轮询一次存活状态，崩溃退避从 1 s 起。停机后 1.6 s 仍
/// 没有会话，才算「停止」真的停住了；`stop` 先摘会话时这里会重新拉起。
#[test]
fn host_prc_009_stopped_plugin_is_not_auto_restarted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data_dir = dir.path().to_path_buf();
    let id = fixture_id();
    let artifact = install_fixture(&data_dir, &id);
    let store = Arc::new(MemoryStore::default());
    store
        .put(&record(&id, artifact, env!("CARGO_PKG_VERSION")))
        .expect("put record");
    let host = host(&data_dir, &store);

    host.start(&id).expect("start fixture");
    host.stop(&id, Duration::from_secs(1))
        .expect("stop fixture");

    std::thread::sleep(Duration::from_millis(1600));
    let status = host.status(&id).expect("status after stop");
    assert_eq!(status.state, PluginState::Stopped);
    assert_eq!(host.session_count(), 0, "stopped plugin must stay stopped");
    host.shutdown_all(Duration::from_secs(1));
}

#[test]
fn host_prc_010_disabled_plugin_is_rejected_before_spawn() {
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
fn proto_hsk_003_handshake_identity_mismatch_is_reported_as_crashed() {
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
fn proto_cfg_001_welcome_and_live_config_reach_the_plugin() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data_dir = dir.path().to_path_buf();
    let id = fixture_id();
    let artifact = install_fixture(&data_dir, &id);
    let store = Arc::new(MemoryStore::default());
    store
        .put(&record(&id, artifact, env!("CARGO_PKG_VERSION")))
        .expect("put record");
    let configs = Arc::new(MemoryConfigStore::default());
    configs
        .put(
            &id,
            &StoredConfig {
                revision: 4,
                values: json!({ "interval_secs": 60 }),
            },
        )
        .expect("put config");
    let host = host_with_config(&data_dir, &store, configs);

    host.start(&id).expect("start fixture");
    let output = host.call(&id, "echo", json!({})).expect("call");
    assert_eq!(output["config_revision"], 4, "welcome must carry config");

    host.apply_config(&id, 5, &json!({ "interval_secs": 30 }))
        .expect("apply config");
    let output = host.call(&id, "echo", json!({})).expect("call");
    assert_eq!(
        output["config_revision"], 5,
        "live update must reach plugin"
    );
    host.shutdown_all(Duration::from_secs(2));
}

#[test]
fn cap_gate_006_blocked_and_quarantined_plugins_are_rejected_before_spawn() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data_dir = dir.path().to_path_buf();
    let id = fixture_id();
    let artifact = install_fixture(&data_dir, &id);
    let store = Arc::new(MemoryStore::default());
    let mut fixture = record(&id, artifact, env!("CARGO_PKG_VERSION"));
    fixture.blocked = true;
    store.put(&fixture).expect("put record");
    let host = host(&data_dir, &store);

    let err = host.start(&id).expect_err("blocked");
    assert_eq!(err.code, PluginErrorCode::Denied);
    assert_eq!(host.session_count(), 0, "no process may be spawned");

    fixture.blocked = false;
    fixture.quarantined = true;
    store.put(&fixture).expect("put record");
    let err = host.start(&id).expect_err("quarantined");
    assert_eq!(err.code, PluginErrorCode::Denied);
    assert_eq!(host.session_count(), 0);
}

#[test]
fn proto_ctl_001_control_channel_lists_calls_and_shuts_down() {
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
        Arc::new(StubConfig),
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

// ---- 契约路由 ----

/// 安装一个 fixture 记录并复制产物
fn install(
    data_dir: &Path,
    store: &MemoryStore,
    id: &PluginId,
    capabilities: &[&str],
    provides: &[&str],
    consumes: &[&str],
) {
    let artifact = install_fixture(data_dir, id);
    store
        .put(&record_with(
            id,
            artifact,
            env!("CARGO_PKG_VERSION"),
            capabilities,
            provides,
            consumes,
        ))
        .expect("put record");
}

fn plugin_id(raw: &str) -> PluginId {
    PluginId::parse(raw).expect("id")
}

#[test]
fn con_rte_001_contract_broadcast_aggregates_responses_and_failures() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data_dir = dir.path().to_path_buf();
    let store = Arc::new(MemoryStore::default());
    let caller = plugin_id("caller");
    let provider_a = plugin_id("provider-a");
    let provider_b = plugin_id("provider-b");
    install(&data_dir, &store, &caller, &[], &[], &["incident.report@1"]);
    install(
        &data_dir,
        &store,
        &provider_a,
        &[],
        &["incident.report@1"],
        &[],
    );
    install(
        &data_dir,
        &store,
        &provider_b,
        &[],
        &["incident.report@1"],
        &[],
    );
    let audit = Arc::new(MemoryAudit::default());
    let host = host_with_audit(
        &data_dir,
        &store,
        Arc::new(MemoryConfigStore::default()),
        audit.clone(),
    );

    let output = host
        .call(
            &caller,
            "call_contract",
            json!({
                "contract": "incident.report@1",
                "input": { "title": "boom", "fail_for": ["provider-b"] },
            }),
        )
        .expect("broadcast");

    let responses = output["responses"].as_array().expect("responses");
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["plugin_id"], "provider-a");
    assert_eq!(responses[0]["output"]["provider"], "provider-a");
    let failures = output["failures"].as_array().expect("failures");
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0]["plugin_id"], "provider-b");
    assert_eq!(failures[0]["error"]["code"], "denied");
    assert_eq!(failures[0]["error"]["category"], "unauthorized");

    let events = audit.events.lock().expect("lock");
    let contract_events: Vec<_> = events
        .iter()
        .filter(|event| event.action == "plugin.contract")
        .collect();
    assert_eq!(contract_events.len(), 2, "each provider must be audited");
    let details: Vec<Value> = contract_events
        .iter()
        .map(|event| serde_json::from_str(event.detail.as_deref().expect("detail")).expect("json"))
        .collect();
    for detail in &details {
        assert_eq!(detail["caller"], "caller");
        assert_eq!(detail["contract"], "incident.report@1");
    }
    assert!(
        details
            .iter()
            .any(|detail| detail["provider"] == "provider-a" && detail["result"] == "ok")
    );
    assert!(
        details
            .iter()
            .any(|detail| detail["provider"] == "provider-b" && detail["result"] == "denied")
    );

    host.shutdown_all(Duration::from_secs(2));
}

#[test]
fn con_rte_002_contract_unicast_requires_target_when_multiple_providers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data_dir = dir.path().to_path_buf();
    let store = Arc::new(MemoryStore::default());
    let caller = plugin_id("caller");
    let provider_a = plugin_id("provider-a");
    let provider_b = plugin_id("provider-b");
    install(&data_dir, &store, &caller, &[], &[], &["agent.converse@1"]);
    for provider in [&provider_a, &provider_b] {
        install(
            &data_dir,
            &store,
            provider,
            &["net:outbound"],
            &["agent.converse@1"],
            &[],
        );
    }
    let host = host(&data_dir, &store);

    let err = host
        .call(
            &caller,
            "call_contract",
            json!({
                "contract": "agent.converse@1",
                "input": { "message": "hi" },
            }),
        )
        .expect_err("ambiguity must require target");
    assert_eq!(err.code, PluginErrorCode::Conflict);

    let output = host
        .call(
            &caller,
            "call_contract",
            json!({
                "contract": "agent.converse@1",
                "target": "provider-b",
                "input": { "message": "hi" },
            }),
        )
        .expect("targeted unicast");
    assert_eq!(output["provider"], "provider-b");
    assert_eq!(output["echo"]["message"], "hi");

    let err = host
        .call(
            &caller,
            "call_contract",
            json!({
                "contract": "agent.converse@1",
                "target": "provider-c",
                "input": { "message": "hi" },
            }),
        )
        .expect_err("provider-c is not installed here");
    assert_eq!(err.code, PluginErrorCode::NotFound);

    host.shutdown_all(Duration::from_secs(2));
}

#[test]
fn con_rte_005_contract_cycle_is_rejected_as_conflict() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data_dir = dir.path().to_path_buf();
    let store = Arc::new(MemoryStore::default());
    let caller = plugin_id("caller");
    let provider = plugin_id("provider-a");
    install(&data_dir, &store, &caller, &[], &[], &["agent.converse@1"]);
    install(
        &data_dir,
        &store,
        &provider,
        &["net:outbound"],
        &["agent.converse@1"],
        &["agent.converse@1"],
    );
    let host = host(&data_dir, &store);

    let err = host
        .call(
            &caller,
            "call_contract",
            json!({
                "contract": "agent.converse@1",
                "target": "provider-a",
                "input": {
                    "chain": [
                        { "contract": "agent.converse@1", "target": "provider-a" },
                        { "contract": "agent.converse@1", "target": "provider-a" }
                    ]
                },
            }),
        )
        .expect_err("cycle must be rejected");
    assert_eq!(err.code, PluginErrorCode::Conflict);
    assert!(err.message.contains("cycle"), "{}", err.message);
    assert!(
        host.session_count() <= 2,
        "nested calls must not spawn extra sessions"
    );

    host.shutdown_all(Duration::from_secs(2));
}

#[test]
fn con_rte_006_contract_depth_is_bounded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data_dir = dir.path().to_path_buf();
    let store = Arc::new(MemoryStore::default());
    let caller = plugin_id("caller");
    let provider = plugin_id("provider-a");
    install(&data_dir, &store, &caller, &[], &[], &["agent.converse@1"]);
    install(
        &data_dir,
        &store,
        &provider,
        &[
            "net:outbound",
            "console:read",
            "console:write",
            "server:lifecycle",
        ],
        &[
            "agent.converse@1",
            "chat.ask@1",
            "lifecycle.act@1",
            "incident.report@1",
            "chat.publish@1",
        ],
        &[
            "agent.converse@1",
            "chat.ask@1",
            "lifecycle.act@1",
            "incident.report@1",
            "chat.publish@1",
        ],
    );
    let host = host(&data_dir, &store);

    let err = host
        .call(
            &caller,
            "call_contract",
            json!({
                "contract": "agent.converse@1",
                "target": "provider-a",
                "input": {
                    "chain": [
                        { "contract": "chat.ask@1", "target": "provider-a" },
                        { "contract": "lifecycle.act@1", "target": "provider-a" },
                        { "contract": "incident.report@1" },
                        { "contract": "chat.publish@1" }
                    ]
                },
            }),
        )
        .expect_err("depth must be bounded");
    assert_eq!(err.code, PluginErrorCode::LimitExceeded);
    assert!(err.message.contains("depth"), "{}", err.message);

    host.shutdown_all(Duration::from_secs(2));
}

#[test]
fn con_rte_003_contract_caller_must_declare_consumes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data_dir = dir.path().to_path_buf();
    let store = Arc::new(MemoryStore::default());
    let caller = plugin_id("caller");
    let provider = plugin_id("provider-a");
    install(&data_dir, &store, &caller, &[], &[], &[]);
    install(
        &data_dir,
        &store,
        &provider,
        &[],
        &["incident.report@1"],
        &[],
    );
    let host = host(&data_dir, &store);

    let err = host
        .call(
            &caller,
            "call_contract",
            json!({
                "contract": "incident.report@1",
                "input": { "title": "boom" },
            }),
        )
        .expect_err("undeclared consume must be denied");
    assert_eq!(err.code, PluginErrorCode::Denied);

    host.shutdown_all(Duration::from_secs(2));
}

#[test]
fn con_rte_004_contract_unknown_trace_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data_dir = dir.path().to_path_buf();
    let store = Arc::new(MemoryStore::default());
    let caller = plugin_id("caller");
    let provider = plugin_id("provider-a");
    install(&data_dir, &store, &caller, &[], &[], &["agent.converse@1"]);
    install(
        &data_dir,
        &store,
        &provider,
        &["net:outbound"],
        &["agent.converse@1"],
        &[],
    );
    let host = host(&data_dir, &store);

    let err = host
        .call(
            &caller,
            "call_contract",
            json!({
                "contract": "agent.converse@1",
                "target": "provider-a",
                "trace": "not-a-live-trace",
                "input": { "message": "hi" },
            }),
        )
        .expect_err("unknown trace must be rejected");
    assert_eq!(err.code, PluginErrorCode::Conflict);

    host.shutdown_all(Duration::from_secs(2));
}

#[test]
fn host_hb_001_live_session_survives_heartbeat_windows() {
    // 心跳窗口 150ms × 3：活跃插件必须一直在线。旧实现发完 ping 立刻检查
    // pong，活跃会话也会在 3 个窗口后被误判超时并重启。
    let dir = tempfile::tempdir().expect("tempdir");
    let data_dir = dir.path().to_path_buf();
    let id = fixture_id();
    let artifact = install_fixture(&data_dir, &id);
    let store = Arc::new(MemoryStore::default());
    let fixture = record(&id, artifact, env!("CARGO_PKG_VERSION"));
    store.put(&fixture).expect("put record");
    let host = host(&data_dir, &store).with_heartbeat(Heartbeat {
        interval: Duration::from_millis(150),
        miss_limit: 3,
    });
    // 覆盖值必须真的生效：`Arc::get_mut` 静默失败曾让这里退回 15 s 默认值，
    // 用例随之变成必然通过的空断言
    assert_eq!(
        host.heartbeat_for_test(),
        Heartbeat {
            interval: Duration::from_millis(150),
            miss_limit: 3,
        },
        "with_heartbeat must take effect"
    );

    let info = host.start(&id).expect("start fixture");
    std::thread::sleep(Duration::from_millis(1_200));

    let status = host.status(&id).expect("status");
    assert_eq!(status.state, PluginState::Running);
    let session = status.session.expect("session stays alive");
    assert_eq!(
        session.session_id, info.session_id,
        "active session must not be torn down by heartbeat"
    );
    assert_eq!(session.pid, info.pid);

    host.stop(&id, Duration::from_secs(2)).expect("stop");
}
