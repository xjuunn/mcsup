//! 服务端：监听、TLS 握手、认证状态机、分发
//!
//! 一条连接一个线程：远端命令是低频操作，连接数等于「同时在敲命令的人」，
//! 线程比异步执行器的调度成本更低、也更容易在崩溃时定位。真正的并发压力
//! 在 Minecraft 服务器进程上，不在这个控制通道。
//!
//! 连接内的状态机是**严格顺序**的：
//!
//! ```text
//! 首帧必须是 Hello → （Enroll | Authenticate）→ 业务帧
//! ```
//!
//! 任何越序帧都按协议错误处理并断开。允许「跳过认证直接发业务帧」会让权限
//! 检查变成可选项，而可选项迟早会被忘记。

use crate::error::TransportError;
use crate::handler::{Handler, RemoteFailure, SessionInfo};
use crate::identity::ServerIdentity;
use crate::session::Session;
use crate::tls;
use protocol::remote::{self, ErrorKind, Request, Response, TaskEvent, TaskStage};
use rustls::{ServerConnection, StreamOwned};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// 同时允许的连接数上限
///
/// 控制通道不需要高并发；设上限是为了让「有人拿这个端口做端口扫描 / 压测」
/// 变成拒绝服务而不是把线程表打满。
pub const MAX_CONNECTIONS: usize = 32;

/// 握手阶段（TLS + `Hello` + 认证）的读写上限
///
/// 业务帧允许长事务（安装、备份），连接建立后是 [`crate::client::IO_TIMEOUT`]；
/// 但握手阶段客户端最多发两帧，允许它占着连接十分钟等于把
/// [`MAX_CONNECTIONS`] 的槽位白送给「连上不发数据」的扫描器。
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 从任意线程请求服务端停止的句柄
///
/// `Handler` 在会话线程里跑，业务侧要能「处理完这一帧就退出」（`Shutdown`）。
/// 把停止能力做成可克隆的独立值，而不是让业务持有 [`Server`]：`Server` 是
/// `run()` 的消费方，把它交给业务会让所有权关系反向。
#[derive(Debug, Clone)]
pub struct StopHandle {
    stop: Arc<AtomicBool>,
    addr: SocketAddr,
}

impl StopHandle {
    /// 置停止标志并唤醒监听循环
    ///
    /// `accept` 是阻塞调用，光置标志要等下一次连接才会被看到。这里置位后
    /// 自连一次把 `accept` 踢醒：连接建立后循环先看到标志再退出，不会
    /// 进入会话状态机。相比「另起线程 join」或「给 listener 设超时轮询」，
    /// 这条路径没有额外线程、也没有轮询延迟。
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.addr);
    }

    /// 是否已请求停止
    #[must_use]
    pub fn is_stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
}

/// 服务端
pub struct Server {
    listener: TcpListener,
    config: Arc<rustls::ServerConfig>,
    handler: Arc<dyn Handler>,
    stop: Arc<AtomicBool>,
    active: Arc<AtomicUsize>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("addr", &self.listener.local_addr().ok())
            .finish_non_exhaustive()
    }
}

