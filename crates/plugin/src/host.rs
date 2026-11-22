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
use crate::session::{self, HandshakeOutcome, Heartbeat, SessionConfig, SessionInner};
use domain::plugin::PluginId;
use domain::plugin::capability::Capability;
use domain::plugin::event::Subscription;
use domain::plugin::state::{PluginEvent, PluginState};
use port::plugin::{
    AuditEvent, AuditResult, AuditSink, HostError, PluginConfigStore, PluginRecord,
    PluginRevocations, PluginRuntimeStatus, PluginStore, PluginSupervisor, SessionInfo, StreamSink,
};
use protocol::plugin::{ConfigValues, WelcomeLimits};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

/// 握手超时：插件必须在 10 s 内连接并发出 `hello`
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// `reject` 后等待插件自行退出的宽限
pub const REJECT_GRACE: Duration = Duration::from_secs(5);

/// 非 resident 插件空闲多久后停止进程
pub const IDLE_STOP_AFTER: Duration = Duration::from_secs(120);

/// 旧会话退出后等待监管线程收尾的上限
const SPAWN_SETTLE: Duration = Duration::from_secs(2);

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
    /// 正在退避等待自动重启的插件
    ///
    /// `stop` 与手动 `start` 会把它摘掉，退避线程醒来看到「已不在表里」就
    /// 放弃重启；否则一次用户停机之后插件还会被旧线程悄悄拉起。
    pending_restarts: Mutex<HashSet<PluginId>>,
    /// 撤销列表；`None` 表示本次装配没有接入（测试与内嵌场景）
    revocations: Mutex<Option<Arc<dyn PluginRevocations>>>,
    shutting_down: AtomicBool,
    /// 空闲停止阈值；同 [`Self::heartbeat`]，只能经锁改写
    idle_after: Mutex<Duration>,
    /// 心跳节奏；`new` 里已经把 `Arc<Self>` 交给契约路由，`Arc::get_mut`
    /// 拿不到独占引用，只能经由锁改写
    heartbeat: Mutex<Heartbeat>,
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
        services: &Arc<HostServices>,
        bus: EventBus,
        audit: Arc<dyn AuditSink>,
        host_version: impl Into<String>,
    ) -> Arc<Self> {
        let run_dir = data_dir.join("run");
        let log_dir = data_dir.join("logs").join("plugins");
        // 契约路由依赖会话表，构造顺序是「先建宿主、再回填弱指针」。
        // 会话拿到的是补齐了 `contracts` 的副本，调用方持有的 Arc 不受影响。
        let router = Arc::new(crate::contract::ContractRouter::new());
        let services = Arc::new(HostServices {
            contracts: Some(Arc::clone(&router) as Arc<dyn port::plugin::ContractInvoker>),
            ..(**services).clone()
        });
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
            pending_restarts: Mutex::new(HashSet::new()),
            revocations: Mutex::new(None),
            shutting_down: AtomicBool::new(false),
            idle_after: Mutex::new(IDLE_STOP_AFTER),
            heartbeat: Mutex::new(Heartbeat::default()),
            self_ref: OnceLock::new(),
        });
        router.attach(&host);
        let _ = host.self_ref.set(Arc::downgrade(&host));
        host
    }

    /// 覆盖心跳节奏（测试用）
    ///
    /// `PluginHost::new` 返回前已留下两个 `Weak` 自引用（`self_ref` 与契约
    /// 路由），`Arc::get_mut` 因此永远返回 `None`，早期实现会静默丢弃覆盖
    /// 值。这里改为经锁写入，任何时刻调用都生效。
    #[must_use]
    pub fn with_heartbeat(self: Arc<Self>, heartbeat: Heartbeat) -> Arc<Self> {
        {
            let mut slot = self
                .heartbeat
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *slot = heartbeat;
        }
        self
    }

    /// 当前心跳节奏
    fn heartbeat(&self) -> Heartbeat {
        *self
            .heartbeat
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 接入撤销列表（组合根注入）
    ///
    /// 启动前查询，命中即拒绝；列表本身读不出来只记审计、不阻断已安装
    /// 插件（`docs/plugins.md` §16.4）。与 [`Self::with_heartbeat`] 同理，
    /// 经锁写入以避开 `Arc` 的共享限制。
    #[must_use]
    pub fn with_revocations(self: Arc<Self>, revocations: Arc<dyn PluginRevocations>) -> Arc<Self> {
        {
            let mut slot = self
                .revocations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *slot = Some(revocations);
        }
        self
    }

    /// 心跳节奏（测试断言覆盖值是否生效）
    #[must_use]
    pub fn heartbeat_for_test(&self) -> Heartbeat {
        self.heartbeat()
    }

    /// 覆盖空闲停止阈值（测试与 `--idle-timeout` 用）
    ///
    /// 与 [`Self::with_heartbeat`] 同理，必须经锁写入。
    #[must_use]
    pub fn with_idle_after(self: Arc<Self>, idle_after: Duration) -> Arc<Self> {
        {
            let mut slot = self
                .idle_after
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *slot = idle_after;
        }
        self
    }

    /// 当前空闲停止阈值
    fn idle_after(&self) -> Duration {
        *self
            .idle_after
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
        // 锁只取一次：闭包里逐元素加锁会被会话数量放大
        let idle_after = self.idle_after();
        let idle = self
            .sessions
            .lock()
            .map(|map| {
                map.values()
                    .filter(|session| !session.record.manifest.resident)
                    .filter(|session| session.idle_for() >= idle_after)
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
                detail: Some(format!("idle for {} s", idle_after.as_secs())),
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

    /// 能力集合
    ///
    /// 控制台跟随线程需要 `server:read` 与 `console`，两者都由门面注入。
    #[must_use]
    pub fn services(&self) -> Option<&Arc<HostServices>> {
        Some(&self.services)
    }

    /// 是否已进入关停流程
    #[must_use]
    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    /// 启动控制台行 → 事件的后台线程
    ///
    /// 与 [`Self::spawn_idle_reaper`] 一样只由常驻宿主进程调用；线程持有弱
    /// 引用，宿主丢弃或关停时自行退出。
    pub fn spawn_console_events(self: &Arc<Self>) {
        crate::console_events::spawn(self);
    }

    /// 数据目录
    #[must_use]
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// 读取安装记录（契约路由用）
    pub(crate) fn record_of(&self, id: &PluginId) -> Result<PluginRecord, HostError> {
        self.record(id)
    }

    /// 全部安装记录（契约路由选提供方用）
    pub(crate) fn records(&self) -> Result<Vec<PluginRecord>, HostError> {
        self.store
            .list()
            .map_err(|err| HostError::internal(format!("list plugin records: {err}")))
    }

    /// 调用提供方 op，使用契约自己的超时
    pub(crate) fn call_plugin(
        &self,
        id: &PluginId,
        op: &str,
        input: Value,
        timeout: Duration,
    ) -> Result<Value, HostError> {
        let session = self.ensure_session(id)?;
        session.call(op, input, timeout)
    }

    /// 审计出口（契约路由记录 `plugin.contract`）
    pub(crate) fn audit(&self) -> &Arc<dyn AuditSink> {
        &self.audit
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

    /// 等监管线程把已退出的会话收走
    ///
    /// `stop` 依赖监管线程做崩溃/停机的分流，退出后要给它一点时间从会话表
    /// 摘除；否则紧接着的 `start` 会撞上仍在表里的旧会话。
    fn await_exit(&self, id: &PluginId, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let present = self.sessions.lock().is_ok_and(|map| map.contains_key(id));
            if !present {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn ensure_session(&self, id: &PluginId) -> Result<Arc<SessionInner>, HostError> {
        if let Some(session) = self.session_of(id) {
            return Ok(session);
        }
        self.start_session(id)
    }

    #[allow(clippy::too_many_lines)] // 取记录、建端点、拉进程、握手是一段不可拆的序列
    fn start_session(&self, id: &PluginId) -> Result<Arc<SessionInner>, HostError> {
        // 先看会话表：崩溃退避线程可能和手动 `start` 同时走到这里，
        // 只有这一处检查能在拉起第二个进程之前拦住它。
        if let Some(session) = self.session_of(id) {
            return Ok(session);
        }
        // 手动 `start` 接管时取消退避期的自动重启：退避线程醒来看到自己
        // 已不在表里就直接退出，不会在已有会话之外再拉一个进程。
        if let Ok(mut pending) = self.pending_restarts.lock() {
            pending.remove(id);
        }
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
                // 上一个会话刚退出、监管线程尚未收走时也会走到这里。等它收尾
                // 再判一次，避免把「重启刚完成」误报成并发启动。
                drop(starting);
                self.await_exit(id, SPAWN_SETTLE);
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
        self.reject_if_revoked(&record)?;
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
        let mut plan = SpawnPlan::new(
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
        // 已知密钥值进入日志打码表：插件把凭据打到 stdout / stderr 时按精确值
        // 替换。读不到密钥不该阻止启动——打码是加固，不是启动前置条件；插件
        // 真要用密钥时会在 `secret:rw` 上拿到同一个错误。
        if let Some(secrets) = &self.services.secrets
            && let Ok(values) = secrets.values(id)
        {
            plan.extend_redact(values);
        }
        let mut process = SpawnedPlugin::spawn(&plan)?;
        let pid = process.pid();
        let conn = match endpoint.accept(HANDSHAKE_TIMEOUT) {
            Ok(conn) => conn,
            Err(err) => {
                // 先看进程是否已经自己退出。握手超时的根因经常是插件启动即
                // 死亡（缺文件、权限不足、启动 panic），而 kill() 会把真实
                // 退出状态覆盖成我们自己发的 SIGKILL。
                let before = process.try_wait().ok().flatten();
                let describe = |status: std::process::ExitStatus| match status
                    .code()
                    .and_then(crate::process::hint_for_code)
                {
                    Some(hint) => format!("{status}, {hint}"),
                    None => status.to_string(),
                };
                let exit = if let Some(status) = before {
                    format!("exited on its own ({})", describe(status))
                } else {
                    process.kill();
                    match process.try_wait().ok().flatten() {
                        Some(status) => format!("was still running, killed ({})", describe(status)),
                        None => "was still running, killed".to_owned(),
                    }
                };
                // 插件没连上时，stderr 往往已经说明了原因（路径不存在、
                // 权限不足、启动即 panic）。只报超时会把真正的错误埋掉。
                let log_tail = plugin_log_tail(&self.log_dir, id);
                let mut reason = format!("handshake_timeout: {err}; process {exit}");
                if !log_tail.is_empty() {
                    reason.push_str("; plugin log: ");
                    reason.push_str(&log_tail);
                }
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
            heartbeat: self.heartbeat(),
        };
        let outcome = session::handshake(conn, config);
        drop(endpoint);
        match outcome {
            Ok(HandshakeOutcome::Accepted(session)) => {
                session.attach_process(process);
                // 握手期间宿主可能开始关停：此时不能再把会话登记进表
                // （`shutdown_all` 已经取走了快照），直接收掉本次刚拉起的进程。
                if self.shutting_down.load(Ordering::SeqCst) {
                    session.kill();
                    drop(guard);
                    return Err(HostError::new(
                        port::plugin::ErrorCode::Unavailable,
                        "plugin host is shutting down",
                    ));
                }
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
                    // 进程已退出但读端还没报错：立刻关会话，不等心跳超时
                    // （默认 15 s × 3 次，退避重启会迟到一个数量级）。
                    if let Some(reason) = session.reap_exited() {
                        session.close(&reason);
                    }
                    continue;
                }
                host.on_session_exit(&id, &session);
                return;
            }
        });
    }

    fn on_session_exit(&self, id: &PluginId, session: &Arc<SessionInner>) {
        let (stopped_by_user, close_reason) = {
            let Ok(mut map) = self.sessions.lock() else {
                return;
            };
            match map.get(id) {
                // 已由新会话接管：旧监管线程既不能删新会话，也不能替它
                // 退订事件或触发重启。
                Some(existing) if !Arc::ptr_eq(existing, session) => return,
                Some(_) => {
                    map.remove(id);
                }
                None => {}
            }
            // 用会话自己的停机标记：它可能已经被 `reap_idle` / `stop`
            // 从表里摘掉，但仍然是一次主动停机，不能当崩溃重启。
            (session.is_stopping(), session.close_reason())
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
        // 会话被动断开时把原因写进记录：没有它，插件静默消失后只能看到
        // 「restarting」，排查需要翻插件日志
        let exit = close_reason.unwrap_or_else(|| "connection closed".to_owned());
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
                "crashed {attempts} times within {} s ({exit}); automatic restart disabled",
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
        // 手动 `start` 可能已经抢先拉起新会话，此时不应再退避重启。
        if self.session_of(id).is_some() {
            return;
        }
        // 登记「等待重启」：`stop` 与手动 `start` 会把它摘掉，本线程醒来
        // 看到自己已不在表里就放弃，避免用户停机后插件被旧线程拉起来。
        if let Ok(mut pending) = self.pending_restarts.lock() {
            pending.insert(id.clone());
        }
        record_state(
            self.store.as_ref(),
            id,
            PluginState::Starting,
            Some(&format!("restarting in {} s after {exit}", delay.as_secs())),
        );
        self.audit.record(&AuditEvent {
            at: now_secs(),
            plugin: id.to_string(),
            action: "lifecycle:restart".to_owned(),
            capability: None,
            result: AuditResult::Failed,
            detail: Some(format!(
                "crash #{attempts} ({exit}), backoff {} s",
                delay.as_secs()
            )),
        });
        std::thread::sleep(delay);
        // 被 `stop` / 手动 `start` 取消后不再拉起进程；状态由取消方负责。
        let still_pending = self
            .pending_restarts
            .lock()
            .is_ok_and(|mut pending| pending.remove(id));
        if !still_pending || self.shutting_down.load(Ordering::SeqCst) {
            return;
        }
        // 重启失败交给下一次 `start`，不在这里递归重试：超过上限时状态已是
        // `failed`，自动重启必须停止。
        if let Err(err) = self.start_session(id) {
            let reason = format!("automatic restart failed: {}", err.message);
            record_state(self.store.as_ref(), id, PluginState::Failed, Some(&reason));
        }
    }

    /// 撤销列表命中即拒绝启动
    ///
    /// 列表读不出来只记审计、不阻断（已安装插件的可用性优先，
    /// `docs/plugins.md` §16.4）；命中才拒绝，并把原因带给调用方。
    fn reject_if_revoked(&self, record: &PluginRecord) -> Result<(), HostError> {
        let Some(revocations) = self
            .revocations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        else {
            return Ok(());
        };
        let list = match revocations.load() {
            Ok(list) => list,
            Err(err) => {
                self.audit.record(&AuditEvent {
                    at: now_secs(),
                    plugin: record.id.to_string(),
                    action: "revocation:read".to_owned(),
                    capability: None,
                    result: AuditResult::Failed,
                    detail: Some(err.message.clone()),
                });
                return Ok(());
            }
        };
        let Some(entry) = list.hit(record.id.as_str(), &record.version) else {
            return Ok(());
        };
        Err(HostError::denied(
            domain::plugin::revocation::revoked_message(entry, &record.version),
        ))
    }

    fn record(&self, id: &PluginId) -> Result<PluginRecord, HostError> {
        self.store.get(id).map_err(|err| match err {
            port::RepositoryError::NotFound(message) => {
                HostError::not_found(format!("plugin {id} not found: {message}"))
            }
            other => HostError::internal(format!("read plugin {id}: {other}")),
        })
    }

    /// op 的执行约束：输入 schema 校验 + descriptor 声明的超时
    ///
    /// 判定收敛在执行入口：无论调用方走直连还是控制通道，输入合法性判定与
    /// 超时都以已安装的 descriptor 为准，不信任调用方自带的数值。
    fn op_constraints(
        &self,
        id: &PluginId,
        op: &str,
        input: &Value,
        allow_stream: bool,
    ) -> Result<Duration, HostError> {
        op_constraints(&self.record(id)?, op, input, allow_stream)
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
        // 崩溃退避期没有会话，但有一个待执行的重启。取消它，否则 `stop`
        // 返回成功之后插件还是会被退避线程拉起来。
        let cancelled = self
            .pending_restarts
            .lock()
            .is_ok_and(|mut pending| pending.remove(id));
        let Some(session) = self.session_of(id) else {
            if !cancelled {
                return Err(HostError::conflict(format!("plugin {id} is not running")));
            }
            record_state(self.store.as_ref(), id, PluginState::Stopped, None);
            self.audit.record(&AuditEvent {
                at: now_secs(),
                plugin: id.to_string(),
                action: "lifecycle:stop".to_owned(),
                capability: None,
                result: AuditResult::Allowed,
                detail: Some("cancelled a pending restart during crash backoff".to_owned()),
            });
            return Ok(());
        };
        // 不在这里摘除会话：监管线程按 `stopping` 标记判定这是用户停机，
        // 先摘掉会让它看到「表里没有」而当作崩溃自动重启。
        session.shutdown(grace);
        self.await_exit(id, Duration::from_secs(2));
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
        let timeout = self.op_constraints(id, op, &input, false)?;
        let session = self.ensure_session(id)?;
        session.call(op, input, timeout)
    }

    fn stream(
        &self,
        id: &PluginId,
        op: &str,
        input: Value,
        sink: &mut dyn StreamSink,
    ) -> Result<(), HostError> {
        let timeout = self.op_constraints(id, op, &input, true)?;
        let session = self.ensure_session(id)?;
        session.stream(op, input, timeout, sink)
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

/// op 的执行约束：输入 schema 校验 + descriptor 声明的超时
///
/// 判定收敛在执行入口：无论调用方走直连还是控制通道，输入合法性判定与
/// 超时都以已安装的 descriptor 为准，不信任调用方自带的数值。
fn op_constraints(
    record: &PluginRecord,
    op: &str,
    input: &Value,
    allow_stream: bool,
) -> Result<Duration, HostError> {
    let path = record.artifact_dir.join("descriptor.json");
    let bytes = std::fs::read(&path)
        .map_err(|err| HostError::internal(format!("read descriptor for {}: {err}", record.id)))?;
    let descriptor = domain::plugin::descriptor::Descriptor::from_json(&bytes, &record.id)
        .map_err(|err| {
            HostError::internal(format!("invalid descriptor for {}: {err}", record.id))
        })?;
    let resolved = descriptor.op(op).ok_or_else(|| {
        HostError::not_found(format!("op {op} is not declared by plugin {}", record.id))
    })?;
    // call 入口不能触发 stream op；stream 入口只能订阅 stream op。
    let is_stream = resolved.op.kind == domain::plugin::descriptor::OpKind::Stream;
    if is_stream != allow_stream {
        let hint = if is_stream {
            "subscribe via stream instead of call"
        } else {
            "call it via call instead of stream"
        };
        return Err(HostError::invalid_input(format!(
            "op {op} kind does not match the entry point; {hint}"
        )));
    }
    domain::plugin::schema::validate_instance(&resolved.input, input, "input")
        .map_err(|err| HostError::invalid_input(format!("input for op {op}: {err}")))?;
    let timeout_ms = resolved
        .op
        .timeout_ms
        .unwrap_or(domain::plugin::descriptor::DEFAULT_OP_TIMEOUT_MS);
    Ok(Duration::from_millis(timeout_ms))
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

/// 插件日志末尾，用于握手失败的诊断
///
/// 只取最后 2 KiB 的最后 8 行。任何一步失败都退化成空串：诊断信息不能盖过
/// 原始错误，日志缺失也不该把「启动失败」变成另一种失败。
fn plugin_log_tail(log_dir: &Path, id: &PluginId) -> String {
    const TAIL_BYTES: u64 = 2048;
    const TAIL_LINES: usize = 8;
    const MAX_REPORTED_CHARS: usize = 1024;

    let path = log_dir.join(format!("{id}.log"));
    let Ok(mut file) = std::fs::File::open(&path) else {
        return String::new();
    };
    let len = file.metadata().map_or(0, |meta| meta.len());
    if len == 0
        || file
            .seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))
            .is_err()
    {
        return String::new();
    }
    let mut buf = Vec::with_capacity(usize::try_from(TAIL_BYTES).unwrap_or(2048));
    if file.read_to_end(&mut buf).is_err() {
        return String::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let tail = lines[lines.len().saturating_sub(TAIL_LINES)..].join(" | ");
    tail.chars().take(MAX_REPORTED_CHARS).collect()
}

pub(crate) fn now_secs() -> i64 {
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

    #[test]
    fn plugin_log_tail_keeps_only_the_last_lines() {
        use std::fmt::Write as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let mut text = String::new();
        for index in 0..20 {
            let _ = writeln!(text, "line {index}");
        }
        std::fs::write(dir.path().join("demo.log"), text).expect("write log");
        let id = PluginId::parse("demo").expect("id");
        let tail = plugin_log_tail(dir.path(), &id);
        assert_eq!(tail.split(" | ").count(), 8);
        assert!(tail.contains("line 19"));
        assert!(!tail.contains("line 11"));
        let missing = PluginId::parse("missing").expect("id");
        assert!(plugin_log_tail(dir.path(), &missing).is_empty());
    }

    #[test]
    fn op_constraints_validate_input_and_timeout() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("descriptor.json"),
            br#"{
                "schema":1,
                "plugin_id":"demo",
                "api":1,
                "ops":[{
                    "id":"greet",
                    "kind":"action",
                    "title":"Greet",
                    "risk":"read",
                    "input":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]},
                    "output":{"type":"object"},
                    "timeout_ms":2500
                },{
                    "id":"follow",
                    "kind":"stream",
                    "title":"Follow",
                    "risk":"read",
                    "input":{"type":"object"},
                    "output":{"type":"object"}
                }]
            }"#,
        )
        .expect("write descriptor");
        let mut record = sample_record();
        record.artifact_dir = dir.path().to_path_buf();

        let input = serde_json::json!({ "name": "Steve" });
        let timeout = op_constraints(&record, "greet", &input, false).expect("valid");
        assert_eq!(timeout, Duration::from_millis(2_500));

        let default = op_constraints(&record, "follow", &serde_json::json!({}), true)
            .expect("stream without declared timeout");
        assert_eq!(
            default,
            Duration::from_millis(domain::plugin::descriptor::DEFAULT_OP_TIMEOUT_MS)
        );

        // 缺必填字段
        let err =
            op_constraints(&record, "greet", &serde_json::json!({}), false).expect_err("required");
        assert_eq!(err.code, port::plugin::ErrorCode::InvalidInput);

        // 未声明的 op
        let err =
            op_constraints(&record, "nope", &serde_json::json!({}), false).expect_err("unknown op");
        assert_eq!(err.code, port::plugin::ErrorCode::NotFound);

        // kind 与入口不匹配
        let err = op_constraints(&record, "follow", &serde_json::json!({}), false)
            .expect_err("stream via call");
        assert_eq!(err.code, port::plugin::ErrorCode::InvalidInput);
        let err = op_constraints(&record, "greet", &input, true).expect_err("action via stream");
        assert_eq!(err.code, port::plugin::ErrorCode::InvalidInput);
    }

    #[test]
    fn op_constraints_reject_missing_descriptor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut record = sample_record();
        record.artifact_dir = dir.path().to_path_buf();
        let err =
            op_constraints(&record, "greet", &serde_json::json!({}), false).expect_err("missing");
        assert_eq!(err.code, port::plugin::ErrorCode::Internal);
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
            blocked: false,
            quarantined: false,
        }
    }
}
