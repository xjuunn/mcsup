//! 端点连接的统一视图
//!
//! Unix socket、Windows 命名管道与回环 TCP 的读写接口相同，差异集中在构造
//! 函数里。上层（握手、会话循环）只依赖本类型，不写 `cfg` 分支。

use std::io::{self, Read, Write};
use std::net::TcpStream;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::time::Duration;

#[cfg(windows)]
use interprocess::TryClone as _;
#[cfg(windows)]
use interprocess::local_socket::Stream as PipeStream;
#[cfg(windows)]
use interprocess::local_socket::prelude::*;
#[cfg(windows)]
use std::sync::{Mutex, MutexGuard};

/// 一条已建立的插件连接
#[derive(Debug)]
pub struct Connection {
    inner: Inner,
}

#[derive(Debug)]
enum Inner {
    #[cfg(unix)]
    Unix(UnixStream),
    #[cfg(windows)]
    Pipe(Mutex<Option<PipeStream>>),
    Tcp(TcpStream),
}

impl Connection {
    /// 包装 Unix socket
    #[cfg(unix)]
    #[must_use]
    pub fn unix(stream: UnixStream) -> Self {
        Self {
            inner: Inner::Unix(stream),
        }
    }

    /// 包装 Windows 命名管道
    #[cfg(windows)]
    #[must_use]
    pub fn pipe(stream: PipeStream) -> Self {
        Self {
            inner: Inner::Pipe(Mutex::new(Some(stream))),
        }
    }

    /// 包装 TCP 连接
    #[must_use]
    pub fn tcp(stream: TcpStream) -> Self {
        Self {
            inner: Inner::Tcp(stream),
        }
    }

    /// 造一对本机回环连接（测试用；不依赖 Unix socket）
    ///
    /// # Errors
    ///
    /// 监听或连接失败时返回 IO 错误
    pub fn pair() -> io::Result<(Self, Self)> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let client = TcpStream::connect(addr)?;
        let (server, _) = listener.accept()?;
        Ok((Self::tcp(server), Self::tcp(client)))
    }

    /// 复制句柄（读线程与写端各持一份）
    ///
    /// # Errors
    ///
    /// 底层 `try_clone` 失败时返回错误
    pub fn try_clone(&self) -> io::Result<Self> {
        let inner = match &self.inner {
            #[cfg(unix)]
            Inner::Unix(stream) => Inner::Unix(stream.try_clone()?),
            #[cfg(windows)]
            Inner::Pipe(stream) => {
                let guard = pipe_guard(stream)?;
                let cloned = pipe_stream(&guard)?.try_clone()?;
                Inner::Pipe(Mutex::new(Some(cloned)))
            }
            Inner::Tcp(stream) => Inner::Tcp(stream.try_clone()?),
        };
        Ok(Self { inner })
    }

    /// 设置读超时
    ///
    /// Windows 命名管道不支持 I/O 超时，`interprocess` 返回 `Unsupported`。
    /// 那是平台能力缺失而不是设置失败，放行由上层的心跳 / 截止时间兜底。
    ///
    /// # Errors
    ///
    /// 底层设置失败时返回错误
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match &self.inner {
            #[cfg(unix)]
            Inner::Unix(stream) => stream.set_read_timeout(timeout),
            #[cfg(windows)]
            Inner::Pipe(stream) => {
                let guard = pipe_guard(stream)?;
                unsupported_ok(pipe_stream(&guard)?.set_recv_timeout(timeout))
            }
            Inner::Tcp(stream) => stream.set_read_timeout(timeout),
        }
    }

    /// 切换阻塞模式
    ///
    /// 控制端点的监听器需要非阻塞轮询才能及时响应 `shutdown`；接受连接后
    /// 必须切回阻塞模式，否则读超时设置不生效。
    ///
    /// # Errors
    ///
    /// 底层设置失败时返回错误
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        match &self.inner {
            #[cfg(unix)]
            Inner::Unix(stream) => stream.set_nonblocking(nonblocking),
            #[cfg(windows)]
            Inner::Pipe(stream) => {
                let guard = pipe_guard(stream)?;
                pipe_stream(&guard)?.set_nonblocking(nonblocking)
            }
            Inner::Tcp(stream) => stream.set_nonblocking(nonblocking),
        }
    }

    /// 设置写超时
    ///
    /// 同 [`Self::set_read_timeout`]：命名管道的 `Unsupported` 不算失败。
    ///
    /// # Errors
    ///
    /// 底层设置失败时返回错误
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match &self.inner {
            #[cfg(unix)]
            Inner::Unix(stream) => stream.set_write_timeout(timeout),
            #[cfg(windows)]
            Inner::Pipe(stream) => {
                let guard = pipe_guard(stream)?;
                unsupported_ok(pipe_stream(&guard)?.set_send_timeout(timeout))
            }
            Inner::Tcp(stream) => stream.set_write_timeout(timeout),
        }
    }

    /// 关闭读端与写端
    ///
    /// 命名管道没有半关闭：这里丢掉句柄（连同克隆出的读端一起）让对端读到
    /// EOF；已关闭时重复调用是无害的。
    pub fn shutdown(&self) {
        match &self.inner {
            #[cfg(unix)]
            Inner::Unix(stream) => {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
            #[cfg(windows)]
            Inner::Pipe(stream) => {
                if let Ok(mut guard) = stream.lock() {
                    let _ = guard.take();
                }
            }
            Inner::Tcp(stream) => {
                // 对端已关闭时 shutdown 返回 NotConnected，属正常情况。
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        }
    }
}

/// 命名管道句柄的互斥锁
#[cfg(windows)]
fn pipe_guard(
    stream: &Mutex<Option<PipeStream>>,
) -> io::Result<MutexGuard<'_, Option<PipeStream>>> {
    stream
        .lock()
        .map_err(|_| io::Error::other("plugin pipe lock is poisoned"))
}

