//! 插件宿主：进程生命周期、会话表、崩溃退避与空闲回收
//!
//! 本模块只回答「进程现在跑没跑、怎么拉起来、怎么停下来」。状态迁移的合法性、
//! 能力交集与事件订阅合法性都在 `domain::plugin`；能力调用在 [`crate::api`]。
//! `[禁止]` 在 CLI 或控制端点里重写一份启动判定。
//!
//! # 实例唯一
//!
//! 每个 `(plugin_id, scope, server)` 至多一个会话。会话表是唯一真相源，
//! 安装记录里的 `last_state` 只在宿主不在时用于离线展示。

use crate::api::HostServices;
use crate::endpoint::Endpoint;
use crate::event::EventBus;
use crate::process::{SpawnPlan, SpawnedPlugin};
use crate::rfc3339;
use crate::session::{self, HandshakeOutcome, SessionConfig, SessionInner};
use domain::plugin::PluginId;
use domain::plugin::capability::Capability;
use domain::plugin::event::Subscription;
use domain::plugin::state::{PluginEvent, PluginState};
use port::plugin::{
    AuditEvent, AuditResult, AuditSink, HostError, PluginConfigStore, PluginRecord,
    PluginRuntimeStatus, PluginStore, PluginSupervisor, SessionInfo, StreamSink,
};
use protocol::plugin::{ConfigValues, WelcomeLimits};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

/// 握手超时：插件必须在 10 s 内连接并发出 `hello`
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// `reject` 后等待插件自行退出的宽限
pub const REJECT_GRACE: Duration = Duration::from_secs(5);

/// 非 resident 插件空闲多久后停止进程
pub const IDLE_STOP_AFTER: Duration = Duration::from_secs(120);

/// 退避序列（秒）
const BACKOFF_SECS: [u64; 5] = [1, 2, 4, 8, 16];

/// 崩溃统计窗口
const CRASH_WINDOW: Duration = Duration::from_secs(600);

/// 窗口内第几次崩溃后停止自动重启
const MAX_CRASHES: usize = 5;

struct Crashes {
    at: Vec<Instant>,
}

impl Crashes {
    fn record(&mut self) -> usize {
        let now = Instant::now();
        self.at.push(now);
        self.at.retain(|at| now.duration_since(*at) <= CRASH_WINDOW);
        self.at.len()
    }

    fn clear(&mut self) {
        self.at.clear();
    }
}

/// 插件宿主
pub struct PluginHost {
    data_dir: PathBuf,
    run_dir: PathBuf,
    log_dir: PathBuf,
    store: Arc<dyn PluginStore>,
    config_store: Arc<dyn PluginConfigStore>,
    services: Arc<HostServices>,
    bus: EventBus,
    audit: Arc<dyn AuditSink>,
    host_version: String,
    sessions: Mutex<HashMap<PluginId, Arc<SessionInner>>>,
    starting: Mutex<HashSet<PluginId>>,
    crashes: Mutex<HashMap<PluginId, Crashes>>,
    shutting_down: AtomicBool,
    idle_after: Duration,
    /// 自引用弱指针：后台监管线程需要拥有宿主
    self_ref: OnceLock<Weak<PluginHost>>,
}

