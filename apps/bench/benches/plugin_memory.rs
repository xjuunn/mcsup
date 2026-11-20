//! 宿主空闲内存（`BENCH-005`）
//!
//! 内存必须在**独立进程**里量：benchmark 进程本身要装 criterion / 样本与
//! 临时目录，混在一起量到的不是宿主。所以这个基准把自己当作宿主进程再拉一次
//! （`--child`），构造 `PluginHost` 后停在空闲状态，父进程用 `ps` 采样 RSS。
//!
//! 量的是宿主库进程（`mcsup` + `plugin` + `storage` + 样本记录），不含 CLI /
//! TUI 自身的常驻内存：那部分与插件无关，且只在 `mcsup __plugin-host` 里出现。
//!
//! 两次采样：
//!
//! - 空宿主：没有安装记录，纯空闲基线；
//! - 100 个非 resident 插件：只有记录，没有任何会话进程常驻（顺带验证
//!   `BENCH-006` 的「无进程常驻」）。
//!
//! 跑法：`cargo bench -p bench --bench plugin_memory`

// 基准夹具构造失败必须立刻可见（同 `hot_paths.rs` 的豁免说明）。
#![allow(clippy::expect_used, reason = "基准样本构造失败必须立刻中止")]
// 报告本身就走 stdout：这个基准没有 criterion，输出即结果。
#![allow(clippy::print_stdout, reason = "基准报告输出")]
// RSS 是字节数，换算 MiB 只用于展示阈值比较。
#![allow(clippy::cast_precision_loss, reason = "RSS 换算 MiB")]

use mcsup::domain::plugin::manifest::Manifest;
use mcsup::domain::plugin::state::PluginState;
use mcsup::domain::plugin::{PLUGIN_API_VERSION, PluginId, PluginScope};
use mcsup::plugin::api::HostServices;
use mcsup::plugin::event::EventBus;
use mcsup::plugin::host::PluginHost;
use mcsup::port::RepositoryError;
use mcsup::port::plugin::{
    NoopAuditSink, PluginConfigStore, PluginRecord, PluginStore, StoredConfig,
};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write as _};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// `BENCH-005` 宿主空闲内存上限
const BUDGET_MIB: f64 = 40.0;

/// 父 / 子进程共用：子进程构造宿主后停在空闲状态
const CHILD_FLAG: &str = "--bench-host-child";

/// 子进程就绪标记
const READY: &str = "bench-host-ready";

const MIB: f64 = 1024.0 * 1024.0;

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
        "name": "Idle metadata",
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
        enabled: false,
        approved: BTreeSet::default(),
        manifest: raw,
        artifact_dir,
        installed_at: 0,
        updated_at: 0,
        last_state: PluginState::Disabled,
        last_error: None,
        previous_version: None,
        source: None,
        sha256: None,
        blocked: false,
        quarantined: false,
    }
}

/// 子进程：构造宿主，停在空闲状态等父进程采样
fn child(records: usize) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(MemoryStore::default());
    for index in 0..records {
        let id = PluginId::parse(&format!("idle{index}")).expect("id");
        let record = record(&id, dir.path().join(format!("artifacts/idle{index}")));
        store.put(&record).expect("put record");
    }
    let host = PluginHost::new(
        dir.path().join("data"),
        Arc::clone(&store) as Arc<dyn PluginStore>,
        Arc::new(NoConfig),
        &Arc::new(HostServices::default()),
        EventBus::new(),
        Arc::new(NoopAuditSink),
        "bench-host",
    );
    assert_eq!(host.session_count(), 0, "idle host must not run a session");
    println!("{READY} {}", host.session_count());
    std::io::stdout().flush().expect("flush ready");

    // 一直空闲到父进程采样完并 kill。这里不 spawn 空闲回收线程：它本身会多
    // 一条线程与定时器，都不是「宿主按需运行」的常驻部分。
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// 采样 RSS（单位：字节）
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn rss_bytes(pid: u32) -> u64 {
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .expect("run ps");
    let kb: u64 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .expect("parse rss");
    kb * 1024
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn rss_bytes(_pid: u32) -> u64 {
    0
}

/// 父进程：拉起一个空闲宿主子进程，等它就绪后采样
fn sample(records: usize) -> Option<f64> {
    let exe = std::env::current_exe().expect("current exe");
    let mut child = Command::new(exe)
        .arg(CHILD_FLAG)
        .arg(records.to_string())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn host child");
    let stdout = child.stdout.take().expect("stdout");
    let mut lines = BufReader::new(stdout).lines();
    let ready = lines.next().expect("ready line").expect("read ready");
    assert!(ready.starts_with(READY), "unexpected child output: {ready}");
    let pid = child.id();
    // 子进程进入空闲循环；RSS 在构造完成后已经稳定，短暂等待只为让迭代器与
    // 线程栈落到稳定值。
    std::thread::sleep(Duration::from_millis(200));
    let rss = rss_bytes(pid);
    let _ = child.kill();
    let _ = child.wait();
    (rss > 0).then(|| rss as f64 / MIB)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(index) = args.iter().position(|arg| arg == CHILD_FLAG) {
        let records: usize = args
            .get(index + 1)
            .and_then(|raw| raw.parse().ok())
            .unwrap_or(0);
        child(records);
        return;
    }

    if cfg!(not(any(target_os = "macos", target_os = "linux"))) {
        println!("BENCH-005 宿主空闲内存: 跳过（当前平台没有 RSS 采样实现）");
        return;
    }

    let empty = sample(0);
    let populated = sample(100);
    let mut failed = false;
    for (label, value) in [("空宿主", empty), ("100 个非 resident 插件", populated)] {
        if let Some(mib) = value {
            let ok = mib <= BUDGET_MIB;
            println!(
                "BENCH-005 {label}: {mib:.1} MiB 预算={BUDGET_MIB:.0} MiB {}",
                if ok { "PASS" } else { "FAIL" }
            );
            failed |= !ok;
        } else {
            println!("BENCH-005 {label}: 采样失败");
            failed = true;
        }
    }
    if failed {
        std::process::exit(1);
    }
}
