//! 控制通道：CLI / server / agent 与宿主之间的本机连接
//!
//! 端点优先 Unix socket（文件权限即认证），路径过长或非 Unix 平台回退回环
//! TCP；TCP 必须携带 `run/control-token`（0600）里的一次性令牌。`[禁止]`
//! 监听非回环地址。
//!
//! 帧形状复用 `protocol::plugin`：`call` 的 `method` 是控制方法名。服务端
//! 一次连接可连续处理多帧；客户端默认一次调用一条连接，避免共享可变状态。
//!
//! 管理命令（install / remove / rollback）在 CLI 进程内完成文件与记录操作，
//! 然后调用 `reload`；`[禁止]` 由宿主代为下载或解包。

use crate::conn::Connection;
use domain::plugin::PluginId;
use domain::plugin::capability::Capability;
use domain::plugin::state::PluginState;
use port::plugin::{
    AuditEvent, AuditResult, AuditSink, ConfigOps, HostError, PluginRecord, PluginRuntimeStatus,
    PluginStore, PluginSupervisor, SessionInfo, StreamSink,
};
use protocol::plugin::{Call, Frame, ResultFrame, StreamFrame, WireError, read_frame, write_frame};
use serde_json::{Value, json};
use std::io::{self, BufReader, BufWriter, Write};
use std::net::TcpListener;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::net::UnixListener;

#[cfg(windows)]
use interprocess::local_socket::ListenerNonblockingMode;
#[cfg(windows)]
use interprocess::local_socket::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 控制端点文件名（Unix socket）
pub const SOCKET_FILE: &str = "plugin-host.sock";

/// 单实例锁文件名
pub const LOCK_FILE: &str = "plugin-host.lock";

/// TCP 回退时写下的端点描述文件
pub const ENDPOINT_FILE: &str = "plugin-host.endpoint";

/// TCP 回退时的一次性令牌文件（0600）
pub const TOKEN_FILE: &str = "control-token";

/// 控制连接的读写超时
const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// `logs` 单次返回的行数上限
///
/// CLI 的 `--tail` 已夹到同一区间；这里再夹一次是因为控制通道对端不可信：
/// 手工构造一帧 `max: 18446744073709551615` 就能让宿主按行数分配。
const MAX_LOG_PAGE_LINES: usize = 5000;

/// 服务端启动失败
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ControlError {
    /// 已有宿主持有单实例锁
    #[error("another plugin host is already running")]
    AlreadyRunning,
    /// 端点绑定或文件操作失败
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// 控制端点
pub struct ControlServer {
    inner: Listener,
    token: Option<String>,
    #[cfg(unix)]
    socket_path: Option<PathBuf>,
    run_dir: PathBuf,
    shutdown: Arc<AtomicBool>,
    /// 锁句柄：关闭（含进程崩溃）即释放
    _lock: std::fs::File,
}

enum Listener {
    #[cfg(unix)]
    Unix(UnixListener),
    #[cfg(windows)]
    Pipe(interprocess::local_socket::Listener),
    Tcp(TcpListener),
}

impl std::fmt::Debug for ControlServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlServer")
            .field("run_dir", &self.run_dir)
            .field("address", &self.address())
            .finish_non_exhaustive()
    }
}