impl Server {
    /// 绑定地址并准备 TLS
    ///
    /// `addr` 支持 `host:port`；`port = 0` 由系统分配（测试与「自动选端口」用）。
    ///
    /// # Errors
    ///
    /// 绑定失败或 TLS 配置失败时返回 [`TransportError`]
    pub fn bind(
        addr: &str,
        identity: &ServerIdentity,
        handler: Arc<dyn Handler>,
    ) -> Result<Self, TransportError> {
        let listener = bind_listener(addr)?;
        let config = tls::server_config(identity)?;
        Ok(Self {
            listener,
            config,
            handler,
            stop: Arc::new(AtomicBool::new(false)),
            active: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// 实际监听地址（端口为 0 时由这里读回真实端口）
    ///
    /// # Errors
    ///
    /// 取地址失败时返回 [`TransportError::Io`]
    pub fn local_addr(&self) -> Result<SocketAddr, TransportError> {
        self.listener
            .local_addr()
            .map_err(|err| TransportError::io("read listener address", err))
    }

    /// 停止标志：置位后 [`Self::run`] 退出（`remote serve --stop` 用）
    ///
    /// 只置位不会唤醒阻塞在 `accept` 上的 [`Self::run`]；要真正停下来用
    /// [`Self::request_stop`]。
    #[must_use]
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }

    /// 请求停止并唤醒监听循环
    ///
    /// `accept` 是阻塞调用，光置标志要等下一次连接才会被看到。这里置位后
    /// 自连一次把 `accept` 踢醒：连接建立后循环先看到标志再退出，不会
    /// 进入会话状态机。相比「另起线程 join」或「给 listener 设超时轮询」，
    /// 这条路径没有额外线程、也没有轮询延迟。
    ///
    /// # Errors
    ///
    /// 取监听地址失败时返回 [`TransportError`]
    pub fn request_stop(&self) -> Result<(), TransportError> {
        self.stop_handle()?.request_stop();
        Ok(())
    }

    /// 可跨线程持有的停止句柄（业务侧 `Shutdown` 用）
    ///
    /// # Errors
    ///
    /// 取监听地址失败时返回 [`TransportError`]
    pub fn stop_handle(&self) -> Result<StopHandle, TransportError> {
        Ok(StopHandle {
            stop: Arc::clone(&self.stop),
            addr: self.local_addr()?,
        })
    }

    /// 当前连接数（诊断用）
    #[must_use]
    pub fn active_connections(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    /// 接受连接直到停止标志置位
    ///
    /// 每条连接一个线程；单条连接的失败只影响它自己（对端断开、帧非法都是
    /// 常态），不会让服务退出。
    ///
    /// # Errors
    ///
    /// 监听套接字本身失败（如被关闭）时返回 [`TransportError`]
    pub fn run(self) -> Result<(), TransportError> {
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            let (stream, peer) = match self.listener.accept() {
                Ok(pair) => pair,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(TransportError::io("accept connection", err)),
            };
            if self.active.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
                tracing::warn!(%peer, "rejecting connection: too many active sessions");
                continue;
            }
            self.active.fetch_add(1, Ordering::Relaxed);
            let config = Arc::clone(&self.config);
            let handler = Arc::clone(&self.handler);
            let active = Arc::clone(&self.active);
            std::thread::spawn(move || {
                if let Err(err) = serve_connection(stream, config, handler.as_ref()) {
                    // 对端断开是常态，只有协议错误值得记一笔
                    if matches!(err, TransportError::Protocol(_)) {
                        tracing::warn!(%peer, %err, "remote session ended with a protocol error");
                    } else {
                        tracing::debug!(%peer, %err, "remote session ended");
                    }
                }
                active.fetch_sub(1, Ordering::Relaxed);
            });
        }
    }

    /// 在已接受的连接上服务一次（测试与单连接场景）
    ///
    /// # Errors
    ///
    /// TLS 或协议失败时返回 [`TransportError`]
    pub fn serve_stream(&self, stream: TcpStream) -> Result<(), TransportError> {
        serve_connection(stream, Arc::clone(&self.config), self.handler.as_ref())
    }
}

/// 绑定监听套接字（`host:port` 逐个尝试，取第一个成功的）
fn bind_listener(addr: &str) -> Result<TcpListener, TransportError> {
    let mut last: Option<std::io::Error> = None;
    for candidate in addr
        .to_socket_addrs()
        .map_err(|err| TransportError::io(format!("resolve {addr}"), err))?
    {
        match TcpListener::bind(candidate) {
            Ok(listener) => return Ok(listener),
            Err(err) => last = Some(err),
        }
    }
    Err(TransportError::io(
        format!("bind {addr}"),
        last.unwrap_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, "no address resolved")
        }),
    ))
}

