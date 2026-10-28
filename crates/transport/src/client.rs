//! 客户端：按需建连、握手、复用一条连接
//!
//! 「按需连接，无常驻会话」（`docs/remote.md` 设计原则 3）在这里落地：一个
//! [`Client`] 只在第一次调用时建连，之后复用同一条连接；连接断了就重建一次。
//! 不做后台重连、不做心跳——本地不跑守护进程，进程退出连接自然消失。
//!
//! 一次连接的生命周期由 [`Client`] 自己管理：建连 → `Hello` → 认证（有 token
//! 时）→ 业务帧。首次配对走 [`Client::enroll`]，它在同一个连接上完成
//! `Hello` + `Enroll`，拿到的 token 由调用方落盘。

use crate::error::TransportError;
use crate::fingerprint::Fingerprint;
use crate::handler::{Enrolled, Greeting, SessionInfo};
use crate::session::Session;
use crate::tls;
use protocol::remote::{self, Request, Response};
use rustls::{ClientConfig, ClientConnection, StreamOwned};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 建连超时：局域网内建连失败通常立刻返回，这个上限只兜「对端不响应」
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 单次请求的读写超时
///
/// 远端可能在装核心、扫 Java（分钟级），因此这个值必须够宽；但它不能是
/// 无限——没有超时的同步读会让 `mcsup` 卡死在一条半死的连接上。
pub const IO_TIMEOUT: Duration = Duration::from_secs(600);

/// 一条已握手的客户端连接
type Conn = Session<StreamOwned<ClientConnection, TcpStream>>;

/// 首次配对前的探测结果：对端证书指纹 + 握手信息
///
/// 指纹来自 TLS 握手本身（不是对端自报），用户确认后用它构造固定指纹的
/// [`Client`]；对端在 `Hello` 里自报的指纹必须与之一致，否则按中间人拒绝。
#[derive(Debug, Clone)]
pub struct Probe {
    /// TLS 证书指纹
    pub fingerprint: Fingerprint,
    /// 握手信息（名称 / 版本 / 能力 / 是否已有客户端）
    pub greeting: Greeting,
}

/// 远端客户端
pub struct Client {
    /// 连接目标（`host:port`）
    target: String,
    /// 固定的服务端证书指纹
    fingerprint: Fingerprint,
    /// 已保存的 token（首次配对前为 `None`）
    token: Option<String>,
    /// 当前连接；断线后置空
    conn: Mutex<Option<Conn>>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("target", &self.target)
            .field("fingerprint", &self.fingerprint)
            .field("authenticated", &self.token.is_some())
            .finish_non_exhaustive()
    }
}

impl Client {
    /// 构造（不建连）
    #[must_use]
    pub fn new(target: impl Into<String>, fingerprint: Fingerprint) -> Self {
        Self {
            target: target.into(),
            fingerprint,
            token: None,
            conn: Mutex::new(None),
        }
    }

    /// 带上已保存的 token
    #[must_use]
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// 连接目标
    #[must_use]
    pub fn target(&self) -> &str {
        &self.target
    }

    /// 固定的指纹
    #[must_use]
    pub fn fingerprint(&self) -> &Fingerprint {
        &self.fingerprint
    }

    /// 主动断开（下次调用会重建）
    ///
    /// # Errors
    ///
    /// 内部锁被 poison 时返回 [`TransportError::Protocol`]
    pub fn disconnect(&self) -> Result<(), TransportError> {
        let mut guard = self.lock()?;
        *guard = None;
        Ok(())
    }

    /// 首次配对前的探测：建连、捕获对端证书指纹并读取握手信息
    ///
    /// 不认证、不写任何东西。调用方把 [`Probe::fingerprint`] 展示给用户，
    /// 确认后再用固定指纹的 [`Client`] 完成 `Enroll`。
    ///
    /// # Errors
    ///
    /// 建连、TLS 或协议失败时返回 [`TransportError`]
    pub fn probe(target: &str) -> Result<Probe, TransportError> {
        let captured: Arc<Mutex<Option<Fingerprint>>> = Arc::new(Mutex::new(None));
        let config = tls::client_config_capture(Arc::clone(&captured))?;
        let mut conn = connect_with(target, &config)?;
        let greeting = match conn.exchange(&hello_request())? {
            Response::Hello {
                protocol_version,
                agent_version,
                server_name,
                capabilities,
                fingerprint,
                has_clients,
            } => {
                remote::check_protocol_version(protocol_version)
                    .map_err(TransportError::Protocol)?;
                let declared = Fingerprint::parse(&fingerprint)?;
                let actual = captured_fingerprint(&captured)?;
                if declared != actual {
                    return Err(TransportError::FingerprintMismatch {
                        expected: actual.to_string(),
                        actual: declared.to_string(),
                    });
                }
                Greeting {
                    agent_version,
                    server_name,
                    capabilities,
                    fingerprint: actual,
                    has_clients,
                }
            }
            Response::Error { kind, message } => {
                return Err(TransportError::Remote { kind, message });
            }
            other => {
                return Err(TransportError::Protocol(format!(
                    "expected hello, got {}",
                    response_name(&other)
                )));
            }
        };
        Ok(Probe {
            fingerprint: captured_fingerprint(&captured)?,
            greeting,
        })
    }

