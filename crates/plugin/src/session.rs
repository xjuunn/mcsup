//! 一条已握手的插件会话
//!
//! 会话负责：帧读循环、调用与流、宿主能力分发、心跳、事件投递、异常关闭。
//! 判定（能力交集、订阅合法性、状态迁移）在 `domain`；本模块只执行。
//!
//! # 方向与 id
//!
//! 两个方向各自维护调用 id：宿主 → 插件用 `SessionInner::call`，插件 →
//! 宿主在 `SessionInner::handle_host_call` 里用回帧带上的 id。`result`
//! 按方向归属，不会串。

use crate::api::{self, DispatchContext, HostServices};
use crate::event::{EventDelivery, event_frame};
use crate::process::SpawnedPlugin;
use domain::plugin::PLUGIN_API_VERSION;
use domain::plugin::capability::{self, Capability};
use domain::plugin::event::{self, Subscription};
use port::plugin::{
    AuditEvent, AuditResult, AuditSink, HostError, PluginRecord, SessionInfo, StreamSink,
};
use port::plugin::{MAX_STREAM_BYTES, category_of};
use protocol::plugin::{
    Call, Cancel, ConfigFrame, ConfigValues, Frame, FrameError, Hello, Reject, RejectCode,
    ResultFrame, StreamFrame, Welcome, WelcomeLimits, WelcomeScope, WireError, read_frame,
    write_frame,
};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::io::BufReader;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::conn::Connection;

/// 配置变更等待插件确认的上限
const CONFIG_APPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// 向插件写一帧的上限
///
/// Windows 命名管道不支持写超时（`interprocess` 返回 `Unsupported`），所以
/// 超时不能依赖 socket 选项：写操作跑在独立线程里，调用方等确认。
/// 插件不读数据时，超时即关闭会话并杀进程，绝不把宿主锁死。
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// 写线程队列深度
///
/// 每个调用方都在等自己的确认，所以队列只是短时缓冲：允许事件突发，
/// 但写端真的堵住时不能无限堆内存。
const WRITE_QUEUE_DEPTH: usize = 256;

/// 流帧通道深度；插件超出即视为背压失控，直接断流
const STREAM_QUEUE_DEPTH: usize = 64;

/// 心跳超时检查的轮询间隔
const HEARTBEAT_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// 心跳节奏：发一次 `ping` 后等待 `pong` 的窗口，以及连续丢失上限
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Heartbeat {
    /// 单次 `ping` / `pong` 的等待窗口
    pub interval: Duration,
    /// 连续未回应多少次后判定失联
    pub miss_limit: u32,
}

impl Default for Heartbeat {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(15),
            miss_limit: 3,
        }
    }
}

/// 握手配置
pub(crate) struct SessionConfig {
    pub record: PluginRecord,
    pub token: String,
    pub host_version: String,
    pub limits: WelcomeLimits,
    pub config: ConfigValues,
    pub services: Arc<HostServices>,
    pub audit: Arc<dyn AuditSink>,
    pub handshake_timeout: Duration,
    /// 已拉起的插件进程 PID；握手阶段就能填入会话表
    pub pid: u32,
    /// 心跳节奏
    pub heartbeat: Heartbeat,
}

/// 握手结果
pub(crate) enum HandshakeOutcome {
    /// 已建立会话
    Accepted(Arc<SessionInner>),
    /// 已回 `reject`，调用方按宽限杀进程
    Rejected(String),
}