impl ControlServer {
    /// 绑定控制端点并取得单实例锁
    ///
    /// # Errors
    ///
    /// 已有实例持有锁、目录创建或绑定失败时返回 [`ControlError`]
    pub fn bind(data_dir: &Path) -> Result<Self, ControlError> {
        let run_dir = data_dir.join("run");
        std::fs::create_dir_all(&run_dir)?;
        let lock = acquire_lock(&run_dir.join(LOCK_FILE))?;
        #[cfg(unix)]
        {
            match Self::bind_unix(&run_dir) {
                Ok((listener, path)) => {
                    return Ok(Self {
                        inner: Listener::Unix(listener),
                        token: None,
                        socket_path: Some(path),
                        run_dir,
                        shutdown: Arc::new(AtomicBool::new(false)),
                        _lock: lock,
                    });
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    return Err(ControlError::AlreadyRunning);
                }
                Err(_) => {}
            }
            // 路径超过平台上限：回退回环 TCP，令牌仍然生效。
            let (listener, token) = bind_tcp(&run_dir)?;
            Ok(Self {
                inner: Listener::Tcp(listener),
                token: Some(token),
                socket_path: None,
                run_dir,
                shutdown: Arc::new(AtomicBool::new(false)),
                _lock: lock,
            })
        }
        #[cfg(windows)]
        {
            match Self::bind_pipe(&run_dir) {
                Ok(listener) => {
                    return Ok(Self {
                        inner: Listener::Pipe(listener),
                        token: None,
                        run_dir,
                        shutdown: Arc::new(AtomicBool::new(false)),
                        _lock: lock,
                    });
                }
                Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
                    return Err(ControlError::AlreadyRunning);
                }
                // 管道系统不可用：回退回环 TCP，令牌仍然生效。
                Err(_) => {}
            }
            let (listener, token) = bind_tcp(&run_dir)?;
            Ok(Self {
                inner: Listener::Tcp(listener),
                token: Some(token),
                run_dir,
                shutdown: Arc::new(AtomicBool::new(false)),
                _lock: lock,
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let (listener, token) = bind_tcp(&run_dir)?;
            Ok(Self {
                inner: Listener::Tcp(listener),
                token: Some(token),
                run_dir,
                shutdown: Arc::new(AtomicBool::new(false)),
                _lock: lock,
            })
        }
    }

    #[cfg(unix)]
    fn bind_unix(run_dir: &Path) -> io::Result<(UnixListener, PathBuf)> {
        let path = run_dir.join(SOCKET_FILE);
        if path.as_os_str().len() > 100 {
            // 超过 macOS 的路径上限：交给 TCP 回退
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "socket path is too long",
            ));
        }
        // 上次异常退出可能留下 socket 文件：先确认无人监听。
        if path.exists() {
            match std::os::unix::net::UnixStream::connect(&path) {
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "another plugin host is listening",
                    ));
                }
                Err(_) => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        Ok((listener, path))
    }

    #[cfg(windows)]
    fn bind_pipe(run_dir: &Path) -> io::Result<interprocess::local_socket::Listener> {
        use interprocess::os::windows::local_socket::NamedPipe;
        let name = control_pipe_name(run_dir)
            .to_fs_name::<NamedPipe>()
            .map_err(io::Error::other)?;
        interprocess::local_socket::ListenerOptions::new()
            .name(name)
            .create_sync()
    }

    /// 端点地址（日志与诊断用）
    #[must_use]
    pub fn address(&self) -> String {
        match &self.inner {
            #[cfg(unix)]
            Listener::Unix(_) => self
                .socket_path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
            #[cfg(windows)]
            Listener::Pipe(_) => control_pipe_name(&self.run_dir),
            Listener::Tcp(listener) => listener
                .local_addr()
                .map_or_else(|_| "tcp://?".to_owned(), |addr| format!("tcp://{addr}")),
        }
    }

    /// 数据目录
    #[must_use]
    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    /// 服务直到收到 `shutdown`
    ///
    /// 每条连接独立线程，互不阻塞。返回前不会主动杀掉仍在处理的连接：
    /// 调用方随后执行 [`PluginHost::shutdown_all`](crate::PluginHost::shutdown_all)。
    ///
    /// # Errors
    ///
    /// 接受连接失败时返回 IO 错误
    // 句柄按值传入：调用方交出唯一所有权，内部再按连接克隆
    #[allow(clippy::needless_pass_by_value)]
    pub fn serve(self, handler: Arc<dyn ControlHandler>) -> io::Result<()> {
        let shutdown = Arc::clone(&self.shutdown);
        // 监听器非阻塞：`[禁止]` 靠「再连一次」唤醒阻塞的 accept 才能退出。
        // 25 ms 的轮询只发生在空闲的宿主进程里，代价可以接受。
        self.set_nonblocking(true)?;
        loop {
            if shutdown.load(Ordering::SeqCst) {
                return Ok(());
            }
            let conn = match self.accept() {
                Ok(conn) => conn,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(25));
                    continue;
                }
                Err(err) => return Err(err),
            };
            // 读 / 写超时只在阻塞模式下有意义
            conn.set_nonblocking(false)?;
            let handler = Arc::clone(&handler);
            let token = self.token.clone();
            let connection_shutdown = Arc::clone(&shutdown);
            std::thread::spawn(move || {
                if let Err(err) =
                    serve_connection(conn, token.as_deref(), handler, &connection_shutdown)
                {
                    tracing::debug!(error = %err, "control connection closed");
                }
            });
        }
    }

    fn set_nonblocking(&self, flag: bool) -> io::Result<()> {
        match &self.inner {
            #[cfg(unix)]
            Listener::Unix(listener) => listener.set_nonblocking(flag),
            #[cfg(windows)]
            Listener::Pipe(listener) => listener.set_nonblocking(if flag {
                ListenerNonblockingMode::Accept
            } else {
                ListenerNonblockingMode::Neither
            }),
            Listener::Tcp(listener) => listener.set_nonblocking(flag),
        }
    }

    fn accept(&self) -> io::Result<Connection> {
        match &self.inner {
            #[cfg(unix)]
            Listener::Unix(listener) => {
                let (stream, _) = listener.accept()?;
                Ok(Connection::unix(stream))
            }
            #[cfg(windows)]
            Listener::Pipe(listener) => Ok(Connection::pipe(listener.accept()?)),
            Listener::Tcp(listener) => {
                let (stream, _) = listener.accept()?;
                // 只接受本机回环：绑定地址已是 127.0.0.1，这里再确认对端。
                if !stream.peer_addr().is_ok_and(|addr| addr.ip().is_loopback()) {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "control endpoint only accepts loopback peers",
                    ));
                }
                Ok(Connection::tcp(stream))
            }
        }
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(path) = self.socket_path.take() {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_file(self.run_dir.join(ENDPOINT_FILE));
        let _ = std::fs::remove_file(self.run_dir.join(TOKEN_FILE));
    }
}

/// 控制端点的 Windows 命名管道名
///
/// 与插件端点用不同前缀，避免和插件自身的管道撞名；哈希输入是 `run_dir`，
/// 两端从同一数据目录推出同一名字。
#[cfg(windows)]
#[must_use]
fn control_pipe_name(run_dir: &Path) -> String {
    format!(
        r"\\.\pipe\mcsup-plugin-host-{hash:016x}",
        hash = fnv1a_bytes(run_dir.to_string_lossy().as_bytes())
    )
}

/// FNV-1a（64 位）：无依赖的稳定哈希，仅用于生成管道名
#[cfg(windows)]
fn fnv1a_bytes(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 取得单实例锁
///
/// 锁由内核持有，进程退出自动释放；`[禁止]` 用「文件存在即加锁」。
fn acquire_lock(path: &Path) -> Result<std::fs::File, ControlError> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    match file.try_lock() {
        Ok(()) => {
            let _ = file.set_len(0);
            let _ = writeln!(&file, "{}", std::process::id());
            Ok(file)
        }
        Err(std::fs::TryLockError::WouldBlock) => Err(ControlError::AlreadyRunning),
        Err(std::fs::TryLockError::Error(err)) => Err(ControlError::Io(err)),
    }
}

