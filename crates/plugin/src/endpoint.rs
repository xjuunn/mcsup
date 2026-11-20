//! 插件端点：Unix socket / Windows 命名管道，不可用时回退回环 TCP
//!
//! 端点只服务一次握手。文件名与管道名都带 64 位 FNV-1a 哈希，输入是
//! `(run_dir, plugin_id, scope, server)`：同 id 不同作用域不会撞名。
//!
//! macOS 的 Unix socket 路径上限约 104 字节，超过 100 字节时改用系统临时目录；
//! 仍超限则回退 TCP。Windows 命名管道在管道系统不可用时同样回退 TCP。
//! TCP 只绑定 `127.0.0.1`，不允许其它地址。

use crate::conn::Connection;
use domain::plugin::{PluginId, PluginScope};
use domain::server::meta::ServerName;
use std::io;
use std::net::{SocketAddr, TcpListener};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::net::UnixListener;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[cfg(windows)]
use interprocess::local_socket::ListenerNonblockingMode;
#[cfg(windows)]
use interprocess::local_socket::prelude::*;

/// 端点：一个尚未完成握手的监听器
pub struct Endpoint {
    inner: Inner,
    #[cfg(unix)]
    path: Option<PathBuf>,
    #[cfg(windows)]
    pipe_name: String,
}

enum Inner {
    #[cfg(unix)]
    Unix(UnixListener),
    #[cfg(windows)]
    Pipe(interprocess::local_socket::Listener),
    Tcp(TcpListener, SocketAddr),
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("address", &self.address())
            .finish()
    }
}

impl Endpoint {
    /// 在 `run_dir` 下建立端点
    ///
    /// # Errors
    ///
    /// 目录创建、socket 绑定或端口分配失败时返回 IO 错误
    pub fn bind(
        run_dir: &Path,
        id: &PluginId,
        scope: PluginScope,
        server: Option<&ServerName>,
    ) -> io::Result<Self> {
        // 哈希带上 `run_dir`：不同 data_dir 的实例不会因为同 id 撞名，
        // 回退到系统临时目录时也不必额外区分。
        let hash = endpoint_hash_salted(run_dir.as_os_str().as_encoded_bytes(), id, scope, server);
        #[cfg(unix)]
        if let Ok(endpoint) = Self::bind_unix(run_dir, hash) {
            return Ok(endpoint);
        }
        #[cfg(windows)]
        if let Ok(endpoint) = Self::bind_pipe(hash) {
            return Ok(endpoint);
        }
        Self::bind_tcp()
    }