/// 完成握手
///
/// # Errors
///
/// 连接读超时、首帧不是 `hello`、帧非法时返回 [`HostError`]；业务拒绝通过
/// [`HandshakeOutcome::Rejected`] 表达（已写出 `reject` 帧）。
#[allow(clippy::too_many_lines)] // 握手帧序、能力校验与 Welcome 组装是同一段协议
pub(crate) fn handshake(
    mut writer: Connection,
    cfg: SessionConfig,
) -> Result<HandshakeOutcome, HostError> {
    // Unix / TCP 支持真正的读超时；Windows 命名管道返回 `Unsupported`，
    // 所以截止时间由下面的短线程兜底，不能只依赖 socket 选项。
    let _ = writer.set_read_timeout(Some(cfg.handshake_timeout));
    let read_half = writer
        .try_clone()
        .map_err(|err| HostError::internal(format!("clone plugin connection: {err}")))?;
    let mut reader = BufReader::new(read_half);
    let (frame_tx, frame_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = read_frame(&mut reader);
        // 把 reader 原样送回：BufReader 里可能已经缓了后续帧，不能丢。
        let _ = frame_tx.send((reader, result));
    });
    let Ok((reader, frame)) = frame_rx.recv_timeout(cfg.handshake_timeout) else {
        writer.shutdown();
        return Err(HostError::new(
            port::plugin::ErrorCode::Crashed,
            format!(
                "handshake failed: plugin sent no hello within {} s",
                cfg.handshake_timeout.as_secs()
            ),
        ));
    };
    let frame = match frame {
        Ok(frame) => frame,
        Err(err) => {
            return Err(HostError::new(
                port::plugin::ErrorCode::Crashed,
                format!("handshake failed: {err}"),
            ));
        }
    };
    let Hello {
        token,
        api,
        plugin_id,
        plugin_version,
        platform: _,
        pid: _,
        requested,
        subscriptions,
    } = match frame {
        Frame::Hello(hello) => hello,
        other => {
            let reason = format!("first frame must be hello, got {}", other.kind());
            reject(&mut writer, RejectCode::Rejected, reason.clone(), None);
            return Ok(HandshakeOutcome::Rejected(reason));
        }
    };
    let record = &cfg.record;
    if !token_eq(&token, &cfg.token) {
        let reason = "invalid session token".to_owned();
        reject(&mut writer, RejectCode::Unauthorized, reason.clone(), None);
        return Ok(HandshakeOutcome::Rejected(reason));
    }
    if plugin_id != record.id.as_str() || plugin_version != record.version {
        let reason = format!(
            "identity mismatch: plugin reports {}@{}, record is {}@{}",
            plugin_id, plugin_version, record.id, record.version
        );
        reject(
            &mut writer,
            RejectCode::IdentityMismatch,
            reason.clone(),
            None,
        );
        return Ok(HandshakeOutcome::Rejected(reason));
    }
    if api != PLUGIN_API_VERSION || record.api != PLUGIN_API_VERSION {
        let reason = format!("plugin requires api {api}, host supports {PLUGIN_API_VERSION}");
        reject(
            &mut writer,
            RejectCode::ApiMismatch,
            reason.clone(),
            Some(serde_json::json!({
                "plugin_api": api,
                "host_api": PLUGIN_API_VERSION,
            })),
        );
        return Ok(HandshakeOutcome::Rejected(reason));
    }
    let manifest_caps = record.declared_capabilities();
    let approved = &record.approved;
    let mut requested_set = BTreeSet::new();
    for raw in &requested {
        match Capability::parse(raw) {
            Ok(cap) if manifest_caps.contains(&cap) => {
                requested_set.insert(cap);
            }
            _ => {
                let parsed = Capability::parse(raw).ok();
                cfg.audit.record(&AuditEvent {
                    at: now_secs(),
                    plugin: record.id.to_string(),
                    action: "handshake.request".to_owned(),
                    capability: parsed,
                    result: AuditResult::Denied,
                    detail: Some(format!("requested {raw} is outside the manifest")),
                });
            }
        }
    }
    let granted = capability::granted(&manifest_caps, &requested_set, approved);
    let declared_subs = event::parse_all(&record.manifest.subscriptions).unwrap_or_default();
    let mut requested_subs = BTreeSet::new();
    for raw in &subscriptions {
        match Subscription::parse(raw) {
            Ok(kind)
                if declared_subs.contains(&kind)
                    && granted.contains(&kind.required_capability()) =>
            {
                requested_subs.insert(kind);
            }
            Ok(kind) => {
                let reason = format!(
                    "subscription {kind} is not allowed (declared: {}, capability granted: {})",
                    declared_subs.contains(&kind),
                    granted.contains(&kind.required_capability())
                );
                reject(&mut writer, RejectCode::NotApproved, reason.clone(), None);
                return Ok(HandshakeOutcome::Rejected(reason));
            }
            Err(_) => {
                let reason = format!("unknown subscription: {raw}");
                reject(
                    &mut writer,
                    RejectCode::SchemaMismatch,
                    reason.clone(),
                    None,
                );
                return Ok(HandshakeOutcome::Rejected(reason));
            }
        }
    }
    let session_id = uuid::Uuid::new_v4().simple().to_string();
    let welcome = Welcome {
        session_id: session_id.clone(),
        host_version: cfg.host_version.clone(),
        granted: granted.iter().map(ToString::to_string).collect(),
        limits: cfg.limits,
        config: cfg.config.clone(),
        scope: WelcomeScope {
            kind: record.scope.as_str().to_owned(),
            server: record.server.as_ref().map(ToString::to_string),
        },
    };
    if write_frame(&mut writer, &Frame::Welcome(welcome)).is_err() {
        return Err(HostError::new(
            port::plugin::ErrorCode::Crashed,
            "failed to send welcome",
        ));
    }
    let _ = writer.set_read_timeout(None);
    // 写端交给独立线程：调用方只等确认，阻塞写永远不会拖住宿主线程。
    let closer = writer.try_clone().ok();
    let (job_tx, job_rx) = mpsc::sync_channel::<WriteJob>(WRITE_QUEUE_DEPTH);
    std::thread::spawn(move || writer_loop(&mut writer, &job_rx));
    let seq = AtomicU64::new(0);
    let info = SessionInfo {
        session_id,
        granted,
        pid: cfg.pid,
        started_at: now_secs(),
    };
    let inner = Arc::new(SessionInner {
        record: cfg.record,
        info: info.clone(),
        subscriptions: requested_subs,
        grants: Mutex::new(info.granted.clone()),
        reader: Mutex::new(Some(reader)),
        writer_tx: Mutex::new(Some(job_tx)),
        closer: Mutex::new(closer),
        pending: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        seq,
        alive: AtomicBool::new(true),
        close_reason: Mutex::new(None),
        last_pong: AtomicU64::new(0),
        ping_seq: AtomicU64::new(0),
        stopping: AtomicBool::new(false),
        inflight: AtomicU32::new(0),
        last_activity: Mutex::new(Instant::now()),
        process: Mutex::new(None),
        services: cfg.services,
        audit: cfg.audit,
        heartbeat: cfg.heartbeat,
    });
    Ok(HandshakeOutcome::Accepted(inner))
}

