//! 端点连接的统一视图
//!
//! Unix socket 与回环 TCP 的读写接口相同，差异集中在构造函数里。上层
//! （握手、会话循环）只依赖本类型，不写 `cfg` 分支。

use std::io::{self, Read, Write};
use std::net::TcpStream;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// 一条已建立的插件连接
#[derive(Debug)]
pub struct Connection {
    inner: Inner,
}

#[derive(Debug)]
enum Inner {
    #[cfg(unix)]
    Unix(UnixStream),
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
            Inner::Tcp(stream) => Inner::Tcp(stream.try_clone()?),
        };
        Ok(Self { inner })
    }

    /// 设置读超时
    ///
    /// # Errors
    ///
    /// 底层设置失败时返回错误
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match &self.inner {
            #[cfg(unix)]
            Inner::Unix(stream) => stream.set_read_timeout(timeout),
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
            Inner::Tcp(stream) => stream.set_nonblocking(nonblocking),
        }
    }

    /// 设置写超时
    ///
    /// # Errors
    ///
    /// 底层设置失败时返回错误
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match &self.inner {
            #[cfg(unix)]
            Inner::Unix(stream) => stream.set_write_timeout(timeout),
            Inner::Tcp(stream) => stream.set_write_timeout(timeout),
        }
    }

    /// 关闭读端与写端
    pub fn shutdown(&self) {
        let result = match &self.inner {
            #[cfg(unix)]
            Inner::Unix(stream) => stream.shutdown(std::net::Shutdown::Both),
            Inner::Tcp(stream) => stream.shutdown(std::net::Shutdown::Both),
        };
        // 对端已关闭时 shutdown 返回 NotConnected，属正常情况。
        let _ = result;
    }
}

impl Read for Connection {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.inner {
            #[cfg(unix)]
            Inner::Unix(stream) => stream.read(buf),
            Inner::Tcp(stream) => stream.read(buf),
        }
    }
}

impl Write for Connection {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut self.inner {
            #[cfg(unix)]
            Inner::Unix(stream) => stream.write(buf),
            Inner::Tcp(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.inner {
            #[cfg(unix)]
            Inner::Unix(stream) => stream.flush(),
            Inner::Tcp(stream) => stream.flush(),
        }
    }
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