impl std::fmt::Debug for PluginHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sessions = self
            .sessions
            .lock()
            .map(|map| map.len())
            .unwrap_or_default();
        f.debug_struct("PluginHost")
            .field("data_dir", &self.data_dir)
            .field("sessions", &sessions)
            .field("shutting_down", &self.shutting_down.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl PluginHost {
    /// 构造宿主（不启动后台线程）
    #[must_use]
    pub fn new(
        data_dir: PathBuf,
        store: Arc<dyn PluginStore>,
        config_store: Arc<dyn PluginConfigStore>,
        services: Arc<HostServices>,
        bus: EventBus,
        audit: Arc<dyn AuditSink>,
        host_version: impl Into<String>,
    ) -> Arc<Self> {
        let run_dir = data_dir.join("run");
        let log_dir = data_dir.join("logs").join("plugins");
        let host = Arc::new(Self {
            data_dir,
            run_dir,
            log_dir,
            store,
            config_store,
            services,
            bus,
            audit,
            host_version: host_version.into(),
            sessions: Mutex::new(HashMap::new()),
            starting: Mutex::new(HashSet::new()),
            crashes: Mutex::new(HashMap::new()),
            shutting_down: AtomicBool::new(false),
            idle_after: IDLE_STOP_AFTER,
            self_ref: OnceLock::new(),
        });
        let _ = host.self_ref.set(Arc::downgrade(&host));
        host
    }

    /// 覆盖空闲停止阈值（测试与 `--idle-timeout` 用）
    ///
    /// 只能在宿主尚未被共享时调用（`PluginHost::new` 之后立即调用）。
    #[must_use]
    pub fn with_idle_after(mut self: Arc<Self>, idle_after: Duration) -> Arc<Self> {
        if let Some(inner) = Arc::get_mut(&mut self) {
            inner.idle_after = idle_after;
        }
        self
    }

    /// 启动空闲回收线程
    ///
    /// 线程只持弱引用：宿主被丢弃后自行退出。
    pub fn spawn_idle_reaper(self: &Arc<Self>) {
        let weak: Weak<Self> = Arc::downgrade(self);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(5));
                let Some(host) = weak.upgrade() else {
                    return;
                };
                host.reap_idle();
                if host.shutting_down.load(Ordering::SeqCst) {
                    return;
                }
            }
        });
    }

    /// 停止空闲的非 resident 会话
    pub fn reap_idle(&self) {
        let idle = self
            .sessions
            .lock()
            .map(|map| {
                map.values()
                    .filter(|session| !session.record.manifest.resident)
                    .filter(|session| session.idle_for() >= self.idle_after)
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for session in idle {
            self.audit.record(&AuditEvent {
                at: now_secs(),
                plugin: session.record.id.to_string(),
                action: "lifecycle:idle_stop".to_owned(),
                capability: None,
                result: AuditResult::Allowed,
                detail: Some(format!("idle for {} s", self.idle_after.as_secs())),
            });
            session.shutdown(Duration::from_secs(5));
            self.forget_session(&session.record.id);
            record_state(
                self.store.as_ref(),
                &session.record.id,
                PluginState::Stopped,
                None,
            );
        }
    }

    /// 停止全部会话（宿主退出）
    pub fn shutdown_all(&self, grace: Duration) {
        self.shutting_down.store(true, Ordering::SeqCst);
        let sessions = self
            .sessions
            .lock()
            .map(|mut map| std::mem::take(&mut *map).into_values().collect::<Vec<_>>())
            .unwrap_or_default();
        for session in sessions {
            session.shutdown(grace);
            self.bus.unsubscribe(&session.record.id);
            record_state(
                self.store.as_ref(),
                &session.record.id,
                PluginState::Stopped,
                None,
            );
        }
    }

    /// 实时会话数（测试与指标）
    #[must_use]
    pub fn session_count(&self) -> usize {
        self.sessions.lock().map_or(0, |map| map.len())
    }

    /// 事件总线
    #[must_use]
    pub fn bus(&self) -> &EventBus {
        &self.bus
    }

    /// 数据目录
    #[must_use]
    pub fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    fn forget_session(&self, id: &PluginId) {
        if let Ok(mut map) = self.sessions.lock()
            && map.get(id).is_some_and(|session| !session.is_alive())
        {
            map.remove(id);
        }
        self.bus.unsubscribe(id);
    }

    fn session_of(&self, id: &PluginId) -> Option<Arc<SessionInner>> {
        self.sessions
            .lock()
            .ok()
            .and_then(|map| map.get(id).cloned())
            .filter(|session| session.is_alive())
    }

    fn ensure_session(&self, id: &PluginId) -> Result<Arc<SessionInner>, HostError> {
        if let Some(session) = self.session_of(id) {
            return Ok(session);
        }
        self.start_session(id)
    }

    #[allow(clippy::too_many_lines)] // 取记录、建端点、拉进程、握手是一段不可拆的序列
    fn start_session(&self, id: &PluginId) -> Result<Arc<SessionInner>, HostError> {
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(HostError::new(
                port::plugin::ErrorCode::Unavailable,
                "plugin host is shutting down",
            ));
        }
        {
            let mut starting = self
                .starting
                .lock()
                .map_err(|_| HostError::internal("starting set poisoned"))?;
            if !starting.insert(id.clone()) {
                return Err(HostError::conflict(format!(
                    "plugin {id} is already starting"
                )));
            }
        }
        let guard = StartingGuard {
            host: self,
            id: id.clone(),
        };
        let record = self.record(id)?;
        if record.blocked {
            return Err(HostError::denied(format!(
                "plugin {id} is blocked; run `mcsup plugin unblock {id}` first"
            )));
        }
        if record.quarantined {
            return Err(HostError::denied(format!(
                "plugin {id} is quarantined; run `mcsup plugin release {id}` first"
            )));
        }
        if !record.enabled {
            return Err(HostError::conflict(format!(
                "plugin {id} is not enabled; run `mcsup plugin enable {id}`"
            )));
        }
        // 判定收敛在领域层：从离线状态出发，`StartRequested` 只允许
        // `stopped` / `failed`。`pending_approval` 与 `disabled` 会被拒绝。
        let from = offline_state(&record);
        from.transition(PluginEvent::StartRequested)
            .map_err(|_| HostError::conflict(format!("plugin {id} is {from}, cannot start")))?;

        let endpoint = Endpoint::bind(&self.run_dir, id, record.scope, record.server.as_ref())
            .map_err(|err| {
                HostError::new(
                    port::plugin::ErrorCode::Crashed,
                    format!("bind endpoint for {id}: {err}"),
                )
            })?;
        let token = new_token();
        let log_file = self.log_dir.join(format!("{id}.log"));
        let plan = SpawnPlan::new(
            &record.artifact_dir,
            &record.manifest.entry,
            log_file,
            id.to_string(),
            &endpoint.address(),
            &token,
            id.as_str(),
            record.scope,
            record
                .server
                .as_ref()
                .map(domain::server::meta::ServerName::as_str),
        )?;
        let mut process = SpawnedPlugin::spawn(&plan)?;
        let pid = process.pid();
        let conn = match endpoint.accept(HANDSHAKE_TIMEOUT) {
            Ok(conn) => conn,
            Err(err) => {
                process.kill();
                let reason = format!("handshake_timeout: {err}");
                record_state(self.store.as_ref(), id, PluginState::Failed, Some(&reason));
                return Err(HostError::new(port::plugin::ErrorCode::Crashed, reason));
            }
        };
        let config = SessionConfig {
            record: record.clone(),
            token,
            host_version: self.host_version.clone(),
            limits: WelcomeLimits::default(),
            config: self.welcome_config(id),
            services: Arc::clone(&self.services),
            audit: Arc::clone(&self.audit),
            handshake_timeout: HANDSHAKE_TIMEOUT,
            pid,
        };
        let outcome = session::handshake(conn, config);
        drop(endpoint);
        match outcome {
            Ok(HandshakeOutcome::Accepted(session)) => {
                session.attach_process(process);
                session.start();
                {
                    let mut map = self
                        .sessions
                        .lock()
                        .map_err(|_| HostError::internal("session table poisoned"))?;
                    map.insert(id.clone(), Arc::clone(&session));
                }
                self.subscribe(&session);
                if let Ok(mut crashes) = self.crashes.lock() {
                    crashes
                        .entry(id.clone())
                        .or_insert(Crashes { at: Vec::new() })
                        .clear();
                }
                record_state(self.store.as_ref(), id, PluginState::Running, None);
                self.audit.record(&AuditEvent {
                    at: now_secs(),
                    plugin: id.to_string(),
                    action: "lifecycle:start".to_owned(),
                    capability: None,
                    result: AuditResult::Allowed,
                    detail: Some(format!("pid {pid}, session {}", session.session_id())),
                });
                self.spawn_supervisor(id.clone(), Arc::downgrade(&session));
                drop(guard);
                Ok(session)
            }
            Ok(HandshakeOutcome::Rejected(reason)) => {
                // `reject` 已写出；等插件自行退出，超时强杀。
                if !process.wait_timeout(REJECT_GRACE) {
                    process.kill();
                }
                self.audit.record(&AuditEvent {
                    at: now_secs(),
                    plugin: id.to_string(),
                    action: "lifecycle:reject".to_owned(),
                    capability: None,
                    result: AuditResult::Denied,
                    detail: Some(reason.clone()),
                });
                record_state(self.store.as_ref(), id, PluginState::Failed, Some(&reason));
                drop(guard);
                Err(HostError::new(port::plugin::ErrorCode::Crashed, reason))
            }
            Err(err) => {
                process.kill();
                record_state(
                    self.store.as_ref(),
                    id,
                    PluginState::Failed,
                    Some(&err.message),
                );
                drop(guard);
                Err(err)
            }
        }
    }

    fn subscribe(&self, session: &Arc<SessionInner>) {
        let subscriptions: BTreeSet<Subscription> = session.subscriptions().clone();
        if subscriptions.is_empty() {
            return;
        }
        let target: Arc<dyn crate::event::EventDelivery> =
            Arc::clone(session) as Arc<dyn crate::event::EventDelivery>;
        self.bus
            .subscribe(session.record.id.clone(), subscriptions, target);
    }

    fn spawn_supervisor(&self, id: PluginId, session: Weak<SessionInner>) {
        let Some(host) = self.self_ref.get().and_then(Weak::upgrade) else {
            return;
        };
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(250));
                let Some(session) = session.upgrade() else {
                    return;
                };
                if session.is_alive() {
                    continue;
                }
                drop(session);
                host.on_session_exit(&id);
                return;
            }
        });
    }

    fn on_session_exit(&self, id: &PluginId) {
        let stopped_by_user = {
            let Ok(mut map) = self.sessions.lock() else {
                return;
            };
            match map.remove(id) {
                Some(session) => session.is_stopping(),
                None => false,
            }
        };
        self.bus.unsubscribe(id);
        if self.shutting_down.load(Ordering::SeqCst) {
            record_state(self.store.as_ref(), id, PluginState::Stopped, None);
            return;
        }
        if stopped_by_user {
            record_state(self.store.as_ref(), id, PluginState::Stopped, None);
            return;
        }
        let attempts = match self.crashes.lock() {
            Ok(mut map) => map
                .entry(id.clone())
                .or_insert(Crashes { at: Vec::new() })
                .record(),
            Err(_) => MAX_CRASHES,
        };
        let Ok(record) = self.record(id) else {
            return;
        };
        if !record.enabled {
            record_state(self.store.as_ref(), id, PluginState::Disabled, None);
            return;
        }
        if attempts >= MAX_CRASHES {
            let reason = format!(
                "crashed {attempts} times within {} s; automatic restart disabled",
                CRASH_WINDOW.as_secs()
            );
            record_state(self.store.as_ref(), id, PluginState::Failed, Some(&reason));
            self.audit.record(&AuditEvent {
                at: now_secs(),
                plugin: id.to_string(),
                action: "lifecycle:crash_limit".to_owned(),
                capability: None,
                result: AuditResult::Failed,
                detail: Some(reason),
            });
            return;
        }
        let delay = Duration::from_secs(BACKOFF_SECS[(attempts - 1).min(BACKOFF_SECS.len() - 1)]);
        record_state(
            self.store.as_ref(),
            id,
            PluginState::Starting,
            Some(&format!("restarting in {} s", delay.as_secs())),
        );
        self.audit.record(&AuditEvent {
            at: now_secs(),
            plugin: id.to_string(),
            action: "lifecycle:restart".to_owned(),
            capability: None,
            result: AuditResult::Failed,
            detail: Some(format!("crash #{attempts}, backoff {} s", delay.as_secs())),
        });
        std::thread::sleep(delay);
        if self.shutting_down.load(Ordering::SeqCst) {
            return;
        }
        // 重启失败交给下一次 `start`，不在这里递归重试：超过上限时状态已是
        // `failed`，自动重启必须停止。
        if let Err(err) = self.start_session(id) {
            let reason = format!("automatic restart failed: {}", err.message);
            record_state(self.store.as_ref(), id, PluginState::Failed, Some(&reason));
        }
    }

    fn record(&self, id: &PluginId) -> Result<PluginRecord, HostError> {
        self.store.get(id).map_err(|err| match err {
            port::RepositoryError::NotFound(message) => {
                HostError::not_found(format!("plugin {id} not found: {message}"))
            }
            other => HostError::internal(format!("read plugin {id}: {other}")),
        })
    }

    /// 握手时下发的配置：明文，插件需要用它工作
    fn welcome_config(&self, id: &PluginId) -> ConfigValues {
        match self.config_store.get(id) {
            Ok(Some(stored)) => ConfigValues {
                revision: stored.revision,
                values: stored.values,
            },
            Ok(None) => ConfigValues::default(),
            Err(err) => {
                // 读不出来不能阻止启动：插件收到空配置，记录里留下线索。
                self.audit.record(&AuditEvent {
                    at: now_secs(),
                    plugin: id.to_string(),
                    action: "config:read".to_owned(),
                    capability: None,
                    result: AuditResult::Failed,
                    detail: Some(format!("{err}")),
                });
                ConfigValues::default()
            }
        }
    }
}