    /// 握手并取对端信息（不需要认证）
    ///
    /// # Errors
    ///
    /// 建连、TLS 或协议失败时返回 [`TransportError`]
    pub fn hello(&self) -> Result<Greeting, TransportError> {
        let mut conn = self.connect()?;
        let response = conn.exchange(&hello_request())?;
        match response {
            Response::Hello {
                protocol_version,
                agent_version,
                server_name,
                capabilities,
                fingerprint,
                has_clients,
            } => {
                remote::check_protocol_version(protocol_version)
                    .map_err(TransportError::Protocol)?;
                // 对端自报的指纹必须与 TLS 层固定的一致：不一致说明对端在
                // 证书之外又声明了另一个身份，两边必有一处在说谎
                let declared = Fingerprint::parse(&fingerprint)?;
                if declared != self.fingerprint {
                    return Err(TransportError::FingerprintMismatch {
                        expected: self.fingerprint.to_string(),
                        actual: declared.to_string(),
                    });
                }
                Ok(Greeting {
                    agent_version,
                    server_name,
                    capabilities,
                    fingerprint: declared,
                    has_clients,
                })
            }
            Response::Error { kind, message } => Err(TransportError::Remote { kind, message }),
            other => Err(TransportError::Protocol(format!(
                "expected hello, got {}",
                response_name(&other)
            ))),
        }
    }

    /// 用配对码换 token
    ///
    /// # Errors
    ///
    /// 建连失败或对端拒绝配对时返回 [`TransportError`]
    pub fn enroll(
        &self,
        pairing_code: &str,
        client_name: &str,
        read_only: bool,
    ) -> Result<Enrolled, TransportError> {
        let mut conn = self.connect()?;
        expect_hello(conn.exchange(&hello_request())?)?;
        let response = conn.exchange(&Request::Enroll {
            pairing_code: pairing_code.to_owned(),
            client_name: client_name.to_owned(),
            read_only,
        })?;
        match response {
            Response::Enrolled {
                token,
                client_id,
                role,
            } => Ok(Enrolled {
                token,
                client_id: auth::ClientId::parse(&client_id).map_err(|err| {
                    TransportError::Protocol(format!("invalid client id from peer: {err}"))
                })?,
                role,
            }),
            Response::Error { kind, message } => Err(TransportError::Remote { kind, message }),
            other => Err(TransportError::Protocol(format!(
                "expected enrolled, got {}",
                response_name(&other)
            ))),
        }
    }

    /// 认证当前连接（新建一条连接做握手 + 认证）
    ///
    /// # Errors
    ///
    /// 没有 token、建连失败或 token 被拒时返回 [`TransportError`]
    pub fn authenticate(&self) -> Result<SessionInfo, TransportError> {
        let token = self.token()?;
        let mut conn = self.connect()?;
        handshake(&mut conn, &token)
    }

    /// 执行一个操作（自动握手 + 认证）
    ///
    /// 复用中的连接断了会重建一次再重试：远端在两次命令之间重启是常态，把
    /// 「重启」表现成一次失败会让每个脚本都要自己重试。
    ///
    /// **建连本身就失败时不重试**：没有「断掉的连接」可重建，第二次尝试只会
    /// 把等待翻倍（不可达远端一次 SYN 超时已经够久），而结果不会变好。
    ///
    /// # Errors
    ///
    /// 建连、认证失败或对端返回错误时返回 [`TransportError`]
    pub fn call(&self, request: &Request) -> Result<Response, TransportError> {
        let reused = self.has_connection();
        match self.call_once(request) {
            Ok(response) => Ok(response),
            Err(err) if should_retry(&err, reused) => {
                tracing::debug!(%err, "remote connection dropped, retrying once");
                self.disconnect()?;
                self.call_once(request)
            }
            Err(err) => Err(err),
        }
    }