fn bind_tcp(run_dir: &Path) -> Result<(TcpListener, String), ControlError> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    let token = crate::host::new_token();
    write_private(&run_dir.join(TOKEN_FILE), token.as_bytes())?;
    write_private(
        &run_dir.join(ENDPOINT_FILE),
        format!("tcp://{addr}").as_bytes(),
    )?;
    Ok((listener, token))
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    #[cfg(unix)]
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[allow(clippy::needless_pass_by_value)] // 每个连接线程各持一份克隆
fn serve_connection(
    conn: Connection,
    token: Option<&str>,
    handler: Arc<dyn ControlHandler>,
    shutdown: &AtomicBool,
) -> io::Result<()> {
    conn.set_read_timeout(Some(IO_TIMEOUT))?;
    conn.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut writer = BufWriter::new(conn.try_clone()?);
    let mut reader = BufReader::new(conn);
    if let Some(expected) = token {
        let frame = read_frame(&mut reader).map_err(frame_io)?;
        let authenticated = match frame {
            Frame::Call(call) if call.method.as_deref() == Some("auth") => call
                .input
                .get("token")
                .and_then(Value::as_str)
                .is_some_and(|token| constant_time_eq(token, expected)),
            _ => false,
        };
        if !authenticated {
            let error = HostError::denied("control token is missing or invalid");
            let _ = write_frame(&mut writer, &Frame::Result(error_frame(0, &error)));
            return Ok(());
        }
        write_frame(&mut writer, &Frame::Result(ResultFrame::ok(0, json!({}))))
            .map_err(frame_io)?;
    }
    loop {
        let frame = match read_frame(&mut reader) {
            Ok(frame) => frame,
            Err(protocol::plugin::FrameError::UnexpectedEof) => return Ok(()),
            Err(err) => return Err(frame_io(err)),
        };
        let Frame::Call(call) = frame else {
            // 控制通道只接受 call；未知 / 非调用帧忽略，保持连接。
            continue;
        };
        let id = call.id;
        let Ok(protocol::plugin::CallRoute::Control(method)) = call.route() else {
            let error = HostError::invalid_input("control channel only accepts method calls");
            write_frame(&mut writer, &Frame::Result(error_frame(id, &error))).map_err(frame_io)?;
            continue;
        };
        let route = method.to_owned();
        if route == "stream" {
            let sink = WireStreamSink {
                id,
                seq: AtomicU64::new(0),
                writer: Mutex::new(&mut writer),
            };
            let mut sink = sink;
            let result = dispatch_stream(handler.as_ref(), &call.input, &mut sink);
            let frame = match result {
                Ok(()) => Frame::Result(ResultFrame::ok(id, json!({}))),
                Err(err) => Frame::Result(error_frame(id, &err)),
            };
            write_frame(&mut writer, &frame).map_err(frame_io)?;
            continue;
        }
        let result = dispatch(handler.as_ref(), &route, &call.input);
        let frame = match result {
            Ok(output) => Frame::Result(ResultFrame::ok(id, output)),
            Err(err) => Frame::Result(error_frame(id, &err)),
        };
        write_frame(&mut writer, &frame).map_err(frame_io)?;
        if route == "shutdown" {
            shutdown.store(true, Ordering::SeqCst);
            return Ok(());
        }
    }
}

fn frame_io(err: protocol::plugin::FrameError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, err)
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (a, b) in left.iter().zip(right) {
        diff |= a ^ b;
    }
    diff == 0
}

fn error_frame(id: u64, error: &HostError) -> ResultFrame {
    let wire = WireError {
        code: error.code,
        category: protocol::plugin::category_of(error.code).to_owned(),
        retryable: error.retryable,
        message: error.message.clone(),
    };
    ResultFrame::error(id, wire)
}

struct WireStreamSink<'a> {
    id: u64,
    seq: AtomicU64,
    writer: Mutex<&'a mut BufWriter<Connection>>,
}

impl StreamSink for WireStreamSink<'_> {
    fn on_chunk(&mut self, chunk: &Value, done: bool) -> Result<(), HostError> {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        let frame = Frame::Stream(StreamFrame {
            id: self.id,
            seq,
            chunk: chunk.clone(),
            done,
        });
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| HostError::internal("control writer poisoned"))?;
        write_frame(&mut **writer, &frame)
            .map_err(|err| HostError::internal(format!("write stream chunk: {err}")))
    }
}

fn id_of(input: &Value) -> Result<PluginId, HostError> {
    let raw = input
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| HostError::invalid_input("missing `id`"))?;
    PluginId::parse(raw).map_err(|err| HostError::invalid_input(format!("invalid id: {err}")))
}

fn grace_of(input: &Value) -> Duration {
    let secs = input
        .get("grace")
        .and_then(Value::as_u64)
        .filter(|secs| *secs <= 300)
        .unwrap_or(10);
    Duration::from_secs(secs)
}

fn dispatch(handler: &dyn ControlHandler, method: &str, input: &Value) -> Result<Value, HostError> {
    match method {
        "auth" => Ok(json!({})),
        "list" => handler.list(),
        "status" => handler.status(&id_of(input)?),
        "start" => handler.start(&id_of(input)?),
        "stop" => handler.stop(&id_of(input)?, grace_of(input)),
        "restart" => handler.restart(&id_of(input)?, grace_of(input)),
        "call" => {
            let id = id_of(input)?;
            let op = input
                .get("op")
                .and_then(Value::as_str)
                .ok_or_else(|| HostError::invalid_input("missing `op`"))?;
            let args = input.get("input").cloned().unwrap_or_else(|| json!({}));
            handler.call(&id, op, args)
        }
        "logs" => {
            let id = id_of(input)?;
            let offset = input.get("offset").and_then(Value::as_u64);
            let max = input
                .get("max")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(200);
            handler.logs(&id, offset, max)
        }
        "config_get" => handler.config_get(&id_of(input)?),
        "config_set" => handler.config_set(&id_of(input)?, input.get("values")),
        "config_apply" => {
            let id = id_of(input)?;
            let revision = input
                .get("revision")
                .and_then(Value::as_u64)
                .ok_or_else(|| HostError::invalid_input("missing `revision`"))?;
            let values = input
                .get("values")
                .ok_or_else(|| HostError::invalid_input("missing `values`"))?;
            handler.config_apply(&id, revision, values)
        }
        "grant" => {
            let id = id_of(input)?;
            let capability = capability_of(input)?;
            handler.grant(&id, capability)
        }
        "revoke" => {
            let id = id_of(input)?;
            let capability = capability_of(input)?;
            handler.revoke(&id, capability)
        }
        "reload" => handler.reload(),
        "shutdown" => handler.shutdown(),
        other => Err(HostError::invalid_input(format!(
            "unknown control method `{other}`"
        ))),
    }
}