    #[cfg(unix)]
    fn bind_unix(run_dir: &Path, hash: u64) -> io::Result<Self> {
        std::fs::create_dir_all(run_dir)?;
        let mut path = run_dir.join(format!("plugin-{hash:016x}.sock"));
        if path.as_os_str().len() > 100 {
            let fallback = std::env::temp_dir().join(format!("mcsup-{}", std::process::id()));
            std::fs::create_dir_all(&fallback)?;
            path = fallback.join(format!("pl-{hash:016x}.sock"));
        }
        // 上次异常退出可能留下同名文件；只有确认无人监听时才删除。
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            inner: Inner::Unix(listener),
            path: Some(path),
        })
    }

    #[cfg(windows)]
    fn bind_pipe(hash: u64) -> io::Result<Self> {
        use interprocess::os::windows::local_socket::NamedPipe;
        let pipe_name = endpoint_pipe_name(hash);
        let name = pipe_name
            .clone()
            .to_fs_name::<NamedPipe>()
            .map_err(io::Error::other)?;
        let listener = interprocess::local_socket::ListenerOptions::new()
            .name(name)
            .create_sync()?;
        Ok(Self {
            inner: Inner::Pipe(listener),
            pipe_name,
        })
    }

    fn bind_tcp() -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        Ok(Self {
            inner: Inner::Tcp(listener, addr),
            #[cfg(unix)]
            path: None,
            // 回退 TCP 时管道名不再使用；保留空串避免 Option 判空。
            #[cfg(windows)]
            pipe_name: String::new(),
        })
    }

    /// 传给插件 `--endpoint` 的地址
    #[must_use]
    pub fn address(&self) -> String {
        match &self.inner {
            #[cfg(unix)]
            Inner::Unix(_) => self
                .path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
            #[cfg(windows)]
            Inner::Pipe(_) => self.pipe_name.clone(),
            Inner::Tcp(_, addr) => format!("tcp://{addr}"),
        }
    }

    /// 在截止时间前接受一条连接
    ///
    /// # Errors
    ///
    /// 超时（[`io::ErrorKind::TimedOut`]）、监听器失效或重复接受时返回错误
    pub fn accept(&self, timeout: Duration) -> io::Result<Connection> {
        let deadline = Instant::now() + timeout;
        match &self.inner {
            #[cfg(unix)]
            Inner::Unix(listener) => {
                listener.set_nonblocking(true)?;
                loop {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            listener.set_nonblocking(false)?;
                            // macOS / BSD 的 accept 继承监听器的 O_NONBLOCK；
                            // 不显式切回阻塞模式，会话读循环会立刻得到 EAGAIN。
                            stream.set_nonblocking(false)?;
                            return Ok(Connection::unix(stream));
                        }
                        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                            if Instant::now() >= deadline {
                                let _ = listener.set_nonblocking(false);
                                return Err(timed_out());
                            }
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        Err(err) => {
                            let _ = listener.set_nonblocking(false);
                            return Err(err);
                        }
                    }
                }
            }
            #[cfg(windows)]
            Inner::Pipe(listener) => {
                listener.set_nonblocking(ListenerNonblockingMode::Accept)?;
                loop {
                    match listener.accept() {
                        Ok(stream) => {
                            // `Accept` 模式只影响 accept 本身，流仍是阻塞的；
                            // 会话读循环依赖这一点。
                            return Ok(Connection::pipe(stream));
                        }
                        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                            if Instant::now() >= deadline {
                                let _ = listener.set_nonblocking(ListenerNonblockingMode::Neither);
                                return Err(timed_out());
                            }
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        Err(err) => {
                            let _ = listener.set_nonblocking(ListenerNonblockingMode::Neither);
                            return Err(err);
                        }
                    }
                }
            }
            Inner::Tcp(listener, _) => {
                listener.set_nonblocking(true)?;
                loop {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            listener.set_nonblocking(false)?;
                            // 同 Unix 分支：accept 出的连接可能继承非阻塞标志。
                            stream.set_nonblocking(false)?;
                            return Ok(Connection::tcp(stream));
                        }
                        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                            if Instant::now() >= deadline {
                                let _ = listener.set_nonblocking(false);
                                return Err(timed_out());
                            }
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        Err(err) => {
                            let _ = listener.set_nonblocking(false);
                            return Err(err);
                        }
                    }
                }
            }
        }
    }
}

fn timed_out() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "plugin did not connect before the handshake deadline",
    )
}

