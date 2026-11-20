//! 插件热路径基准：descriptor 校验、帧编解码、宿主分发、事件扇出、列表
//!
//! 对应 `docs/plugins.md` §20.5 中不需要真实子进程的部分。握手（冷 / 热
//! 启动）在 `plugin_handshake.rs`，宿主空闲内存在 `plugin_memory.rs`。
//! 跑法：`cargo bench -p bench --bench plugin`。

// 基准夹具构造失败必须立刻可见（同 `hot_paths.rs` 的豁免说明）。
#![allow(clippy::expect_used, reason = "基准样本构造失败必须立刻中止")]

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufReader, Cursor};
use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mcsup::domain::plugin::PluginId;
use mcsup::domain::plugin::capability::Capability;
use mcsup::domain::plugin::descriptor::Descriptor;
use mcsup::domain::plugin::event::Subscription;
use mcsup::domain::server::event::DomainEvent;
use mcsup::domain::server::meta::ServerName;
use mcsup::domain::server::status::ServerStatus;
use mcsup::domain::server::version::VersionId;
use mcsup::plugin::{DispatchContext, HostServices, dispatch};
use mcsup::plugin::{EventBus, EventDelivery};
use mcsup::port::plugin::{HostError, ServerRead, ServerStatusView, ServerSummary};
use mcsup::protocol::plugin::{Call, Frame, ResultFrame, read_frame, write_frame};
use serde_json::{Value, json};

/// 生成含 `ops` 个 op 的合法 descriptor
fn descriptor_bytes(ops: usize) -> Vec<u8> {
    let entries: Vec<Value> = (0..ops)
        .map(|index| {
            json!({
                "id": format!("op{index}"),
                "kind": "query",
                "title": format!("查询 {index}"),
                "risk": "read",
                "input": { "type": "object", "properties": {}, "additionalProperties": false },
                "output": { "type": "object", "properties": {}, "additionalProperties": false }
            })
        })
        .collect();
    serde_json::to_vec(&json!({
        "schema": 1,
        "plugin_id": "bench",
        "api": 1,
        "ops": entries
    }))
    .expect("encode descriptor")
}

/// descriptor 解析 + 全量校验（安装时一次，但决定安装耗时）
fn bench_descriptor(c: &mut Criterion) {
    let plugin = PluginId::parse("bench").expect("id");
    let mut group = c.benchmark_group("plugin_descriptor");
    for ops in [1usize, 20, 100] {
        let bytes = descriptor_bytes(ops);
        group.bench_with_input(BenchmarkId::from_parameter(ops), &bytes, |b, bytes| {
            b.iter(|| Descriptor::from_json(bytes, &plugin).expect("valid"));
        });
    }
    group.finish();
}

/// 一帧的写入与读回（每帧都要走一次）
fn bench_frames(c: &mut Criterion) {
    let mut group = c.benchmark_group("plugin_frame");

    let payload = json!({ "items": vec!["x".repeat(16); 64] });
    let call = Call::host(1, "server:read", "list", payload);
    group.bench_function("call_write_read", |b| {
        b.iter(|| {
            let mut buffer = Vec::new();
            write_frame(&mut buffer, &Frame::Call(call.clone())).expect("write");
            let mut reader = BufReader::new(Cursor::new(buffer));
            let _ = read_frame(&mut reader).expect("read");
        });
    });

    let result = ResultFrame::ok(1, json!({ "items": vec!["x".repeat(16); 64] }));
    group.bench_function("result_write_read", |b| {
        b.iter(|| {
            let mut buffer = Vec::new();
            write_frame(&mut buffer, &Frame::Result(result.clone())).expect("write");
            let mut reader = BufReader::new(Cursor::new(buffer));
            let _ = read_frame(&mut reader).expect("read");
        });
    });

    group.finish();
}

/// 宿主能力分发：判定 + 参数解析 + 结果投影（不含业务）
struct MockRead;

impl ServerRead for MockRead {
    fn list(&self) -> Result<Vec<ServerSummary>, HostError> {
        Ok((0..3)
            .map(|index| ServerSummary {
                name: ServerName::parse(&format!("s{index}")).expect("name"),
                status: ServerStatus::Stopped,
                version: None,
            })
            .collect())
    }

    fn status(&self, _server: &ServerName) -> Result<ServerStatusView, HostError> {
        Ok(ServerStatusView {
            status: ServerStatus::Stopped,
            version: None,
            players: None,
            uptime: None,
        })
    }

    fn version(&self, _server: &ServerName) -> Result<Option<VersionId>, HostError> {
        Ok(None)
    }

    fn properties(&self, _server: &ServerName) -> Result<BTreeMap<String, String>, HostError> {
        Ok(BTreeMap::new())
    }
}