fn dispatch_stream(
    handler: &dyn ControlHandler,
    input: &Value,
    sink: &mut dyn StreamSink,
) -> Result<(), HostError> {
    let id = id_of(input)?;
    let op = input
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| HostError::invalid_input("missing `op`"))?;
    let args = input.get("input").cloned().unwrap_or_else(|| json!({}));
    handler.stream(&id, op, args, sink)
}

fn capability_of(input: &Value) -> Result<Capability, HostError> {
    let raw = input
        .get("capability")
        .and_then(Value::as_str)
        .ok_or_else(|| HostError::invalid_input("missing `capability`"))?;
    Capability::parse(raw)
        .map_err(|err| HostError::invalid_input(format!("unknown capability: {err}")))
}

/// 控制方法实现
///
/// 宿主运行时之外的入口（CLI 管理命令、未来 REST）都经这个 trait 复用同一套
/// 分发；方法体只做协议字段映射与错误码转换。
pub trait ControlHandler: Send + Sync {
    /// 列出全部记录与实时状态
    ///
    /// # Errors
    ///
    /// 记录读取失败时返回错误
    fn list(&self) -> Result<Value, HostError>;
    /// 单插件状态
    ///
    /// # Errors
    ///
    /// 不存在返回 `not_found`
    fn status(&self, id: &PluginId) -> Result<Value, HostError>;
    /// 拉起插件
    ///
    /// # Errors
    ///
    /// 未启用或握手失败时返回错误
    fn start(&self, id: &PluginId) -> Result<Value, HostError>;
    /// 停止插件
    ///
    /// # Errors
    ///
    /// 未运行时返回 `conflict`
    fn stop(&self, id: &PluginId, grace: Duration) -> Result<Value, HostError>;
    /// 重启插件
    ///
    /// # Errors
    ///
    /// 同 [`Self::start`]
    fn restart(&self, id: &PluginId, grace: Duration) -> Result<Value, HostError>;
    /// 调用插件 op
    ///
    /// # Errors
    ///
    /// 能力未授予、超时等
    fn call(&self, id: &PluginId, op: &str, input: Value) -> Result<Value, HostError>;
    /// 订阅插件 stream
    ///
    /// # Errors
    ///
    /// 同 [`Self::call`]
    fn stream(
        &self,
        id: &PluginId,
        op: &str,
        input: Value,
        sink: &mut dyn StreamSink,
    ) -> Result<(), HostError>;
    /// 游标读取日志
    ///
    /// # Errors
    ///
    /// 读取失败时返回错误
    fn logs(&self, id: &PluginId, offset: Option<u64>, max: usize) -> Result<Value, HostError>;
    /// 读取配置（密钥打码）
    ///
    /// # Errors
    ///
    /// 记录不存在或配置无效时返回错误
    fn config_get(&self, id: &PluginId) -> Result<Value, HostError>;
    /// 写入配置
    ///
    /// # Errors
    ///
    /// schema 校验失败或插件拒绝应用时返回错误
    fn config_set(&self, id: &PluginId, values: Option<&Value>) -> Result<Value, HostError>;
    /// 只下发已校验的配置（管理命令已落盘）
    ///
    /// # Errors
    ///
    /// 插件拒绝或超时
    fn config_apply(
        &self,
        id: &PluginId,
        revision: u64,
        values: &Value,
    ) -> Result<Value, HostError>;
    /// 追加授权
    ///
    /// # Errors
    ///
    /// 能力未声明时返回 `invalid_input`
    fn grant(&self, id: &PluginId, capability: Capability) -> Result<Value, HostError>;
    /// 撤销授权（立即生效）
    ///
    /// # Errors
    ///
    /// 能力未授予时返回 `not_found`
    fn revoke(&self, id: &PluginId, capability: Capability) -> Result<Value, HostError>;
    /// 重读安装记录
    ///
    /// # Errors
    ///
    /// 记录读取失败时返回错误
    fn reload(&self) -> Result<Value, HostError>;
    /// 请求宿主退出
    ///
    /// # Errors
    ///
    /// 当前实现恒成功
    fn shutdown(&self) -> Result<Value, HostError>;
}

/// 基于宿主持久化能力的控制实现
pub struct HostControl {
    supervisor: Arc<dyn PluginSupervisor>,
    store: Arc<dyn PluginStore>,
    config: Arc<dyn ConfigOps>,
    log_dir: PathBuf,
    audit: Arc<dyn AuditSink>,
}

impl std::fmt::Debug for HostControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostControl")
            .field("log_dir", &self.log_dir)
            .finish_non_exhaustive()
    }
}

impl HostControl {
    /// 构造
    #[must_use]
    pub fn new(
        supervisor: Arc<dyn PluginSupervisor>,
        store: Arc<dyn PluginStore>,
        config: Arc<dyn ConfigOps>,
        log_dir: PathBuf,
        audit: Arc<dyn AuditSink>,
    ) -> Self {
        Self {
            supervisor,
            store,
            config,
            log_dir,
            audit,
        }
    }
}

fn session_json(info: &SessionInfo) -> Value {
    json!({
        "session_id": info.session_id,
        "granted": info.granted.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "pid": info.pid,
        "started_at": info.started_at,
    })
}