#[cfg(unix)]
impl Drop for Endpoint {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Windows 命名管道名（插件端点）
///
/// 路径进不了管道命名空间，按哈希取名；同一 `(run_dir, id, scope, server)`
/// 推出同一名字，两端不需要额外交换端点文件。
#[cfg(any(windows, test))]
#[must_use]
pub fn endpoint_pipe_name(hash: u64) -> String {
    format!(r"\\.\pipe\mcsup-plugin-{hash:016x}")
}

/// 端点哈希：`(plugin_id, scope, server)` 的 FNV-1a 64
#[must_use]
pub fn endpoint_hash(id: &PluginId, scope: PluginScope, server: Option<&ServerName>) -> u64 {
    endpoint_hash_salted(&[], id, scope, server)
}

fn endpoint_hash_salted(
    salt: &[u8],
    id: &PluginId,
    scope: PluginScope,
    server: Option<&ServerName>,
) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    feed(salt);
    feed(b"\0");
    feed(id.as_str().as_bytes());
    feed(b"\0");
    feed(scope.as_str().as_bytes());
    feed(b"\0");
    // `machine` 作用域不绑定服务器，`server` 参数即使误传也不参与哈希。
    if scope == PluginScope::Server
        && let Some(server) = server
    {
        feed(server.as_str().as_bytes());
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::io::{Read as _, Write as _};

    #[test]
    fn hash_distinguishes_scope_and_server() {
        let id = PluginId::parse("chat-bridge").expect("id");
        let server = ServerName::parse("survival").expect("server");
        let machine = endpoint_hash(&id, PluginScope::Machine, None);
        let scoped = endpoint_hash(&id, PluginScope::Server, Some(&server));
        let other = endpoint_hash(&id, PluginScope::Server, None);
        assert_ne!(machine, scoped);
        assert_ne!(scoped, other);
        assert_eq!(
            machine,
            endpoint_hash(&id, PluginScope::Machine, Some(&server)),
            "machine 作用域不把 server 计入哈希"
        );
    }

    #[test]
    fn pipe_name_is_stable_and_hash_sensitive() {
        let name = endpoint_pipe_name(0x1234_5678_9abc_def0);
        assert_eq!(name, endpoint_pipe_name(0x1234_5678_9abc_def0));
        assert!(name.starts_with(r"\\.\pipe\mcsup-plugin-"));
        assert_ne!(name, endpoint_pipe_name(0x1234_5678_9abc_def1));
    }

    #[cfg(unix)]
    #[test]
    fn unix_endpoint_accepts_and_cleans_up() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = PluginId::parse("demo").expect("id");
        let endpoint = Endpoint::bind(dir.path(), &id, PluginScope::Machine, None).expect("bind");
        let address = endpoint.address();
        assert!(Path::new(&address).exists());
        let address_owned = address.clone();
        let client = std::thread::spawn(move || {
            let mut stream =
                std::os::unix::net::UnixStream::connect(address_owned).expect("connect");
            stream.write_all(b"ping\n").expect("write");
        });
        let mut conn = endpoint.accept(Duration::from_secs(2)).expect("accept");
        let mut buf = [0_u8; 5];
        conn.read_exact(&mut buf).expect("read");
        assert_eq!(&buf, b"ping\n");
        client.join().expect("join");
        drop(endpoint);
        assert!(!Path::new(&address).exists());
    }

    #[cfg(windows)]
    #[test]
    fn pipe_endpoint_round_trips() {
        use std::io::{Read as _, Write as _};

        let dir = tempfile::tempdir().expect("tempdir");
        let id = PluginId::parse("demo").expect("id");
        let endpoint = Endpoint::bind(dir.path(), &id, PluginScope::Machine, None).expect("bind");
        let address = endpoint.address();
        assert!(
            matches!(&endpoint.inner, Inner::Pipe(_)),
            "Windows 上绑定命名管道而不是回退 TCP"
        );
        let client = std::thread::spawn(move || {
            use interprocess::os::windows::local_socket::NamedPipe;
            let name = address.to_fs_name::<NamedPipe>().expect("pipe name");
            let mut stream = interprocess::local_socket::Stream::connect(name).expect("connect");
            stream.write_all(b"ping\n").expect("write");
        });
        let mut conn = endpoint.accept(Duration::from_secs(2)).expect("accept");
        let mut buf = [0_u8; 5];
        conn.read_exact(&mut buf).expect("read");
        assert_eq!(&buf, b"ping\n");
        client.join().expect("join");
    }

    #[test]
    fn accept_times_out_without_a_client() {
        let id = PluginId::parse("demo").expect("id");
        let dir = tempfile::tempdir().expect("tempdir");
        let endpoint = Endpoint::bind(dir.path(), &id, PluginScope::Machine, None);
        // 这个测试同时覆盖各平台实现：没有客户端时必须超时而不是挂起。
        let result = endpoint.expect("bind").accept(Duration::from_millis(50));
        assert!(result.is_err());
    }
}