/// 一条连接上的完整会话
fn serve_connection(
    stream: TcpStream,
    config: Arc<rustls::ServerConfig>,
    handler: &dyn Handler,
) -> Result<(), TransportError> {
    crate::client::configure(&stream)?;
    // 握手阶段用短超时接管 socket 的读写上限；认证完成后再放宽到业务超时。
    // clone 出来的句柄指向同一个 socket，改选项对原连接生效。
    set_timeouts(&stream, HANDSHAKE_TIMEOUT)?;
    let business = stream
        .try_clone()
        .map_err(|err| TransportError::io("clone handshake socket", err))?;

    let connection = ServerConnection::new(config)?;
    let mut session = Session::new(StreamOwned::new(connection, stream));

    say_hello(&mut session, handler)?;
    let Some(session_info) = authenticate(&mut session, handler)? else {
        // 认证被拒或刚完成配对：token 已交付，连接到此为止
        return Ok(());
    };
    set_timeouts(&business, crate::client::IO_TIMEOUT)?;
    dispatch(&mut session, handler, &session_info)
}

/// 覆盖 socket 的读写超时（`client::configure` 之外单独调，用于握手阶段降级）
fn set_timeouts(stream: &TcpStream, timeout: std::time::Duration) -> Result<(), TransportError> {
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|()| stream.set_write_timeout(Some(timeout)))
        .map_err(|err| TransportError::io("configure handshake timeout", err))
}

/// 第一步：`Hello` 交换
///
/// 首帧不是 `Hello` 就断开：允许跳过握手等于允许客户端自选版本。
fn say_hello<S: std::io::Read + std::io::Write>(
    session: &mut Session<S>,
    handler: &dyn Handler,
) -> Result<(), TransportError> {
    let greeting = handler.greeting();
    match session.recv_request()? {
        Request::Hello {
            protocol_version, ..
        } => {
            if let Err(message) = remote::check_protocol_version(protocol_version) {
                let _ = session.send_response(&Response::Error {
                    kind: ErrorKind::PROTOCOL.to_owned(),
                    message,
                });
                return Err(TransportError::Protocol(
                    "client protocol version mismatch".to_owned(),
                ));
            }
        }
        other => {
            let _ = session.send_response(
                &RemoteFailure::new(
                    ErrorKind::PROTOCOL,
                    format!("expected hello, got {}", other.operation()),
                )
                .into_response(),
            );
            return Err(TransportError::Protocol(
                "first frame was not hello".to_owned(),
            ));
        }
    }
    session.send_response(&Response::Hello {
        protocol_version: remote::REMOTE_PROTOCOL_VERSION,
        agent_version: greeting.agent_version,
        server_name: greeting.server_name,
        capabilities: greeting.capabilities,
        fingerprint: greeting.fingerprint.to_string(),
        has_clients: greeting.has_clients,
    })
}

/// 第二步：`Enroll` 或 `Authenticate`
///
/// 返回 `None` 表示这条连接不该继续：配对已交付 token，或认证被拒。
fn authenticate<S: std::io::Read + std::io::Write>(
    session: &mut Session<S>,
    handler: &dyn Handler,
) -> Result<Option<SessionInfo>, TransportError> {
    let info = match session.recv_request()? {
        Request::Enroll {
            pairing_code,
            client_name,
            read_only,
        } => {
            let enrolled = match handler.enroll(&pairing_code, &client_name, read_only) {
                Ok(enrolled) => enrolled,
                Err(failure) => {
                    let _ = session.send_response(&failure.into_response());
                    return Ok(None);
                }
            };
            session.send_response(&Response::Enrolled {
                token: enrolled.token,
                client_id: enrolled.client_id.as_str().to_owned(),
                role: enrolled.role,
            })?;
            // 配对是一次性动作：token 已交付，后续命令用新连接认证。
            // 不在同一连接上继续，是为了让「配对」与「日常」走完全相同的
            // 认证路径，少一条只在首次生效的分支。
            return Ok(None);
        }
        Request::Authenticate { token } => match handler.authenticate(&token) {
            Ok(info) => info,
            Err(failure) => {
                let _ = session.send_response(&failure.into_response());
                return Ok(None);
            }
        },
        other => {
            let _ = session.send_response(
                &RemoteFailure::new(
                    ErrorKind::UNAUTHORIZED,
                    format!("expected enroll or authenticate, got {}", other.operation()),
                )
                .into_response(),
            );
            return Ok(None);
        }
    };
    session.send_response(&Response::Authenticated {
        client_id: info.client_id.as_str().to_owned(),
        role: info.role,
    })?;
    Ok(Some(info))
}