fn offline_state(record: &PluginRecord) -> PluginState {
    if !record.enabled {
        return match record.last_state {
            PluginState::PendingApproval | PluginState::Failed => record.last_state,
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

fn record_json(record: &PluginRecord, status: &PluginRuntimeStatus) -> Value {
    json!({
        "id": record.id.to_string(),
        "version": record.version,
        "enabled": record.enabled,
        "scope": record.scope.as_str(),
        "server": record.server.as_ref().map(ToString::to_string),
        "state": status.state.as_str(),
        "session": status.session.as_ref().map(session_json),
        "needs_approval": record.needs_approval(),
        "approved": record.approved.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "capabilities": record.declared_capabilities().iter().map(ToString::to_string).collect::<Vec<_>>(),
        "last_error": status.last_error,
        "blocked": record.blocked,
        "quarantined": record.quarantined,
        "sha256": record.sha256,
        "source": record.source,
    })
}

impl ControlHandler for HostControl {
    fn list(&self) -> Result<Value, HostError> {
        let records = self
            .store
            .list()
            .map_err(|err| HostError::internal(format!("list plugin records: {err}")))?;
        let mut items = Vec::with_capacity(records.len());
        for record in records {
            let status =
                self.supervisor
                    .status(&record.id)
                    .unwrap_or_else(|_| PluginRuntimeStatus {
                        id: record.id.clone(),
                        state: offline_state(&record),
                        session: None,
                        last_error: record.last_error.clone(),
                    });
            items.push(record_json(&record, &status));
        }
        Ok(json!({ "items": items }))
    }

    fn status(&self, id: &PluginId) -> Result<Value, HostError> {
        let record = self.store.get(id).map_err(|err| match err {
            port::RepositoryError::NotFound(_) => {
                HostError::not_found(format!("plugin {id} not found"))
            }
            other => HostError::internal(format!("read plugin {id}: {other}")),
        })?;
        let status = self
            .supervisor
            .status(id)
            .unwrap_or_else(|_| PluginRuntimeStatus {
                id: id.clone(),
                state: offline_state(&record),
                session: None,
                last_error: record.last_error.clone(),
            });
        Ok(record_json(&record, &status))
    }

    fn start(&self, id: &PluginId) -> Result<Value, HostError> {
        let info = self.supervisor.start(id)?;
        Ok(session_json(&info))
    }

    fn stop(&self, id: &PluginId, grace: Duration) -> Result<Value, HostError> {
        self.supervisor.stop(id, grace)?;
        Ok(json!({ "stopped": true }))
    }

    fn restart(&self, id: &PluginId, grace: Duration) -> Result<Value, HostError> {
        if let Ok(status) = self.supervisor.status(id)
            && status.session.is_some()
        {
            self.supervisor.stop(id, grace)?;
        }
        let info = self.supervisor.start(id)?;
        Ok(session_json(&info))
    }

    fn call(&self, id: &PluginId, op: &str, input: Value) -> Result<Value, HostError> {
        self.supervisor.call(id, op, input)
    }

    fn stream(
        &self,
        id: &PluginId,
        op: &str,
        input: Value,
        sink: &mut dyn StreamSink,
    ) -> Result<(), HostError> {
        self.supervisor.stream(id, op, input, sink)
    }

    fn logs(&self, id: &PluginId, offset: Option<u64>, max: usize) -> Result<Value, HostError> {
        if max == 0 || max > MAX_LOG_PAGE_LINES {
            return Err(HostError::limit_exceeded(format!(
                "max must be 1..={MAX_LOG_PAGE_LINES}"
            )));
        }
        let path = self.log_dir.join(format!("{id}.log"));
        // 只读一页所需的窗口：`max` 行 × 单行上限，再夹到读取总上限。
        // 整文件读在 `--follow` 下是每 500 ms 一次的全量重读，日志涨到
        // 几百 MB 时既拖慢调用又可能把宿主内存打满。
        let want = max
            .saturating_mul(crate::process::MAX_LOG_LINE_BYTES)
            .min(domain::limits::MAX_LOG_TAIL_BYTES);
        let window = match offset {
            Some(start) => domain::log::Window::from(&path, start, want),
            None => domain::log::Window::tail(&path, want),
        }
        .map_err(|err| HostError::internal(format!("read plugin log: {err}")))?;
        let mut lines = window.lines();
        if lines.len() > max {
            match offset {
                // 首次调用要的是尾部：保留最后 `max` 行
                None => {
                    let excess = lines.len() - max;
                    lines.drain(..excess);
                }
                // 跟进模式保留**最前** `max` 行，游标落在最后一行末尾，
                // 下一拍从那里续读。留尾部会让中间的行永久丢失。
                Some(_) => lines.truncate(max),
            }
        }
        let next = lines
            .last()
            .map_or_else(|| window.end(), |(_, cursor)| *cursor);
        Ok(json!({
            "lines": lines.into_iter().map(|(text, _)| text).collect::<Vec<_>>(),
            "next": next,
            "size": window.file_len(),
        }))
    }

    fn config_get(&self, id: &PluginId) -> Result<Value, HostError> {
        self.config.get(id)
    }

    fn config_set(&self, id: &PluginId, values: Option<&Value>) -> Result<Value, HostError> {
        let values = values.ok_or_else(|| HostError::invalid_input("missing `values`"))?;
        self.config.set(id, values)
    }

    fn config_apply(
        &self,
        id: &PluginId,
        revision: u64,
        values: &Value,
    ) -> Result<Value, HostError> {
        self.supervisor.apply_config(id, revision, values)?;
        Ok(json!({ "applied": true, "revision": revision }))
    }

    fn grant(&self, id: &PluginId, capability: Capability) -> Result<Value, HostError> {
        let mut record = self.store.get(id).map_err(|err| match err {
            port::RepositoryError::NotFound(_) => {
                HostError::not_found(format!("plugin {id} not found"))
            }
            other => HostError::internal(format!("read plugin {id}: {other}")),
        })?;
        // 判定与状态迁移在 `PluginRecord::grant_capability`（领域收敛点），
        // 与 `service::PluginService::grant` 共用同一份。授予只可能返回
        // `NotDeclared` / `NotOpen`，都按输入错误报出。
        record
            .grant_capability(capability)
            .map_err(|err| HostError::invalid_input(err.to_string()))?;
        record.updated_at = now_secs();
        self.store
            .put(&record)
            .map_err(|err| HostError::internal(format!("write plugin record: {err}")))?;
        self.audit.record(&AuditEvent {
            at: now_secs(),
            plugin: id.to_string(),
            action: "capability:grant".to_owned(),
            capability: Some(capability),
            result: AuditResult::Allowed,
            detail: None,
        });
        self.supervisor.reload()?;
        Ok(json!({ "granted": capability.to_string() }))
    }

    fn revoke(&self, id: &PluginId, capability: Capability) -> Result<Value, HostError> {
        let mut record = self.store.get(id).map_err(|err| match err {
            port::RepositoryError::NotFound(_) => {
                HostError::not_found(format!("plugin {id} not found"))
            }
            other => HostError::internal(format!("read plugin {id}: {other}")),
        })?;
        record
            .revoke_capability(capability)
            .map_err(|err| HostError::not_found(err.to_string()))?;
        record.updated_at = now_secs();
        self.store
            .put(&record)
            .map_err(|err| HostError::internal(format!("write plugin record: {err}")))?;
        self.supervisor.revoke(id, capability)?;
        self.audit.record(&AuditEvent {
            at: now_secs(),
            plugin: id.to_string(),
            action: "capability:revoke".to_owned(),
            capability: Some(capability),
            result: AuditResult::Denied,
            detail: None,
        });
        Ok(json!({ "revoked": capability.to_string() }))
    }

    fn reload(&self) -> Result<Value, HostError> {
        self.supervisor.reload()?;
        Ok(json!({ "reloaded": true }))
    }

    fn shutdown(&self) -> Result<Value, HostError> {
        Ok(json!({ "shutting_down": true }))
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}

// ---------------------------------------------------------------- 客户端

/// 控制客户端
///
/// 实现 [`PluginSupervisor`]，供 `service::PluginService` 与门面装配使用。
/// 每条命令独立连接：宿主重启后下一次调用自然重连，不需要失效检测。
pub struct ControlClient {
    run_dir: PathBuf,
    next_id: AtomicU64,
}

impl std::fmt::Debug for ControlClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlClient")
            .field("run_dir", &self.run_dir)
            .finish_non_exhaustive()
    }
}

impl ControlClient {
    /// 按数据目录构造；`data_dir` 与宿主一致
    #[must_use]
    pub fn new(data_dir: &Path) -> Self {
        Self {
            run_dir: data_dir.join("run"),
            next_id: AtomicU64::new(1),
        }
    }

    /// 宿主是否可达
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.connect().is_ok()
    }

    fn connect(&self) -> Result<Connection, HostError> {
        #[cfg(unix)]
        {
            let path = self.run_dir.join(SOCKET_FILE);
            if path.exists()
                && let Ok(stream) = std::os::unix::net::UnixStream::connect(&path)
            {
                stream
                    .set_read_timeout(Some(IO_TIMEOUT))
                    .map_err(io_error)?;
                stream
                    .set_write_timeout(Some(IO_TIMEOUT))
                    .map_err(io_error)?;
                return Ok(Connection::unix(stream));
            }
        }
        #[cfg(windows)]
        {
            // 优先命名管道：没有端点文件也就没有令牌，认证由管道 ACL 承担。
            let pipe_name = control_pipe_name(&self.run_dir);
            let connected = pipe_name
                .to_fs_name::<interprocess::os::windows::local_socket::NamedPipe>()
                .and_then(|name| interprocess::local_socket::Stream::connect(name));
            if let Ok(stream) = connected {
                let conn = Connection::pipe(stream);
                conn.set_read_timeout(Some(IO_TIMEOUT)).map_err(io_error)?;
                conn.set_write_timeout(Some(IO_TIMEOUT)).map_err(io_error)?;
                return Ok(conn);
            }
        }
        let endpoint = self.run_dir.join(ENDPOINT_FILE);
        let address = std::fs::read_to_string(&endpoint).map_err(|_| {
            HostError::new(
                port::plugin::ErrorCode::Unavailable,
                "plugin host is not running",
            )
        })?;
        let address = address.trim().trim_start_matches("tcp://");
        let stream = std::net::TcpStream::connect(address).map_err(|err| {
            HostError::new(
                port::plugin::ErrorCode::Unavailable,
                format!("connect plugin host: {err}"),
            )
        })?;
        stream
            .set_read_timeout(Some(IO_TIMEOUT))
            .map_err(io_error)?;
        stream
            .set_write_timeout(Some(IO_TIMEOUT))
            .map_err(io_error)?;
        let token = std::fs::read_to_string(self.run_dir.join(TOKEN_FILE)).map_err(|err| {
            HostError::new(
                port::plugin::ErrorCode::Unavailable,
                format!("read control token: {err}"),
            )
        })?;
        let conn = Connection::tcp(stream);
        {
            let mut writer = BufWriter::new(conn.try_clone().map_err(io_error)?);
            let mut reader = BufReader::new(conn.try_clone().map_err(io_error)?);
            let call = Call::control(0, "auth", json!({ "token": token.trim() }));
            write_frame(&mut writer, &Frame::Call(call)).map_err(frame_error)?;
            match read_frame(&mut reader).map_err(frame_error)? {
                Frame::Result(result) if result.ok => {}
                Frame::Result(result) => return Err(wire_error(result)),
                _ => {
                    return Err(HostError::internal(
                        "control handshake returned an unexpected frame",
                    ));
                }
            }
        }
        Ok(conn)
    }

    fn request(&self, method: &str, input: Value) -> Result<Value, HostError> {
        let conn = self.connect()?;
        let mut writer = BufWriter::new(conn.try_clone().map_err(io_error)?);
        let mut reader = BufReader::new(conn);
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        write_frame(&mut writer, &Frame::Call(Call::control(id, method, input)))
            .map_err(frame_error)?;
        match read_frame(&mut reader).map_err(frame_error)? {
            Frame::Result(result) if result.id == id => {
                if result.ok {
                    Ok(result.output.unwrap_or(Value::Null))
                } else {
                    Err(wire_error(result))
                }
            }
            _ => Err(HostError::internal(
                "control channel returned a mismatched frame",
            )),
        }
    }

    /// 列出全部插件（原始 JSON）
    ///
    /// # Errors
    ///
    /// 宿主不可达时返回 `unavailable`
    pub fn list(&self) -> Result<Value, HostError> {
        self.request("list", json!({}))
    }

    /// 读取日志
    ///
    /// # Errors
    ///
    /// 宿主不可达时返回 `unavailable`
    pub fn logs(&self, id: &PluginId, offset: Option<u64>, max: usize) -> Result<Value, HostError> {
        self.request(
            "logs",
            json!({ "id": id.to_string(), "offset": offset, "max": max }),
        )
    }

    /// 请求宿主退出
    ///
    /// # Errors
    ///
    /// 宿主不可达时返回 `unavailable`
    pub fn shutdown(&self) -> Result<Value, HostError> {
        self.request("shutdown", json!({}))
    }
}

#[allow(clippy::needless_pass_by_value)] // `map_err(io_error)` 的入参就是所有权
fn io_error(err: io::Error) -> HostError {
    HostError::new(
        port::plugin::ErrorCode::Unavailable,
        format!("control channel: {err}"),
    )
}

#[allow(clippy::needless_pass_by_value)] // `map_err(frame_error)` 的入参就是所有权
fn frame_error(err: protocol::plugin::FrameError) -> HostError {
    HostError::new(
        port::plugin::ErrorCode::Unavailable,
        format!("control frame: {err}"),
    )
}

fn wire_error(result: ResultFrame) -> HostError {
    match result.error {
        Some(error) => HostError {
            code: error.code,
            category: port::plugin::category_of(error.code),
            retryable: error.retryable,
            message: error.message,
        },
        None => HostError::internal("control call failed without an error payload"),
    }
}

fn parse_status(output: &Value) -> Result<PluginRuntimeStatus, HostError> {
    let state: PluginState = serde_json::from_value(
        output
            .get("state")
            .cloned()
            .ok_or_else(|| HostError::internal("control status is missing `state`"))?,
    )
    .map_err(|err| HostError::internal(format!("invalid plugin state: {err}")))?;
    let session = output
        .get("session")
        .filter(|value| !value.is_null())
        .map(|value| {
            let granted = value
                .get("granted")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .filter_map(|raw| Capability::parse(raw).ok())
                        .collect()
                })
                .unwrap_or_default();
            SessionInfo {
                session_id: value
                    .get("session_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                granted,
                pid: value
                    .get("pid")
                    .and_then(Value::as_u64)
                    .and_then(|pid| u32::try_from(pid).ok())
                    .unwrap_or(0),
                started_at: value.get("started_at").and_then(Value::as_i64).unwrap_or(0),
            }
        });
    let id = output
        .get("id")
        .and_then(Value::as_str)
        .and_then(|raw| PluginId::parse(raw).ok())
        .ok_or_else(|| HostError::internal("control status is missing `id`"))?;
    Ok(PluginRuntimeStatus {
        id,
        state,
        session,
        last_error: output
            .get("last_error")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    })
}