fn reject(conn: &mut Connection, code: RejectCode, message: String, details: Option<Value>) {
    let _ = write_frame(
        conn,
        &Frame::Reject(Reject {
            code,
            message,
            details,
        }),
    );
}

fn token_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (a, b) in left.bytes().zip(right.bytes()) {
        diff |= a ^ b;
    }
    diff == 0
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

enum Pending {
    Result(Sender<ResultFrame>),
    Stream(SyncSender<StreamEvent>),
}

/// 流式调用从插件收到的事件
///
/// 错误走单独的分支，不能把 `Result` 帧丢给流消费者——否则通道断开，
/// 调用方只能看到「会话结束」这种不可诊断的 `unavailable`。
enum StreamEvent {
    Chunk(StreamFrame),
    Failed(ResultFrame),
}

/// 交给写线程的一帧；`ack` 让调用方拿到「真的写出去了」或具体错误
struct WriteJob {
    frame: Frame,
    ack: Sender<Result<(), String>>,
}

/// 写线程：独占写端，串行化所有帧
///
/// 队列关闭（会话结束）或写失败即退出。阻塞在 `write` 里时由 `kill` 进程
/// 解除：对端句柄关闭后写返回错误。
///
/// 结果帧超限时改发错误帧（[`oversize_replacement`]）：能力层的输出——
/// 文件内容、HTTP 响应体——本来就可能有几 MiB，是合法调用，不该让会话陪葬。
fn writer_loop(writer: &mut Connection, jobs: &Receiver<WriteJob>) {
    while let Ok(job) = jobs.recv() {
        let result = match write_frame(writer, &job.frame) {
            Ok(()) => Ok(()),
            Err(FrameError::TooLarge { limit }) => match oversize_replacement(&job.frame, limit) {
                Some(replacement) => write_frame(writer, &replacement),
                None => Err(FrameError::TooLarge { limit }),
            }
            .map_err(|err| err.to_string()),
            Err(err) => Err(err.to_string()),
        };
        let failed = result.is_err();
        let _ = job.ack.send(result);
        if failed {
            break;
        }
    }
    writer.shutdown();
}

/// 超限结果帧的错误替身
///
/// 只有 `result` 帧能这样替换：它的载荷是宿主自己去取的（文件、网络），
/// 超限是「这次取多了」，换成 `limit_exceeded` 让插件知道该怎么改。流与
/// 事件帧的载荷由插件自己切分，超限说明对端用错了协议，交给写失败路径。
fn oversize_replacement(frame: &Frame, limit: usize) -> Option<Frame> {
    let Frame::Result(result) = frame else {
        return None;
    };
    Some(Frame::Result(ResultFrame::error(
        result.id,
        WireError::from_code(
            port::plugin::ErrorCode::LimitExceeded,
            format!("response exceeds the {limit} byte frame limit"),
        ),
    )))
}