/// 把任务事件写成中间帧的观察者
///
/// 连接线程同一时刻只处理一个请求，`Mutex` 只是为了让观察者满足
/// [`port::TaskObserver`] 的 `Sync` 约束——不存在并发写同一条连接的场景。
struct SessionSink<'a, S: std::io::Read + std::io::Write> {
    session: std::sync::Mutex<&'a mut Session<S>>,
    /// 上次已发出的进度：下载回调按块触发，逐块发帧会把连接与客户端都打满
    progress: std::sync::Mutex<ProgressMark>,
}

/// 进度帧的节流状态
#[derive(Debug, Default)]
struct ProgressMark {
    done: u64,
    at: Option<std::time::Instant>,
}

/// 进度帧最小间隔与最小增量
///
/// 下载器每块都回调一次（8–64 KiB），逐块发一条 JSON 帧会让「进度条比下载还慢」。
/// 阶段与输出不节流：它们的频率本来就很低，且是用户判断「有没有卡死」的依据。
const PROGRESS_MIN_BYTES: u64 = 128 * 1024;
const PROGRESS_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

impl<S: std::io::Read + std::io::Write> SessionSink<'_, S> {
    /// 尽力写出一帧：事件是旁路，写失败（对端已断开）不改变业务结果
    fn send(&self, event: TaskEvent) {
        let Ok(mut session) = self.session.lock() else {
            return;
        };
        let _ = session.send_response(&Response::Task { event });
    }

    /// 是否值得为这次进度发一帧
    fn should_emit(&self, done: u64, total: Option<u64>) -> bool {
        // 走完（或已知总量且已到终点）必须发：否则进度条停在 99%
        if total == Some(done) {
            return true;
        }
        let Ok(mut mark) = self.progress.lock() else {
            return false;
        };
        let now = std::time::Instant::now();
        let enough_bytes = done.saturating_sub(mark.done) >= PROGRESS_MIN_BYTES;
        let enough_time = mark
            .at
            .is_none_or(|at| now.duration_since(at) >= PROGRESS_MIN_INTERVAL);
        if enough_bytes || enough_time {
            mark.done = done;
            mark.at = Some(now);
            true
        } else {
            false
        }
    }
}

impl<S: std::io::Read + std::io::Write + Send> port::ProgressSink for SessionSink<'_, S> {
    fn on_progress(&self, done: u64, total: Option<u64>) {
        if self.should_emit(done, total) {
            self.send(TaskEvent::Progress { done, total });
        }
    }
}

impl<S: std::io::Read + std::io::Write + Send> port::InstallerOutputSink for SessionSink<'_, S> {
    fn on_output(&self, line: &str) {
        self.send(TaskEvent::Output {
            line: line.to_owned(),
        });
    }
}

impl<S: std::io::Read + std::io::Write + Send> port::TaskObserver for SessionSink<'_, S> {
    fn on_stage(&self, stage: TaskStage) {
        self.send(TaskEvent::Stage { stage });
    }
}