struct StartingGuard<'a> {
    host: &'a PluginHost,
    id: PluginId,
}

impl Drop for StartingGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut starting) = self.host.starting.lock() {
            starting.remove(&self.id);
        }
    }
}

impl PluginSupervisor for PluginHost {
    fn start(&self, id: &PluginId) -> Result<SessionInfo, HostError> {
        let session = self.ensure_session(id)?;
        Ok(session.info.clone())
    }

    fn stop(&self, id: &PluginId, grace: Duration) -> Result<(), HostError> {
        let Some(session) = self.session_of(id) else {
            return Err(HostError::conflict(format!("plugin {id} is not running")));
        };
        session.shutdown(grace);
        self.forget_session(id);
        record_state(self.store.as_ref(), id, PluginState::Stopped, None);
        self.audit.record(&AuditEvent {
            at: now_secs(),
            plugin: id.to_string(),
            action: "lifecycle:stop".to_owned(),
            capability: None,
            result: AuditResult::Allowed,
            detail: Some(format!("grace {} s", grace.as_secs())),
        });
        Ok(())
    }

    fn status(&self, id: &PluginId) -> Result<PluginRuntimeStatus, HostError> {
        let record = self.record(id)?;
        let session = self.session_of(id);
        let state = if let Some(session) = &session {
            if session.is_stopping() {
                PluginState::Stopping
            } else {
                PluginState::Running
            }
        } else {
            offline_state(&record)
        };
        Ok(PluginRuntimeStatus {
            id: id.clone(),
            state,
            session: session.as_ref().map(|session| {
                let mut info = session.info.clone();
                info.granted = session.grants();
                info
            }),
            last_error: record.last_error,
        })
    }