    /// 连接池里是否已有一条连接（只窥探，不建连）
    ///
    /// 用来区分「复用中的连接断了」与「压根没连上」：只有前者值得重试。
    /// 锁中毒按「没有连接」处理——那种情况下重试也修不好。
    fn has_connection(&self) -> bool {
        self.conn.lock().is_ok_and(|guard| guard.is_some())
    }

    /// 执行一个操作，不做重试
    ///
    /// # Errors
    ///
    /// 同 [`Client::call`]
    pub fn call_once(&self, request: &Request) -> Result<Response, TransportError> {
        let token = self.token()?;
        let mut guard = self.lock()?;
        let mut conn = if let Some(conn) = guard.take() {
            conn
        } else {
            let mut conn = self.connect()?;
            handshake(&mut conn, &token)?;
            conn
        };
        match conn.exchange(request) {
            Ok(response) => {
                *guard = Some(conn);
                Ok(response)
            }
            Err(err) => {
                // 连接状态未知，直接丢弃；调用方决定是否重试
                drop(guard);
                Err(err)
            }
        }
    }

    /// 已保存的 token
    fn token(&self) -> Result<String, TransportError> {
        self.token
            .clone()
            .ok_or_else(|| TransportError::Unauthorized("no token stored for this remote".into()))
    }

    /// 建一条新连接（已完成 TLS 握手）
    fn connect(&self) -> Result<Conn, TransportError> {
        let config = tls::client_config(&self.fingerprint)?;
        connect_with(&self.target, &config)
    }

    /// 取连接锁
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Option<Conn>>, TransportError> {
        self.conn
            .lock()
            .map_err(|_| TransportError::Protocol("client connection lock poisoned".to_owned()))
    }
}

/// 按目标地址与 TLS 配置建一条已握手的连接
fn connect_with(target: &str, config: &Arc<ClientConfig>) -> Result<Conn, TransportError> {
    let addr = target
        .to_socket_addrs()
        .map_err(|err| TransportError::io(format!("resolve {target}"), err))?
        .next()
        .ok_or_else(|| {
            TransportError::io(
                format!("resolve {target}"),
                std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, "no address resolved"),
            )
        })?;
    let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
        .map_err(|err| TransportError::io(format!("connect {target}"), err))?;
    configure(&stream)?;

    let server_name = rustls_pki_types::ServerName::try_from("mcsup.local")
        .map_err(|err| TransportError::Protocol(format!("server name: {err}")))?
        .to_owned();
    let connection = ClientConnection::new(Arc::clone(config), server_name)?;
    Ok(Session::new(StreamOwned::new(connection, stream)))
}

/// 取 TLS 握手期间捕获的指纹
fn captured_fingerprint(
    captured: &Arc<Mutex<Option<Fingerprint>>>,
) -> Result<Fingerprint, TransportError> {
    captured
        .lock()
        .map_err(|_| TransportError::Protocol("fingerprint lock poisoned".to_owned()))?
        .clone()
        .ok_or_else(|| TransportError::Protocol("peer did not present a certificate".to_owned()))
}

/// 客户端首帧
fn hello_request() -> Request {
    Request::Hello {
        protocol_version: remote::REMOTE_PROTOCOL_VERSION,
        client_version: env!("CARGO_PKG_VERSION").to_owned(),
    }
}

/// `Hello` + `Authenticate`：新连接必须先过这两步
fn handshake(conn: &mut Conn, token: &str) -> Result<SessionInfo, TransportError> {
    expect_hello(conn.exchange(&hello_request())?)?;
    match conn.exchange(&Request::Authenticate {
        token: token.to_owned(),
    })? {
        Response::Authenticated { client_id, role } => Ok(SessionInfo {
            client_id: auth::ClientId::parse(&client_id).map_err(|err| {
                TransportError::Protocol(format!("invalid client id from peer: {err}"))
            })?,
            role,
        }),
        Response::Error { kind, message } => Err(TransportError::Remote { kind, message }),
        other => Err(TransportError::Protocol(format!(
            "expected authenticated, got {}",
            response_name(&other)
        ))),
    }
}

/// 断言握手帧合法
fn expect_hello(response: Response) -> Result<(), TransportError> {
    match response {
        Response::Hello {
            protocol_version, ..
        } => remote::check_protocol_version(protocol_version).map_err(TransportError::Protocol),
        Response::Error { kind, message } => Err(TransportError::Remote { kind, message }),
        other => Err(TransportError::Protocol(format!(
            "expected hello, got {}",
            response_name(&other)
        ))),
    }
}