/// 第三步：业务帧一问一答，直到对端关闭
fn dispatch<S: std::io::Read + std::io::Write + Send>(
    session: &mut Session<S>,
    handler: &dyn Handler,
    session_info: &SessionInfo,
) -> Result<(), TransportError> {
    loop {
        let request = match session.recv_request() {
            Ok(request) => request,
            // 正常关闭：对端发完就断
            Err(TransportError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::UnexpectedEof
                    || source.kind() == std::io::ErrorKind::ConnectionReset =>
            {
                return Ok(());
            }
            Err(err) => return Err(err),
        };
        let is_shutdown = matches!(request, Request::Shutdown);
        // 每条业务帧前重查会话有效性：撤销 / 过期必须在**下一帧**就生效，
        // 而不是等连接自然断开
        if !handler.still_authorized(session_info) {
            let _ = session.send_response(
                &RemoteFailure::unauthorized("session revoked or expired").into_response(),
            );
            return Ok(());
        }
        let response = {
            // 观察者只在这次调用期间借用连接：同一线程内不会有第二个写者，
            // 放进 `Mutex` 只是为了满足 `TaskObserver: Sync`
            let sink = SessionSink {
                session: std::sync::Mutex::new(&mut *session),
                progress: std::sync::Mutex::new(ProgressMark::default()),
            };
            if session_info.permits(&request) {
                handler.handle_with(session_info, &request, &sink)
            } else {
                RemoteFailure::unauthorized(format!(
                    "role {} cannot perform {}",
                    session_info.role,
                    request.operation()
                ))
                .into_response()
            }
        };
        let ok = !matches!(response, Response::Error { .. });
        session.send_response(&response)?;
        if is_shutdown && ok {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::{Enrolled, Greeting, SessionInfo};
    use crate::identity::ServerIdentity;
    use auth::{ClientId, Role};
    use protocol::remote::Capabilities;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// 内存版 handler：不碰磁盘，只验证传输与状态机
    struct TestHandler {
        token: String,
        fingerprint: crate::fingerprint::Fingerprint,
        seen: Mutex<Vec<String>>,
    }

    impl TestHandler {
        /// 指纹必须与 TLS 层出示的证书一致，否则客户端的「双重校验」会
        /// 把测试里的假指纹当成中间人
        fn new(fingerprint: crate::fingerprint::Fingerprint) -> Self {
            Self {
                token: "a".repeat(64),
                fingerprint,
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    impl Handler for TestHandler {
        fn greeting(&self) -> Greeting {
            Greeting {
                agent_version: "0.1.0".to_owned(),
                server_name: "test-box".to_owned(),
                capabilities: Capabilities::all(),
                fingerprint: self.fingerprint.clone(),
                has_clients: true,
            }
        }

        fn enroll(
            &self,
            code: &str,
            _client_name: &str,
            read_only: bool,
        ) -> Result<Enrolled, RemoteFailure> {
            if code == "1234-5678" {
                Ok(Enrolled {
                    token: self.token.clone(),
                    client_id: ClientId::generate(),
                    role: if read_only {
                        Role::Viewer
                    } else {
                        Role::Operator
                    },
                })
            } else {
                Err(RemoteFailure::unauthorized("bad pairing code"))
            }
        }

        fn authenticate(&self, token: &str) -> Result<SessionInfo, RemoteFailure> {
            if token == self.token {
                Ok(SessionInfo {
                    client_id: ClientId::generate(),
                    role: Role::Operator,
                })
            } else {
                Err(RemoteFailure::unauthorized("bad token"))
            }
        }

        fn handle(&self, _session: &SessionInfo, request: &Request) -> Response {
            if let Ok(mut seen) = self.seen.lock() {
                seen.push(request.operation().to_owned());
            }
            match request {
                Request::Ping => Response::Pong {
                    agent_version: "0.1.0".to_owned(),
                    uptime_secs: 1,
                },
                Request::ServerList => Response::Servers { items: Vec::new() },
                Request::Shutdown => Response::Ok,
                other => RemoteFailure::invalid(format!("unsupported: {}", other.operation()))
                    .into_response(),
            }
        }

        /// `Ping` 顺带回两条进度：验证「中间帧在前、终结帧在后」的线序
        fn handle_with(
            &self,
            session: &SessionInfo,
            request: &Request,
            progress: &dyn port::TaskObserver,
        ) -> Response {
            if matches!(request, Request::Ping) {
                progress.on_stage(TaskStage::Downloading);
                progress.on_progress(0, None);
                progress.on_progress(100, Some(100));
            }
            self.handle(session, request)
        }
    }

    /// 记录收到的任务事件，用于断言「中间帧在前、终结帧在后」
    #[derive(Default)]
    struct Recorder {
        seen: Mutex<Vec<TaskEvent>>,
    }

    impl port::ProgressSink for Recorder {
        fn on_progress(&self, done: u64, total: Option<u64>) {
            self.seen
                .lock()
                .expect("lock")
                .push(TaskEvent::Progress { done, total });
        }
    }

    impl port::InstallerOutputSink for Recorder {}

    impl port::TaskObserver for Recorder {
        fn on_stage(&self, stage: TaskStage) {
            self.seen
                .lock()
                .expect("lock")
                .push(TaskEvent::Stage { stage });
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mcsup-server-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    /// 起一个真实 TLS 服务端，用真实客户端跑通「配对 → 认证 → 业务帧」
    ///
    /// 这是 transport 的验收线：TLS、指纹固定、帧协议、认证状态机四者必须
    /// 一起工作，任何一环单独测都测不出「连不上」。
    #[test]
    fn real_tls_roundtrip_with_enroll_and_ping() {
        let dir = temp_dir("roundtrip");
        let identity = ServerIdentity::load_or_create(&dir).expect("identity");
        let fingerprint = identity.fingerprint().clone();
        let handler = Arc::new(TestHandler::new(identity.fingerprint().clone()));
        let server = Server::bind("127.0.0.1:0", &identity, handler.clone()).expect("bind");
        let addr = server.local_addr().expect("addr");
        let stop = server.stop_flag();
        let join = std::thread::spawn(move || server.run());

        let client = crate::client::Client::new(addr.to_string(), fingerprint.clone());
        let greeting = client.hello().expect("hello");
        assert_eq!(greeting.server_name, "test-box");
        assert_eq!(greeting.fingerprint, fingerprint);

        // 错码被拒
        let err = client
            .enroll("0000-0000", "laptop", false)
            .expect_err("bad code");
        assert!(matches!(err, TransportError::Remote { .. }), "{err:?}");

        // 正确配对
        let enrolled = client.enroll("1234-5678", "laptop", false).expect("enroll");
        assert_eq!(enrolled.role, Role::Operator);

        // 用 token 建新连接做业务
        let authed = crate::client::Client::new(addr.to_string(), fingerprint.clone())
            .with_token(enrolled.token);
        let info = authed.authenticate().expect("authenticate");
        assert_eq!(info.role, Role::Operator);
        let response = authed.call(&Request::Ping).expect("ping");
        assert!(matches!(response, Response::Pong { .. }), "{response:?}");
        // 同一连接复用：第二次调用不应重新握手（handler 只记录业务帧）
        let response = authed.call(&Request::ServerList).expect("list");
        assert!(matches!(response, Response::Servers { .. }), "{response:?}");

        // 错的 token 被拒
        let bad =
            crate::client::Client::new(addr.to_string(), fingerprint).with_token("b".repeat(64));
        let err = bad.call(&Request::Ping).expect_err("bad token");
        assert!(matches!(err, TransportError::Remote { .. }), "{err:?}");

        stop.store(true, Ordering::Relaxed);
        // 唤醒 accept：连一次即可（服务端会先看到停止标志并退出）
        let _ = TcpStream::connect(addr);
        let _ = join.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 中间帧必须先于终结帧到达，并被交给观察者；不带观察者的 `call` 也要能吃掉它们
    ///
    /// 这条线是远端进度的根：漏掉「读到 Task 继续读」，安装会被当成「对端回了
    /// 一条进度」而提前结束；漏掉节流之外的转发，用户看到的就是空白等待。
    #[test]
    fn progress_frames_precede_the_final_response() {
        let dir = temp_dir("progress");
        let identity = ServerIdentity::load_or_create(&dir).expect("identity");
        let fingerprint = identity.fingerprint().clone();
        let handler = Arc::new(TestHandler::new(identity.fingerprint().clone()));
        let server = Server::bind("127.0.0.1:0", &identity, handler).expect("bind");
        let addr = server.local_addr().expect("addr");
        let stop = server.stop_flag();
        let join = std::thread::spawn(move || server.run());

        let authed = crate::client::Client::new(addr.to_string(), fingerprint.clone())
            .with_token("a".repeat(64));
        let recorder = Recorder::default();
        let response = authed
            .call_streaming(&Request::Ping, &recorder)
            .expect("streaming ping");
        assert!(
            matches!(response, Response::Pong { .. }),
            "终结帧必须是 Pong，不能把进度帧当结果：{response:?}"
        );
        assert_eq!(
            recorder.seen.lock().expect("lock").as_slice(),
            [
                TaskEvent::Stage {
                    stage: TaskStage::Downloading
                },
                TaskEvent::Progress {
                    done: 0,
                    total: None
                },
                TaskEvent::Progress {
                    done: 100,
                    total: Some(100)
                },
            ]
        );

        // 不带观察者的调用方不受影响：中间帧被丢弃，仍然拿到终结帧
        let response = authed.call(&Request::Ping).expect("plain ping");
        assert!(matches!(response, Response::Pong { .. }), "{response:?}");

        stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(addr);
        let _ = join.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 指纹不符必须在 TLS 握手阶段就失败，不能连上之后再拒
    #[test]
    fn wrong_fingerprint_fails_the_handshake() {
        let dir = temp_dir("pin");
        let identity = ServerIdentity::load_or_create(&dir).expect("identity");
        let handler = Arc::new(TestHandler::new(identity.fingerprint().clone()));
        let server = Server::bind("127.0.0.1:0", &identity, handler).expect("bind");
        let addr = server.local_addr().expect("addr");
        let stop = server.stop_flag();
        let join = std::thread::spawn(move || server.run());

        let wrong = crate::fingerprint::Fingerprint::of_der(b"someone else's cert");
        let client = crate::client::Client::new(addr.to_string(), wrong);
        let err = client.hello().expect_err("fingerprint mismatch");
        assert!(
            matches!(err, TransportError::Io { .. } | TransportError::Protocol(_)),
            "{err:?}"
        );

        stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(addr);
        let _ = join.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 首帧不是 Hello 必须被拒：否则认证变成可选
    #[test]
    fn business_frame_before_hello_is_rejected() {
        let dir = temp_dir("order");
        let identity = ServerIdentity::load_or_create(&dir).expect("identity");
        let handler = Arc::new(TestHandler::new(identity.fingerprint().clone()));
        let server = Server::bind("127.0.0.1:0", &identity, handler).expect("bind");
        let addr = server.local_addr().expect("addr");
        let stop = server.stop_flag();
        let join = std::thread::spawn(move || server.run());

        let stream = TcpStream::connect(addr).expect("connect");
        let config = tls::client_config(identity.fingerprint()).expect("client config");
        let name = rustls_pki_types::ServerName::try_from("mcsup.local")
            .expect("name")
            .to_owned();
        let connection = rustls::ClientConnection::new(config, name).expect("conn");
        let mut session = Session::new(StreamOwned::new(connection, stream));
        session.send_request(&Request::Ping).expect("send");
        let response = session.recv_response().expect("recv");
        match response {
            Response::Error { kind, .. } => assert_eq!(kind, ErrorKind::PROTOCOL),
            other => panic!("unexpected: {other:?}"),
        }

        stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(addr);
        let _ = join.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 绑定失败要报错，不能静默挑一个别的端口
    #[test]
    fn bind_failure_is_reported() {
        let dir = temp_dir("bind");
        let identity = ServerIdentity::load_or_create(&dir).expect("identity");
        let handler = Arc::new(TestHandler::new(identity.fingerprint().clone()));
        let server = Server::bind("127.0.0.1:0", &identity, handler.clone()).expect("bind");
        let addr = server.local_addr().expect("addr");
        let err = Server::bind(&addr.to_string(), &identity, handler).expect_err("in use");
        assert!(matches!(err, TransportError::Io { .. }), "{err:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 握手超时必须真的落在 socket 上
    ///
    /// 漏掉这一步时测试不会自己变红，但「连上不发数据」的扫描器能白占
    /// [`MAX_CONNECTIONS`] 槽位直到业务超时（600s）。
    #[test]
    fn handshake_timeout_is_applied_to_the_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let client = TcpStream::connect(addr).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        set_timeouts(&server, HANDSHAKE_TIMEOUT).expect("set timeouts");
        assert_eq!(
            server.read_timeout().expect("read timeout"),
            Some(HANDSHAKE_TIMEOUT)
        );
        drop(client);
    }
}
