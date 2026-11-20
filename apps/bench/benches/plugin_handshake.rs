//! 真实握手基准：宿主 + 真实子进程（`BENCH-001` / `BENCH-002`）
//!
//! 量的是 `PluginHost::start` 的完整耗时：建端点 → 拉起进程 → `hello` →
//! 授权计算 → `welcome`。停止放在计时之外，否则量到的是停机而不是握手。
//!
//! 冷 / 热的口径：
//!
//! - 冷：每次都把一个**新拷贝**的 fixture 放进产物目录再拉起。新 inode 的
//!   首次执行包含操作系统的可执行校验（macOS 的代码签名校验等），这正是
//!   真实安装（解包 → 首次运行）会付的代价。
//! - 热：同一个产物目录反复 `start` → `stop`。进程已执行过，校验与页缓存
//!   命中，只剩拉起与握手。
//!
//! 阈值按 p95 判定（`docs/plugins.md` §20.5）。criterion 只报均值 / 中位数，
//! 所以这个基准自己采样并算分位，不套 criterion。超标时退出码非 0。
//!
//! 跑法：`cargo bench -p bench --bench plugin_handshake`

// 基准夹具构造失败必须立刻可见（同 `hot_paths.rs` 的豁免说明）。
#![allow(clippy::expect_used, reason = "基准样本构造失败必须立刻中止")]
// 报告本身就走 stdout：阈值按 p95 判定，criterion 不报分位。
#![allow(clippy::print_stdout, reason = "基准报告输出")]

use mcsup::domain::plugin::manifest::Manifest;
use mcsup::domain::plugin::state::PluginState;
use mcsup::domain::plugin::{PLUGIN_API_VERSION, PluginId, PluginScope};
use mcsup::plugin::api::HostServices;
use mcsup::plugin::event::EventBus;
use mcsup::plugin::host::PluginHost;
use mcsup::port::RepositoryError;
use mcsup::port::plugin::{
    NoopAuditSink, PluginConfigStore, PluginRecord, PluginStore, PluginSupervisor, StoredConfig,
};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// `BENCH-001` 冷启动握手 p95 上限
const COLD_BUDGET: Duration = Duration::from_millis(500);

/// `BENCH-002` 热启动握手 p95 上限
const HOT_BUDGET: Duration = Duration::from_millis(150);

const SAMPLES: usize = 30;
const WARMUP: usize = 5;

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

/// 基准不需要配置；返回空配置，写入丢弃
struct NoConfig;

impl PluginConfigStore for NoConfig {
    fn get(&self, _id: &PluginId) -> Result<Option<StoredConfig>, RepositoryError> {
        Ok(None)
    }

    fn put(&self, _id: &PluginId, _config: &StoredConfig) -> Result<(), RepositoryError> {
        Ok(())
    }
}

fn record(id: &PluginId, artifact_dir: PathBuf) -> PluginRecord {
    let version = env!("CARGO_PKG_VERSION");
    let manifest = json!({
        "schema": 1,
        "id": id.as_str(),
        "name": "Benchmark fixture",
        "version": version,
        "api": PLUGIN_API_VERSION,
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
        api: PLUGIN_API_VERSION,
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
        signed: false,
        blocked: false,
        quarantined: false,
    }
}

/// 把一个全新的 fixture 拷贝放进 `artifact_dir`，返回产物目录
///
/// 必须用 `copy` 而不是 `hard_link`：硬链接与源文件同一个 inode，那已经是
/// 热执行；冷启动的意义在于「第一次执行这个文件」。
fn install_fresh(artifact_dir: &Path) {
    if artifact_dir.exists() {
        std::fs::remove_dir_all(artifact_dir).expect("clean previous artifact");
    }
    let bin_dir = artifact_dir.join("bin");
    std::fs::create_dir_all(&bin_dir).expect("create bin dir");
    let source = PathBuf::from(env!("CARGO_BIN_EXE_mcsup-plugin-bench-fixture"));
    let target = bin_dir.join("plugin-fixture");
    std::fs::copy(&source, &target).expect("copy fixture");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fixture");
    }
}

struct Bed {
    dir: tempfile::TempDir,
    store: Arc<MemoryStore>,
    host: Arc<PluginHost>,
    id: PluginId,
}

impl Bed {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().join("data");
        let store = Arc::new(MemoryStore::default());
        let host = PluginHost::new(
            data_dir,
            Arc::clone(&store) as Arc<dyn PluginStore>,
            Arc::new(NoConfig),
            &Arc::new(HostServices::default()),
            EventBus::new(),
            Arc::new(NoopAuditSink),
            "bench-host",
        );
        let id = PluginId::parse("benchmark").expect("id");
        Self {
            dir,
            store,
            host,
            id,
        }
    }

    /// 计时一次「拉起并握手」，随后停止（停机不计入）
    fn timed_start(&self, artifact_dir: &Path) -> Duration {
        self.store
            .put(&record(&self.id, artifact_dir.to_path_buf()))
            .expect("put record");
        let started = Instant::now();
        self.host.start(&self.id).expect("handshake");
        let elapsed = started.elapsed();
        self.host
            .stop(&self.id, Duration::from_secs(1))
            .expect("stop");
        elapsed
    }

    fn cold_dir(&self) -> PathBuf {
        self.dir.path().join("artifacts/cold")
    }

    fn hot_dir(&self) -> PathBuf {
        self.dir.path().join("artifacts/hot")
    }
}

fn percentile(sorted: &[Duration], percent: usize) -> Duration {
    let rank = (sorted.len() * percent).div_ceil(100);
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

fn report(label: &str, samples: &mut [Duration], budget: Duration) -> bool {
    samples.sort_unstable();
    let p50 = percentile(samples, 50);
    let p95 = percentile(samples, 95);
    let max = *samples.last().expect("samples");
    let ok = p95 <= budget;
    println!(
        "{label}: n={} p50={} p95={} max={} 预算={} {}",
        samples.len(),
        ms(p50),
        ms(p95),
        ms(max),
        ms(budget),
        if ok { "PASS" } else { "FAIL" }
    );
    ok
}

fn ms(duration: Duration) -> String {
    format!("{:.1}ms", duration.as_secs_f64() * 1000.0)
}

fn main() {
    let bed = Bed::new();

    // 冷：每次换一份新拷贝；预热同样换新拷贝，避免把「首次执行」留在样本里。
    let cold_dir = bed.cold_dir();
    for _ in 0..WARMUP {
        install_fresh(&cold_dir);
        let _ = bed.timed_start(&cold_dir);
    }
    let mut cold = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        install_fresh(&cold_dir);
        cold.push(bed.timed_start(&cold_dir));
    }

    // 热：同一份产物目录反复起停，先预热一次执行。
    let hot_dir = bed.hot_dir();
    std::fs::create_dir_all(hot_dir.parent().expect("parent")).expect("create artifacts dir");
    install_fresh(&hot_dir);
    for _ in 0..WARMUP {
        let _ = bed.timed_start(&hot_dir);
    }
    let mut hot = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        hot.push(bed.timed_start(&hot_dir));
    }

    bed.host.shutdown_all(Duration::from_secs(1));

    let cold_ok = report("BENCH-001 冷启动握手", &mut cold, COLD_BUDGET);
    let hot_ok = report("BENCH-002 热启动握手", &mut hot, HOT_BUDGET);
    if !cold_ok || !hot_ok {
        std::process::exit(1);
    }
}