fn bench_dispatch(c: &mut Criterion) {
    let services = HostServices {
        server_read: Some(Arc::new(MockRead)),
        ..HostServices::default()
    };
    let plugin = PluginId::parse("bench").expect("id");
    let granted: BTreeSet<Capability> = [Capability::ServerRead].into_iter().collect();
    let context = DispatchContext {
        plugin: &plugin,
        granted: &granted,
        scope_server: None,
        contracts: None,
    };
    let call = Call::host(1, "server:read", "list", json!({}));

    c.bench_function("plugin_dispatch/server_read_list", |b| {
        b.iter(|| dispatch(&services, &call, context).expect("dispatch"));
    });
}

/// 事件扇出：一个事件投递给 10 个订阅者
struct CountingSink;

impl EventDelivery for CountingSink {
    fn deliver(&self, _kind: Subscription, _at: &str, _server: Option<&str>, _data: Value) {}
}

fn bench_event_fanout(c: &mut Criterion) {
    let mut group = c.benchmark_group("plugin_event_fanout");
    let event = DomainEvent::ChatMessage {
        server: "survival".to_owned(),
        from: "Steve".to_owned(),
        text: "<Steve> hello".to_owned(),
        at: "2026-01-01T00:00:00Z".to_owned(),
    };

    for subscribers in [1usize, 10] {
        let bus = EventBus::new();
        for index in 0..subscribers {
            let plugin = PluginId::parse(&format!("sink{index}")).expect("id");
            let mut subscriptions = BTreeSet::new();
            subscriptions.insert(Subscription::ChatMessage);
            bus.subscribe(plugin, subscriptions, Arc::new(CountingSink));
        }
        group.throughput(Throughput::Elements(1));
        group.bench_with_input(BenchmarkId::from_parameter(subscribers), &bus, |b, bus| {
            b.iter(|| bus.publish(event.clone()));
        });
    }
    group.finish();
}

/// `BENCH-006`：100 个非 resident 插件的列表
///
/// 非 resident 意味着没有会话、没有进程，列表就是读 100 份安装记录并解析
/// manifest。这里走真实文件后端（`PluginFileStore`），不用内存桩：落盘与
/// 反序列化正是列表的主要开销。
fn bench_list(c: &mut Criterion) {
    use mcsup::domain::plugin::manifest::Manifest;
    use mcsup::domain::plugin::state::PluginState;
    use mcsup::domain::plugin::{PLUGIN_API_VERSION, PluginScope};
    use mcsup::port::RepositoryError;
    use mcsup::port::plugin::{
        NoopAuditSink, PluginConfigStore, PluginRecord, PluginStore, StoredConfig,
    };

    struct NoConfig;

    impl PluginConfigStore for NoConfig {
        fn get(&self, _id: &PluginId) -> Result<Option<StoredConfig>, RepositoryError> {
            Ok(None)
        }

        fn put(&self, _id: &PluginId, _config: &StoredConfig) -> Result<(), RepositoryError> {
            Ok(())
        }
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let store = mcsup::storage::plugin::PluginFileStore::open(&dir.path().join("db/plugins"))
        .expect("open store");
    for index in 0..100 {
        let id = PluginId::parse(&format!("idle{index}")).expect("id");
        let manifest = json!({
            "schema": 1,
            "id": id.as_str(),
            "name": "Idle metadata",
            "version": "1.0.0",
            "api": PLUGIN_API_VERSION,
            "entry": { "type": "process", "cmd": "bin/plugin" },
            "capabilities": [],
            "license": "MIT",
        });
        let raw = Manifest::from_json(&serde_json::to_vec(&manifest).expect("manifest json"))
            .expect("valid manifest")
            .raw;
        let record = PluginRecord {
            id: id.clone(),
            version: "1.0.0".to_owned(),
            api: PLUGIN_API_VERSION,
            scope: PluginScope::Machine,
            server: None,
            enabled: false,
            approved: BTreeSet::new(),
            manifest: raw,
            artifact_dir: dir.path().join("artifacts").join(id.as_str()),
            installed_at: 0,
            updated_at: 0,
            last_state: PluginState::Disabled,
            last_error: None,
            previous_version: None,
            source: None,
            sha256: None,
            blocked: false,
            quarantined: false,
        };
        PluginStore::put(&store, &record).expect("put record");
    }

    // 宿主不可达时的 `PluginService::list`：只读记录，不触碰任何进程。
    let service = mcsup::service::PluginService::new(
        Arc::new(store) as Arc<dyn PluginStore>,
        Arc::new(NoConfig),
        None,
        None,
        None,
        Arc::new(NoopAuditSink),
        dir.path().join("plugins"),
        dir.path().join("plugin-data"),
        dir.path().join("logs"),
    );
    assert_eq!(service.list().expect("list").len(), 100);

    c.bench_function("plugin_list/100_non_resident", |b| {
        b.iter(|| service.list().expect("list"));
    });
}

criterion_group!(
    benches,
    bench_descriptor,
    bench_frames,
    bench_dispatch,
    bench_event_fanout,
    bench_list
);
criterion_main!(benches);
