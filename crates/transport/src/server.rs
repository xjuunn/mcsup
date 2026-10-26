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
use protocol::remote::{self, ErrorKind, Request, Response};
use rustls::{ServerConnection, StreamOwned};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// 同时允许的连接数上限
///
/// 控制通道不需要高并发；设上限是为了让「有人拿这个端口做端口扫描 / 压测」
/// 变成拒绝服务而不是把线程表打满。
pub const MAX_CONNECTIONS: usize = 32;

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
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Ok(addr) = self.listener.local_addr() {
            let _ = TcpStream::connect(addr);
        }
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
    let connection = ServerConnection::new(config)?;
    let mut session = Session::new(StreamOwned::new(connection, stream));

    say_hello(&mut session, handler)?;
    let Some(session_info) = authenticate(&mut session, handler)? else {
        // 认证被拒或刚完成配对：token 已交付，连接到此为止
        return Ok(());
    };
    dispatch(&mut session, handler, &session_info)
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

/// 第三步：业务帧一问一答，直到对端关闭
fn dispatch<S: std::io::Read + std::io::Write>(
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
        let response = if session_info.permits(&request) {
            handler.handle(session_info, &request)
        } else {
            RemoteFailure::unauthorized(format!(
                "role {} cannot perform {}",
                session_info.role,
                request.operation()
            ))
            .into_response()
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
}