/// 响应的帧名（错误信息里用，不打印载荷——载荷可能含 token）
fn response_name(response: &Response) -> &'static str {
    match response {
        Response::Hello { .. } => "hello",
        Response::Enrolled { .. } => "enrolled",
        Response::Authenticated { .. } => "authenticated",
        Response::Pong { .. } => "pong",
        Response::Ok => "ok",
        Response::Workspaces { .. } => "workspaces",
        Response::Servers { .. } => "servers",
        Response::Javas { .. } => "javas",
        Response::Clients { .. } => "clients",
        Response::Statuses { .. } => "statuses",
        Response::Started { .. } => "started",
        Response::Stopped { .. } => "stopped",
        Response::LogLines { .. } => "log_lines",
        Response::Removed { .. } => "removed",
        Response::Props { .. } => "props",
        Response::SettingsChanged { .. } => "settings_changed",
        Response::ModpackApplied { .. } => "modpack_applied",
        Response::JavaUninstalled { .. } => "java_uninstalled",
        Response::SnapshotCreated { .. } => "snapshot_created",
        Response::Snapshots { .. } => "snapshots",
        Response::Restored { .. } => "restored",
        Response::SnapshotDeleted { .. } => "snapshot_deleted",
        Response::SnapshotVerified { .. } => "snapshot_verified",
        Response::Pruned { .. } => "pruned",
        Response::Error { .. } => "error",
    }
}

/// 给一条已连接的套接字设超时与 `nodelay`
pub(crate) fn configure(stream: &TcpStream) -> Result<(), TransportError> {
    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(IO_TIMEOUT)))
        .and_then(|()| stream.set_nodelay(true))
        .map_err(|err| TransportError::io("configure socket", err))
}

/// 是否值得重试：只有「复用中的连接断了」才值得
///
/// `reused` 为 false 表示这次调用连连接都没建起来：没有可重建的断连，重试
/// 只是把等待时间翻倍（对端不可达时一次建连超时已经够久），结果不会变好。
fn should_retry(err: &TransportError, reused: bool) -> bool {
    reused && err.is_retryable()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_keeps_target_and_fingerprint() {
        let fp = Fingerprint::of_der(b"cert");
        let client = Client::new("127.0.0.1:1", fp.clone());
        assert_eq!(client.target(), "127.0.0.1:1");
        assert_eq!(client.fingerprint(), &fp);
        assert!(client.token.is_none());
    }

    /// 没有 token 时不能发业务请求：错误必须是「未授权」而不是「连接失败」
    #[test]
    fn call_without_token_is_unauthorized() {
        let client = Client::new("127.0.0.1:1", Fingerprint::of_der(b"cert"));
        let err = client.call(&Request::Ping).expect_err("no token");
        assert!(matches!(err, TransportError::Unauthorized(_)), "{err:?}");
        assert_eq!(err.kind(), "unauthorized");
        assert!(!err.is_retryable());
    }

    #[test]
    fn connect_failure_is_retryable_io() {
        // 端口 1 上不会有服务；建连应立即失败
        let client =
            Client::new("127.0.0.1:1", Fingerprint::of_der(b"cert")).with_token("a".repeat(64));
        let err = client.call(&Request::Ping).expect_err("refused");
        assert!(matches!(err, TransportError::Io { .. }), "{err:?}");
        assert!(err.is_retryable());
    }

    /// 建连失败不重试：没有断掉的连接可重建，重试只会把等待翻倍
    #[test]
    fn retry_needs_an_existing_connection() {
        let refused = TransportError::io(
            "connect 127.0.0.1:1",
            std::io::Error::from(std::io::ErrorKind::ConnectionRefused),
        );
        assert!(refused.is_retryable(), "建连失败本身是可重试类别");
        assert!(!should_retry(&refused, false), "没连上过就不重试");
        assert!(should_retry(&refused, true), "复用中的连接断了才重试");
    }

    /// 不可重试的错误（未授权 / 协议）无论如何都不重试
    #[test]
    fn non_retryable_errors_are_never_retried() {
        let unauthorized = TransportError::Unauthorized("no token".to_owned());
        assert!(!should_retry(&unauthorized, true));
        assert!(!should_retry(&unauthorized, false));
    }

    #[test]
    fn response_names_are_stable() {
        assert_eq!(response_name(&Response::Ok), "ok");
        assert_eq!(
            response_name(&Response::Error {
                kind: "io".to_owned(),
                message: String::new()
            }),
            "error"
        );
    }
}