/// 已关闭的管道不能继续读写
#[cfg(windows)]
fn pipe_stream<'a>(guard: &'a MutexGuard<'_, Option<PipeStream>>) -> io::Result<&'a PipeStream> {
    guard.as_ref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::BrokenPipe,
            "plugin pipe connection is closed",
        )
    })
}

/// 平台不支持的超时设置放行；其余错误照常返回
#[cfg(windows)]
fn unsupported_ok(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(err) if err.kind() == io::ErrorKind::Unsupported => Ok(()),
        other => other,
    }
}

impl Read for Connection {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.inner {
            #[cfg(unix)]
            Inner::Unix(stream) => stream.read(buf),
            #[cfg(windows)]
            Inner::Pipe(stream) => {
                let mut guard = pipe_guard(stream)?;
                match guard.as_mut() {
                    Some(stream) => stream.read(buf),
                    None => Err(closed_pipe()),
                }
            }
            Inner::Tcp(stream) => stream.read(buf),
        }
    }
}

impl Write for Connection {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut self.inner {
            #[cfg(unix)]
            Inner::Unix(stream) => stream.write(buf),
            #[cfg(windows)]
            Inner::Pipe(stream) => {
                let mut guard = pipe_guard(stream)?;
                match guard.as_mut() {
                    Some(stream) => stream.write(buf),
                    None => Err(closed_pipe()),
                }
            }
            Inner::Tcp(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.inner {
            #[cfg(unix)]
            Inner::Unix(stream) => stream.flush(),
            #[cfg(windows)]
            Inner::Pipe(stream) => {
                let mut guard = pipe_guard(stream)?;
                match guard.as_mut() {
                    Some(stream) => stream.flush(),
                    None => Err(closed_pipe()),
                }
            }
            Inner::Tcp(stream) => stream.flush(),
        }
    }
}

#[cfg(windows)]
fn closed_pipe() -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        "plugin pipe connection is closed",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_pair_carries_bytes_both_ways() {
        let (mut a, mut b) = Connection::pair().expect("pair");
        a.write_all(b"hello\n").expect("write");
        let mut buf = [0_u8; 6];
        b.read_exact(&mut buf).expect("read");
        assert_eq!(&buf, b"hello\n");
        b.shutdown();
        a.shutdown();
    }
}