/// 会话内部状态
pub(crate) struct SessionInner {
    pub(crate) record: PluginRecord,
    pub(crate) info: SessionInfo,
    subscriptions: BTreeSet<Subscription>,
    /// 当前会话的有效授予集合；撤销授权时原地收缩，不重启会话
    grants: Mutex<BTreeSet<Capability>>,
    /// 读半端：由读线程独占；`None` 表示读线程已取走
    reader: Mutex<Option<BufReader<Connection>>>,
    /// 写线程的队列；`None` 表示会话已关闭
    writer_tx: Mutex<Option<SyncSender<WriteJob>>>,
    /// 写端克隆：仅用于关闭时打断阻塞的读/写（Unix/TCP 生效）
    closer: Mutex<Option<Connection>>,
    pending: Mutex<HashMap<u64, Pending>>,
    next_id: AtomicU64,
    seq: AtomicU64,
    alive: AtomicBool,
    /// 关闭原因：`close` 记录，宿主据此写出可诊断的 `last_error`
    close_reason: Mutex<Option<String>>,
    last_pong: AtomicU64,
    ping_seq: AtomicU64,
    stopping: AtomicBool,
    inflight: AtomicU32,
    last_activity: Mutex<Instant>,
    process: Mutex<Option<SpawnedPlugin>>,
    services: Arc<HostServices>,
    audit: Arc<dyn AuditSink>,
    heartbeat: Heartbeat,
}