impl PluginSupervisor for ControlClient {
    fn start(&self, id: &PluginId) -> Result<SessionInfo, HostError> {
        let output = self.request("start", json!({ "id": id.to_string() }))?;
        let status = parse_status(&json!({
            "id": id.to_string(),
            "state": "running",
            "session": output,
        }))?;
        status
            .session
            .ok_or_else(|| HostError::internal("control start returned no session"))
    }

    fn stop(&self, id: &PluginId, grace: Duration) -> Result<(), HostError> {
        self.request(
            "stop",
            json!({ "id": id.to_string(), "grace": grace.as_secs() }),
        )?;
        Ok(())
    }

    fn status(&self, id: &PluginId) -> Result<PluginRuntimeStatus, HostError> {
        let output = self.request("status", json!({ "id": id.to_string() }))?;
        parse_status(&output)
    }

    fn call(&self, id: &PluginId, op: &str, input: Value) -> Result<Value, HostError> {
        self.request(
            "call",
            json!({ "id": id.to_string(), "op": op, "input": input }),
        )
    }

    fn stream(
        &self,
        id: &PluginId,
        op: &str,
        input: Value,
        sink: &mut dyn StreamSink,
    ) -> Result<(), HostError> {
        let conn = self.connect()?;
        let mut writer = BufWriter::new(conn.try_clone().map_err(io_error)?);
        let mut reader = BufReader::new(conn);
        let call_id = self.next_id.fetch_add(1, Ordering::SeqCst);
        write_frame(
            &mut writer,
            &Frame::Call(Call::control(
                call_id,
                "stream",
                json!({ "id": id.to_string(), "op": op, "input": input }),
            )),
        )
        .map_err(frame_error)?;
        loop {
            match read_frame(&mut reader).map_err(frame_error)? {
                Frame::Stream(frame) if frame.id == call_id => {
                    sink.on_chunk(&frame.chunk, frame.done)?;
                    if frame.done {
                        return Ok(());
                    }
                }
                Frame::Result(result) if result.id == call_id => {
                    if result.ok {
                        return Ok(());
                    }
                    return Err(wire_error(result));
                }
                _ => {}
            }
        }
    }

