//! 已建立连接上的帧收发
//!
//! 一个 [`Session`] 就是一条连接：TLS 之上按 `protocol::remote` 的行帧纪律
//! 一问一答。读侧用 [`BufReader`] 缓存，写侧直接写底层流——TLS 记录层自己会
//! 攒包，再套一层写缓冲只会让 `flush` 的时机变得难以推理。
//!
//! 请求与响应各有明确的发送 / 接收方法，而不是一个泛型 `send`：两端共用
//! 同一个 `Session`，若允许「客户端发响应」，状态机就只能靠运行时断言兜住。

use crate::error::TransportError;
use protocol::remote::{self, Request, Response};
use std::io::{BufReader, Read, Write};

/// 一条已完成的连接（TLS 已握手）
pub struct Session<S: Read + Write> {
    inner: BufReader<S>,
}

impl<S: Read + Write> std::fmt::Debug for Session<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session").finish_non_exhaustive()
    }
}

impl<S: Read + Write> Session<S> {
    /// 包装一条已握手的流
    pub fn new(stream: S) -> Self {
        Self {
            inner: BufReader::new(stream),
        }
    }

    /// 发一帧请求（客户端侧）
    ///
    /// # Errors
    ///
    /// 写入失败时返回 [`TransportError::Io`]
    pub fn send_request(&mut self, request: &Request) -> Result<(), TransportError> {
        remote::write_request(self.inner.get_mut(), request)
            .map_err(|err| TransportError::io(format!("send {} frame", request.operation()), err))
    }

    /// 收一帧响应（客户端侧）
    ///
    /// # Errors
    ///
    /// 读取失败返回 [`TransportError::Io`]；帧非法或版本不匹配返回
    /// [`TransportError::Protocol`]
    pub fn recv_response(&mut self) -> Result<Response, TransportError> {
        remote::read_response(&mut self.inner).map_err(classify_read_error)
    }

    /// 发一帧响应（服务端侧）
    ///
    /// # Errors
    ///
    /// 写入失败时返回 [`TransportError::Io`]
    pub fn send_response(&mut self, response: &Response) -> Result<(), TransportError> {
        remote::write_response(self.inner.get_mut(), response)
            .map_err(|err| TransportError::io("send response frame", err))
    }

    /// 收一帧请求（服务端侧）
    ///
    /// # Errors
    ///
    /// 读取失败返回 [`TransportError::Io`]；帧非法或版本不匹配返回
    /// [`TransportError::Protocol`]
    pub fn recv_request(&mut self) -> Result<Request, TransportError> {
        remote::read_request(&mut self.inner).map_err(classify_read_error)
    }

    /// 客户端：发一帧、收一帧
    ///
    /// # Errors
    ///
    /// 同 [`Self::send_request`] 与 [`Self::recv_response`]
    pub fn exchange(&mut self, request: &Request) -> Result<Response, TransportError> {
        self.send_request(request)?;
        self.recv_response()
    }

    /// 拆回底层流（连接复用到别的用途时）
    pub fn into_inner(self) -> S {
        self.inner.into_inner()
    }

    /// 底层流引用
    pub fn get_ref(&self) -> &S {
        self.inner.get_ref()
    }
}

/// 把「帧内容坏了」与「连接断了」分开
///
/// 入口的退出码不同：前者是对端版本 / 实现有问题，后者是网络问题且值得重试。
fn classify_read_error(err: std::io::Error) -> TransportError {
    match err.kind() {
        std::io::ErrorKind::InvalidData => TransportError::Protocol(err.to_string()),
        _ => TransportError::io("read frame", err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// 预置一段字节的只读流：验证读侧的解码与错误分类
    fn reader(bytes: Vec<u8>) -> Session<Cursor<Vec<u8>>> {
        Session::new(Cursor::new(bytes))
    }

    #[test]
    fn decodes_a_written_response() {
        let mut encoded = Vec::new();
        remote::write_response(&mut encoded, &Response::Ok).expect("encode");
        let mut session = reader(encoded);
        assert!(matches!(
            session.recv_response().expect("recv"),
            Response::Ok
        ));
    }

    #[test]
    fn decodes_a_written_request() {
        let mut encoded = Vec::new();
        remote::write_request(&mut encoded, &Request::Ping).expect("encode");
        let mut session = reader(encoded);
        assert!(matches!(
            session.recv_request().expect("recv"),
            Request::Ping
        ));
    }

    /// 非法帧必须是 Protocol 而不是 Io：入口据此区分「对端坏了」与「网络断了」
    #[test]
    fn garbage_is_a_protocol_error() {
        let mut session = reader(b"not a frame\n".to_vec());
        let err = session.recv_response().expect_err("garbage");
        assert!(matches!(err, TransportError::Protocol(_)), "{err:?}");
    }

    #[test]
    fn closed_stream_is_an_io_error() {
        let mut session = reader(Vec::new());
        let err = session.recv_response().expect_err("eof");
        assert!(matches!(err, TransportError::Io { .. }), "{err:?}");
        assert!(err.is_retryable());
    }
}