    fn call(&self, id: &PluginId, op: &str, input: Value) -> Result<Value, HostError> {
        let session = self.ensure_session(id)?;
        session.call(op, input, session::default_timeout())
    }

    fn stream(
        &self,
        id: &PluginId,
        op: &str,
        input: Value,
        sink: &mut dyn StreamSink,
    ) -> Result<(), HostError> {
        let session = self.ensure_session(id)?;
        session.stream(op, input, session::default_timeout(), sink)
    }

    fn reload(&self) -> Result<(), HostError> {
        let records = self
            .store
            .list()
            .map_err(|err| HostError::internal(format!("reload plugin records: {err}")))?;
        let known: BTreeSet<&PluginId> = records.iter().map(|record| &record.id).collect();
        let stale = self
            .sessions
            .lock()
            .map(|map| {
                map.iter()
                    .filter(|(id, session)| !known.contains(*id) || !session.is_alive())
                    .map(|(id, _)| id.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for id in stale {
            self.forget_session(&id);
        }
        Ok(())
    }

    fn revoke(&self, id: &PluginId, capability: Capability) -> Result<(), HostError> {
        if let Some(session) = self.session_of(id) {
            session.revoke_grant(capability);
        }
        Ok(())
    }

    fn apply_config(&self, id: &PluginId, revision: u64, values: &Value) -> Result<(), HostError> {
        if let Some(session) = self.session_of(id) {
            return session.apply_config(revision, values);
        }
        // 会话不在：配置已落盘，下次握手经 `welcome.config` 下发。
        Ok(())
    }
}

/// 离线状态：记录里没有实时会话时用于展示
fn offline_state(record: &PluginRecord) -> PluginState {
    if !record.enabled {
        return match record.last_state {
            PluginState::PendingApproval => PluginState::PendingApproval,
            PluginState::Failed => PluginState::Failed,
            _ => PluginState::Disabled,
        };
    }
    match record.last_state {
        PluginState::Running
        | PluginState::Starting
        | PluginState::Stopping
        | PluginState::Degraded => PluginState::Stopped,
        other => other,
    }
}

fn record_state(store: &dyn PluginStore, id: &PluginId, state: PluginState, reason: Option<&str>) {
    let Ok(mut record) = store.get(id) else {
        return;
    };
    record.last_state = state;
    record.last_error = reason.map(ToOwned::to_owned);
    record.updated_at = now_secs();
    let _ = store.put(&record);
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}

/// 一次性 32 字节令牌（64 hex）
pub(crate) fn new_token() -> String {
    let mut bytes = Vec::with_capacity(32);
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    let mut token = String::with_capacity(64);
    for byte in bytes {
        token.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        token.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    token
}

/// 当前时间文本，供宿主日志使用
#[must_use]
pub fn now_text() -> String {
    rfc3339::now()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_has_wire_shape() {
        let token = new_token();
        assert_eq!(token.len(), 64);
        assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn offline_state_never_reports_running() {
        let mut record = sample_record();
        record.last_state = PluginState::Running;
        assert_eq!(offline_state(&record), PluginState::Stopped);
        record.enabled = false;
        assert_eq!(offline_state(&record), PluginState::Disabled);
        record.last_state = PluginState::PendingApproval;
        assert_eq!(offline_state(&record), PluginState::PendingApproval);
    }

    fn sample_record() -> PluginRecord {
        use domain::plugin::PluginScope;
        use domain::plugin::manifest::Manifest;
        use std::collections::BTreeSet;
        PluginRecord {
            id: PluginId::parse("demo").expect("id"),
            version: "0.1.0".to_owned(),
            api: 1,
            scope: PluginScope::Machine,
            server: None,
            enabled: true,
            approved: BTreeSet::new(),
            manifest: Manifest::from_json(
                br#"{"schema":1,"id":"demo","name":"Demo","version":"0.1.0","api":1,
                     "entry":{"type":"process","cmd":"bin/demo"},"capabilities":["events"],
                     "license":"MIT"}"#,
            )
            .expect("manifest")
            .raw,
            artifact_dir: PathBuf::from("/tmp"),
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
}