    fn reload(&self) -> Result<(), HostError> {
        self.request("reload", json!({}))?;
        Ok(())
    }

    fn revoke(&self, id: &PluginId, capability: Capability) -> Result<(), HostError> {
        self.request(
            "revoke",
            json!({ "id": id.to_string(), "capability": capability.to_string() }),
        )?;
        Ok(())
    }

    fn logs(&self, id: &PluginId, offset: Option<u64>, max: usize) -> Result<Value, HostError> {
        ControlClient::logs(self, id, offset, max)
    }

    fn apply_config(&self, id: &PluginId, revision: u64, values: &Value) -> Result<(), HostError> {
        self.request(
            "config_apply",
            json!({
                "id": id.to_string(),
                "revision": revision,
                "values": values,
            }),
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use port::RepositoryError;
    use port::plugin::PluginRecord;
    use protocol::plugin::PLUGIN_CONTROL_VERSION;
    use std::collections::{BTreeMap, BTreeSet};

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

    struct FakeSupervisor;

    struct StubConfig;

    impl ConfigOps for StubConfig {
        fn get(&self, _id: &PluginId) -> Result<Value, HostError> {
            Err(HostError::unsupported("config is not wired in this test"))
        }
        fn set(&self, _id: &PluginId, _values: &Value) -> Result<Value, HostError> {
            Err(HostError::unsupported("config is not wired in this test"))
        }
    }

    impl PluginSupervisor for FakeSupervisor {
        fn start(&self, _id: &PluginId) -> Result<SessionInfo, HostError> {
            Ok(SessionInfo {
                session_id: "s1".to_owned(),
                granted: BTreeSet::default(),
                pid: 42,
                started_at: 1,
            })
        }
        fn stop(&self, _id: &PluginId, _grace: Duration) -> Result<(), HostError> {
            Ok(())
        }
        fn status(&self, id: &PluginId) -> Result<PluginRuntimeStatus, HostError> {
            Ok(PluginRuntimeStatus {
                id: id.clone(),
                state: PluginState::Running,
                session: None,
                last_error: None,
            })
        }
        fn call(&self, _id: &PluginId, op: &str, _input: Value) -> Result<Value, HostError> {
            Ok(json!({ "op": op }))
        }
        fn stream(
            &self,
            _id: &PluginId,
            _op: &str,
            _input: Value,
            sink: &mut dyn StreamSink,
        ) -> Result<(), HostError> {
            sink.on_chunk(&json!({ "a": 1 }), false)?;
            sink.on_chunk(&json!({ "a": 2 }), true)
        }
        fn reload(&self) -> Result<(), HostError> {
            Ok(())
        }
    }

    struct Collect(Mutex<Vec<Value>>);

    impl StreamSink for Collect {
        fn on_chunk(&mut self, chunk: &Value, done: bool) -> Result<(), HostError> {
            self.0.lock().expect("lock").push(chunk.clone());
            if done {
                self.0.lock().expect("lock").push(json!({ "done": true }));
            }
            Ok(())
        }
    }

    fn handler() -> HostControl {
        HostControl::new(
            Arc::new(FakeSupervisor),
            Arc::new(MemoryStore::default()),
            Arc::new(StubConfig),
            std::env::temp_dir(),
            Arc::new(port::plugin::NoopAuditSink),
        )
    }

    fn handler_with_log_dir(dir: PathBuf) -> HostControl {
        HostControl::new(
            Arc::new(FakeSupervisor),
            Arc::new(MemoryStore::default()),
            Arc::new(StubConfig),
            dir,
            Arc::new(port::plugin::NoopAuditSink),
        )
    }

    fn page_lines(payload: &Value) -> Vec<&str> {
        payload["lines"]
            .as_array()
            .expect("lines")
            .iter()
            .filter_map(Value::as_str)
            .collect()
    }

    #[test]
    fn dispatch_rejects_unknown_methods() {
        let err = dispatch(&handler(), "nope", &json!({})).expect_err("unknown");
        assert_eq!(err.code, port::plugin::ErrorCode::InvalidInput);
    }

    #[test]
    fn dispatch_requires_id_for_status() {
        let err = dispatch(&handler(), "status", &json!({})).expect_err("missing id");
        assert_eq!(err.code, port::plugin::ErrorCode::InvalidInput);
    }

    #[test]
    #[allow(clippy::items_after_statements)] // 适配器只在这个用例里用
    fn stream_forwards_chunks_in_order() {
        let sink = Arc::new(Mutex::new(Collect(Mutex::new(Vec::new()))));
        struct Adapter(Arc<Mutex<Collect>>);
        impl StreamSink for Adapter {
            fn on_chunk(&mut self, chunk: &Value, done: bool) -> Result<(), HostError> {
                self.0.lock().expect("lock").on_chunk(chunk, done)
            }
        }
        let mut adapter = Adapter(Arc::clone(&sink));
        dispatch_stream(
            &handler(),
            &json!({ "id": "demo", "op": "watch" }),
            &mut adapter,
        )
        .expect("stream");
        let chunks = sink.lock().expect("lock").0.lock().expect("lock").clone();
        assert_eq!(chunks.len(), 3);
    }

    #[test]
    fn token_comparison_is_length_safe() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
    }

    #[test]
    fn control_version_is_stable() {
        assert_eq!(PLUGIN_CONTROL_VERSION, 1);
    }

    /// 首次调用要尾部：`next` 与 `size` 都指向文件末尾
    #[test]
    fn logs_tail_returns_the_last_lines_with_a_cursor() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("demo.log"), "one\ntwo\nthree\n").expect("write");
        let control = handler_with_log_dir(dir.path().to_path_buf());
        let id = PluginId::parse("demo").expect("id");
        let payload = control.logs(&id, None, 2).expect("logs");
        assert_eq!(page_lines(&payload), vec!["two", "three"]);
        assert_eq!(payload["next"].as_u64(), Some(14));
        assert_eq!(payload["size"].as_u64(), Some(14));
    }

    /// 跟进模式保留**最前**一页并推进游标：截断尾部会让中间的行永久丢失
    #[test]
    fn logs_follow_advances_the_cursor_without_dropping_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("demo.log"), "a\nb\nc\nd\n").expect("write");
        let control = handler_with_log_dir(dir.path().to_path_buf());
        let id = PluginId::parse("demo").expect("id");
        let first = control.logs(&id, Some(0), 2).expect("first page");
        assert_eq!(page_lines(&first), vec!["a", "b"]);
        assert_eq!(first["next"].as_u64(), Some(4));
        let second = control
            .logs(&id, Some(first["next"].as_u64().expect("next")), 2)
            .expect("second page");
        assert_eq!(page_lines(&second), vec!["c", "d"]);
        assert_eq!(second["next"].as_u64(), Some(8));
    }

    /// `max` 来自控制通道对端，不能按它分配
    #[test]
    fn logs_rejects_an_out_of_range_page_size() {
        let control = handler();
        let id = PluginId::parse("demo").expect("id");
        for max in [0, MAX_LOG_PAGE_LINES + 1] {
            let err = control.logs(&id, None, max).expect_err("out of range");
            assert_eq!(err.code, port::plugin::ErrorCode::LimitExceeded);
        }
    }
}