impl std::fmt::Debug for SessionInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.record.id)
            .field("session", &self.info.session_id)
            .field("alive", &self.alive.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl SessionInner {
    /// 绑定进程句柄（握手在进程拉起之后完成）
    pub(crate) fn attach_process(&self, process: SpawnedPlugin) {
        if let Ok(mut slot) = self.process.lock() {
            *slot = Some(process);
        }
    }

    /// 本次会话实际订阅的事件
    pub(crate) fn subscriptions(&self) -> &BTreeSet<Subscription> {
        &self.subscriptions
    }

    /// 当前有效授予集合
    pub(crate) fn grants(&self) -> BTreeSet<Capability> {
        self.grants
            .lock()
            .map(|grants| grants.clone())
            .unwrap_or_default()
    }

    /// 撤销一项能力：立即影响后续调用与事件投递
    pub(crate) fn revoke_grant(&self, capability: Capability) {
        if let Ok(mut grants) = self.grants.lock() {
            grants.remove(&capability);
        }
    }

    fn has_grant(&self, capability: Capability) -> bool {
        self.grants
            .lock()
            .is_ok_and(|grants| grants.contains(&capability))
    }

    /// 启动读循环与心跳
    pub(crate) fn start(self: &Arc<Self>) {
        let inner = Arc::clone(self);
        std::thread::spawn(move || inner.read_loop());
        let inner = Arc::clone(self);
        std::thread::spawn(move || inner.heartbeat_loop());
    }

    /// 是否存活
    pub(crate) fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// 主动探测子进程是否已退出
    ///
    /// 命名管道的读端在 Windows 上可能长时间没有响应，只靠心跳（默认最长
    /// 45 s）判定崩溃会让退避重启迟到一个数量级。监管线程每拍问一次进程
    /// 状态，退出即立刻关会话。
    pub(crate) fn reap_exited(&self) -> Option<String> {
        let mut slot = self.process.lock().ok()?;
        let process = slot.as_mut()?;
        match process.try_wait() {
            Ok(Some(status)) => Some(format!("plugin process exited ({status})")),
            _ => None,
        }
    }

    /// 关闭原因（`close` 第一次调用时写入）
    pub(crate) fn close_reason(&self) -> Option<String> {
        self.close_reason.lock().ok().and_then(|slot| slot.clone())
    }

    /// 是否正在优雅停机
    pub(crate) fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    /// 会话 id
    pub(crate) fn session_id(&self) -> &str {
        &self.info.session_id
    }

    /// 最近活动时间
    pub(crate) fn idle_for(&self) -> Duration {
        self.last_activity
            .lock()
            .map(|at| at.elapsed())
            .unwrap_or_default()
    }

    fn touch(&self) {
        if let Ok(mut at) = self.last_activity.lock() {
            *at = Instant::now();
        }
    }

    /// 发送一帧；失败即关闭会话
    ///
    /// 写操作在独立线程里执行，这里只等确认：插件不读数据时会在
    /// [`WRITE_TIMEOUT`] 后关闭会话并杀进程，不会永久阻塞调用方。
    fn send(&self, frame: &Frame) -> Result<(), HostError> {
        let queue = self
            .writer_tx
            .lock()
            .map_err(|_| HostError::internal("session write lock poisoned"))?
            .clone();
        let Some(queue) = queue else {
            return Err(self.closed_error());
        };
        let (ack_tx, ack_rx) = mpsc::channel();
        match queue.try_send(WriteJob {
            frame: frame.clone(),
            ack: ack_tx,
        }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                let reason = format!(
                    "write queue is full ({WRITE_QUEUE_DEPTH} frames); plugin is not reading its input"
                );
                self.close(&reason);
                return Err(HostError::new(port::plugin::ErrorCode::Crashed, reason));
            }
            Err(TrySendError::Disconnected(_)) => {
                self.close("writer thread stopped");
                return Err(self.closed_error());
            }
        }
        match ack_rx.recv_timeout(WRITE_TIMEOUT) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => {
                self.close(&format!("write failed: {err}"));
                Err(HostError::new(
                    port::plugin::ErrorCode::Crashed,
                    format!("plugin connection closed: {err}"),
                ))
            }
            Err(RecvTimeoutError::Timeout) => {
                let reason = format!(
                    "write timed out after {} s (plugin is not reading its input)",
                    WRITE_TIMEOUT.as_secs()
                );
                self.close(&reason);
                Err(HostError::new(port::plugin::ErrorCode::Crashed, reason))
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.close("writer thread stopped");
                Err(self.closed_error())
            }
        }
    }

    fn closed_error(&self) -> HostError {
        let reason = self
            .close_reason()
            .unwrap_or_else(|| "unknown reason".to_owned());
        HostError::new(
            port::plugin::ErrorCode::Unavailable,
            format!("plugin session is closed: {reason}"),
        )
    }

    /// 调用插件 op
    ///
    /// # Errors
    ///
    /// 超时、连接断开、插件返回错误时返回 [`HostError`]
    pub(crate) fn call(
        self: &Arc<Self>,
        op: &str,
        input: Value,
        timeout: Duration,
    ) -> Result<Value, HostError> {
        self.touch();
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        self.register(id, Pending::Result(tx))?;
        if let Err(err) = self.send(&Frame::Call(Call::op(id, op, input))) {
            self.remove_pending(id);
            return Err(err);
        }
        match rx.recv_timeout(timeout) {
            Ok(result) => {
                if result.ok {
                    Ok(result.output.unwrap_or(Value::Null))
                } else {
                    Err(wire_to_host(
                        result.error,
                        "plugin returned an unsuccessful result",
                    ))
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                self.remove_pending(id);
                let _ = self.send(&Frame::Cancel(Cancel {
                    id,
                    reason: Some("timeout".to_owned()),
                }));
                Err(HostError::new(
                    port::plugin::ErrorCode::Timeout,
                    format!("plugin op {op} timed out after {} ms", timeout.as_millis()),
                ))
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.remove_pending(id);
                Err(HostError::new(
                    port::plugin::ErrorCode::Unavailable,
                    "plugin session ended before the result arrived",
                ))
            }
        }
    }

    /// 订阅插件 stream
    ///
    /// # Errors
    ///
    /// 同 [`Self::call`]；超过总字节上限返回 `limit_exceeded`
    pub(crate) fn stream(
        self: &Arc<Self>,
        op: &str,
        input: Value,
        timeout: Duration,
        sink: &mut dyn StreamSink,
    ) -> Result<(), HostError> {
        self.touch();
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::sync_channel::<StreamEvent>(STREAM_QUEUE_DEPTH);
        self.register(id, Pending::Stream(tx))?;
        if let Err(err) = self.send(&Frame::Call(Call::op(id, op, input))) {
            self.remove_pending(id);
            return Err(err);
        }
        let deadline = Instant::now() + timeout;
        let mut total = 0_usize;
        // seq 从 0 连续递增（plugins.md §6.5）；缺口即终止流
        let mut expected_seq = 0_u64;
        loop {
            let now = Instant::now();
            if now >= deadline {
                self.cancel(id, "timeout");
                return Err(HostError::new(
                    port::plugin::ErrorCode::Timeout,
                    format!("stream {op} timed out"),
                ));
            }
            let wait = (deadline - now).min(Duration::from_secs(60));
            match rx.recv_timeout(wait) {
                Ok(StreamEvent::Failed(result)) => {
                    return Err(wire_to_host(
                        result.error,
                        "plugin reported a stream failure",
                    ));
                }
                Ok(StreamEvent::Chunk(frame)) => {
                    if frame.seq != expected_seq {
                        self.cancel(id, "protocol");
                        return Err(HostError::new(
                            port::plugin::ErrorCode::Internal,
                            format!(
                                "stream {op} broke the seq sequence: got {}, expected {expected_seq}",
                                frame.seq
                            ),
                        ));
                    }
                    expected_seq += 1;
                    total = total.saturating_add(
                        serde_json::to_vec(&frame.chunk).map_or(0, |bytes| bytes.len()),
                    );
                    if total > MAX_STREAM_BYTES {
                        self.cancel(id, "limit_exceeded");
                        return Err(HostError::limit_exceeded(format!(
                            "stream {op} exceeded {MAX_STREAM_BYTES} bytes"
                        )));
                    }
                    if let Err(err) = sink.on_chunk(&frame.chunk, frame.done) {
                        self.cancel(id, "client");
                        return Err(err);
                    }
                    if frame.done {
                        return Ok(());
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    self.cancel(id, "idle");
                    return Err(HostError::new(
                        port::plugin::ErrorCode::Timeout,
                        format!("stream {op} was idle for 60 s"),
                    ));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(HostError::new(
                        port::plugin::ErrorCode::Unavailable,
                        "plugin session ended before the stream completed",
                    ));
                }
            }
        }
    }

    /// 下发配置变更并等待插件确认
    ///
    /// 插件应用失败时返回它给出的错误；调用方负责回滚落盘值。
    ///
    /// # Errors
    ///
    /// 插件回 `ok:false`、连接断开或 10 s 内未确认时返回对应错误
    pub(crate) fn apply_config(
        self: &Arc<Self>,
        revision: u64,
        values: &Value,
    ) -> Result<(), HostError> {
        self.touch();
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        self.register(id, Pending::Result(tx))?;
        let frame = Frame::Config(ConfigFrame {
            id,
            revision,
            values: values.clone(),
        });
        if let Err(err) = self.send(&frame) {
            self.remove_pending(id);
            return Err(err);
        }
        match rx.recv_timeout(CONFIG_APPLY_TIMEOUT) {
            Ok(result) if result.ok => Ok(()),
            Ok(result) => Err(wire_to_host(
                result.error,
                "plugin rejected the config change",
            )),
            Err(RecvTimeoutError::Timeout) => {
                self.remove_pending(id);
                Err(HostError::new(
                    port::plugin::ErrorCode::Timeout,
                    format!(
                        "plugin did not acknowledge config revision {revision} within {} s",
                        CONFIG_APPLY_TIMEOUT.as_secs()
                    ),
                ))
            }
            Err(RecvTimeoutError::Disconnected) => Err(HostError::new(
                port::plugin::ErrorCode::Unavailable,
                "plugin session ended before acknowledging the config change",
            )),
        }
    }

    fn cancel(&self, id: u64, reason: &str) {
        self.remove_pending(id);
        let _ = self.send(&Frame::Cancel(Cancel {
            id,
            reason: Some(reason.to_owned()),
        }));
    }

    fn register(&self, id: u64, pending: Pending) -> Result<(), HostError> {
        let mut map = self
            .pending
            .lock()
            .map_err(|_| HostError::internal("session lock poisoned"))?;
        map.insert(id, pending);
        Ok(())
    }

    fn remove_pending(&self, id: u64) {
        if let Ok(mut map) = self.pending.lock() {
            map.remove(&id);
        }
    }

    fn read_loop(self: Arc<Self>) {
        let Ok(mut slot) = self.reader.lock() else {
            self.close("reader lock poisoned");
            return;
        };
        let Some(mut reader) = slot.take() else {
            self.close("read loop already started");
            return;
        };
        loop {
            match read_frame(&mut reader) {
                Ok(frame) => self.on_frame(frame),
                Err(err) => {
                    if self.is_alive() {
                        self.close(&format!("connection ended: {err}"));
                    }
                    break;
                }
            }
            if !self.is_alive() {
                break;
            }
        }
    }

    fn on_frame(self: &Arc<Self>, frame: Frame) {
        match frame {
            Frame::Result(result) => {
                let pending = self
                    .pending
                    .lock()
                    .ok()
                    .and_then(|mut map| map.remove(&result.id));
                match pending {
                    Some(Pending::Result(tx)) => {
                        let _ = tx.send(result);
                    }
                    // 流式调用用 `result` 报错：把它原样交给消费者，
                    // 不能丢帧断通道（否则只能看到 unavailable）。
                    Some(Pending::Stream(tx)) => {
                        let _ = tx.try_send(StreamEvent::Failed(result));
                    }
                    None => {}
                }
            }
            Frame::Stream(stream) => {
                let done = stream.done;
                let id = stream.id;
                let mut overflow = false;
                let failed = {
                    let Ok(map) = self.pending.lock() else {
                        return;
                    };
                    match map.get(&id) {
                        Some(Pending::Stream(tx)) => {
                            match tx.try_send(StreamEvent::Chunk(stream)) {
                                Ok(()) => false,
                                Err(TrySendError::Full(_)) => {
                                    overflow = true;
                                    true
                                }
                                Err(TrySendError::Disconnected(_)) => true,
                            }
                        }
                        _ => false,
                    }
                };
                if done || failed {
                    self.remove_pending(id);
                }
                if overflow {
                    // 消费者是本进程代码；队列打满说明插件在刷帧，不能为了
                    // 一个坏流把读线程（连带 pong）卡死。
                    self.close(&format!("stream {id} exceeded the host queue limit"));
                }
            }
            Frame::Pong(n) => {
                self.last_pong.store(n, Ordering::SeqCst);
            }
            Frame::Ping(n) => {
                let _ = self.send(&Frame::Pong(n));
            }
            Frame::Call(call) => self.handle_host_call(call),
            Frame::Cancel(cancel) => {
                self.remove_pending(cancel.id);
            }
            Frame::Unknown { .. }
            | Frame::Hello(_)
            | Frame::Welcome(_)
            | Frame::Reject(_)
            | Frame::Event(_)
            | Frame::Config(_)
            | Frame::Shutdown { .. } => {}
        }
    }

    fn handle_host_call(self: &Arc<Self>, call: Call) {
        if self.inflight.fetch_add(1, Ordering::SeqCst) >= 8 {
            self.inflight.fetch_sub(1, Ordering::SeqCst);
            let error = HostError::limit_exceeded("plugin exceeded 8 inflight host calls");
            self.write_result(&call, Err(error));
            return;
        }
        let inner = Arc::clone(self);
        std::thread::spawn(move || {
            // 每次都读取当前授予集合：撤销授权必须即时生效。
            let granted = inner
                .grants
                .lock()
                .map(|grants| grants.clone())
                .unwrap_or_default();
            let result = api::dispatch(
                &inner.services,
                &call,
                DispatchContext {
                    plugin: &inner.record.id,
                    granted: &granted,
                    scope_server: inner.record.server.as_ref(),
                    contracts: inner.services.contracts.as_deref(),
                },
            );
            inner.audit.record(&AuditEvent {
                at: now_secs(),
                plugin: inner.record.id.to_string(),
                action: match (call.capability.as_deref(), call.method.as_deref()) {
                    (Some(capability), Some(method)) => {
                        format!("capability:{capability}:{method}")
                    }
                    (Some(capability), None) => format!("capability:{capability}"),
                    _ => "capability:unknown".to_owned(),
                },
                capability: call
                    .capability
                    .as_deref()
                    .and_then(|raw| Capability::parse(raw).ok()),
                result: match &result {
                    Ok(_) => AuditResult::Allowed,
                    Err(err) if err.code == port::plugin::ErrorCode::Denied => AuditResult::Denied,
                    Err(_) => AuditResult::Failed,
                },
                detail: match &result {
                    // 快照 id 与落盘文件名是备份 / 内容审计的检索键；只摘
                    // 这两个非敏感字段，不能把整个输出写进审计
                    //（`storage:kv` 的读取含明文值）。
                    Ok(value) => Self::audit_detail(value),
                    Err(err) => Some(format!("{}: {}", err.code, err.message)),
                },
            });
            inner.write_result(&call, result);
            inner.inflight.fetch_sub(1, Ordering::SeqCst);
        });
    }

    /// 审计摘要：只摘可检索的非敏感字段
    fn audit_detail(value: &Value) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(id) = value.get("snapshot").and_then(Value::as_str) {
            parts.push(format!("snapshot={id}"));
        }
        if let Some(file) = value.get("file").and_then(Value::as_str) {
            parts.push(format!("file={file}"));
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" "))
        }
    }

    fn write_result(&self, call: &Call, result: Result<Value, HostError>) {
        let frame = match result {
            Ok(output) => ResultFrame::ok(call.id, output),
            Err(err) => {
                ResultFrame::error(call.id, WireError::from_code(err.code, err.message.clone()))
            }
        };
        let _ = self.send(&Frame::Result(frame));
    }

    fn heartbeat_loop(self: Arc<Self>) {
        let mut missed = 0_u32;
        while self.is_alive() {
            let n = self.ping_seq.fetch_add(1, Ordering::SeqCst) + 1;
            if self.send(&Frame::Ping(n)).is_err() {
                break;
            }
            // 发出 ping 后必须留出窗口等 pong 回来再判定，否则本次心跳
            // 永远记一次丢失，活跃插件也会在 miss_limit 个周期后被误杀。
            if self.wait_pong(n) {
                missed = 0;
            } else {
                missed += 1;
            }
            if !self.is_alive() {
                break;
            }
            if missed >= self.heartbeat.miss_limit {
                self.close("heartbeat timeout");
                break;
            }
        }
    }

    /// 在心跳窗口内等待第 `n` 次心跳的回应；停机时立即返回
    fn wait_pong(&self, n: u64) -> bool {
        let deadline = Instant::now() + self.heartbeat.interval;
        loop {
            if self.last_pong.load(Ordering::SeqCst) >= n {
                return true;
            }
            if !self.is_alive() || Instant::now() >= deadline {
                return self.last_pong.load(Ordering::SeqCst) >= n;
            }
            std::thread::sleep(HEARTBEAT_POLL_INTERVAL);
        }
    }

    /// 请求优雅退出
    pub(crate) fn shutdown(&self, grace: Duration) {
        self.stopping.store(true, Ordering::SeqCst);
        let _ = self.send(&Frame::Shutdown {
            grace_ms: u64::try_from(grace.as_millis()).unwrap_or(u64::MAX),
        });
        let exited = self
            .process
            .lock()
            .ok()
            .and_then(|mut process| process.as_mut().map(|p| p.wait_timeout(grace)))
            .unwrap_or(true);
        if !exited {
            self.kill();
        }
        self.close("shutdown");
    }

    /// 强杀进程并关闭连接
    pub(crate) fn kill(&self) {
        if let Ok(mut process) = self.process.lock()
            && let Some(process) = process.as_mut()
        {
            process.kill();
        }
        self.close("killed");
    }

    pub(crate) fn close(&self, reason: &str) {
        // 只记录第一次关闭原因：后续调用（如监管线程的 `killed`）不该覆盖
        // 真正触发关闭的事件
        if let Ok(mut slot) = self.close_reason.lock()
            && slot.is_none()
        {
            *slot = Some(reason.to_owned());
        }
        if self.alive.swap(false, Ordering::SeqCst) {
            // 先杀进程：阻塞在管道读写里的线程会因对端关闭立即返回。
            // 顺序不能反，否则写线程可能还握着一个永久阻塞的写。
            if let Ok(mut process) = self.process.lock()
                && let Some(process) = process.as_mut()
            {
                process.kill();
            }
            if let Ok(mut slot) = self.writer_tx.lock() {
                *slot = None;
            }
            if let Some(closer) = self.closer.lock().ok().and_then(|mut slot| slot.take()) {
                closer.shutdown();
            }
            let pending = self
                .pending
                .lock()
                .map(|mut map| std::mem::take(&mut *map))
                .unwrap_or_default();
            for (_, pending) in pending {
                match pending {
                    Pending::Result(tx) => {
                        let _ = tx.send(ResultFrame::error(
                            0,
                            WireError::from_code(
                                port::plugin::ErrorCode::Crashed,
                                format!("plugin session closed: {reason}"),
                            ),
                        ));
                    }
                    Pending::Stream(tx) => {
                        let _ = tx.try_send(StreamEvent::Failed(ResultFrame::error(
                            0,
                            WireError::from_code(
                                port::plugin::ErrorCode::Crashed,
                                format!("plugin session closed: {reason}"),
                            ),
                        )));
                    }
                }
            }
        }
    }
}

impl EventDelivery for SessionInner {
    fn deliver(&self, kind: Subscription, at: &str, server: Option<&str>, data: Value) {
        if !self.is_alive() {
            return;
        }
        // 撤销能力后，事件立刻停止投递，不需要重启进程。
        if !self.has_grant(kind.required_capability()) {
            return;
        }
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        let frame = event_frame(seq, kind, at, server, data);
        if self.send(&Frame::Event(frame)).is_ok() {
            self.touch();
        }
    }
}

fn wire_to_host(error: Option<WireError>, fallback: &str) -> HostError {
    match error {
        Some(error) => HostError {
            code: error.code,
            category: category_of(error.code),
            retryable: error.retryable,
            message: error.message,
        },
        None => HostError::new(port::plugin::ErrorCode::Internal, fallback),
    }
}
