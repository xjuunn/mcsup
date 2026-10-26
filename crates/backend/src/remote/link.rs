//! 远端连接网关：一个远端对应一条 [`Client`]，三个镜像仓库共用
//!
//! 客户端侧的远端模型是「只读镜像 + 直连写」（`docs/remote.md`「数据与同步」）：
//! 读走 `list` 帧，写走对应操作帧。这里的职责只有三件事——搬运参数、解帧、
//! 把 [`ErrorKind`] 映射回 [`RepositoryError`]。任何「能不能做」的判定都在
//! 对端的 `service`，客户端不重复实现第二套。
//!
//! 错误映射是协议的一部分（`ops.rs` 的镜像操作）：`not_found` →
//! [`RepositoryError::NotFound`] 等。按 `kind` 映射而不是解析消息，与 agent
//! 侧的分类纪律一致。

use port::RepositoryError;
use protocol::remote::{ErrorKind, Request, Response};
use std::sync::Arc;
use transport::{Client, TransportError};

/// 一条到某个远端的连接网关
///
/// `Client` 内部按需建连、断线重试一次；网关只负责把帧发出去。克隆廉价的
/// `Arc<RemoteLink>` 让三个镜像仓库共享同一条连接池。
#[derive(Clone)]
pub struct RemoteLink {
    client: Arc<Client>,
}

impl std::fmt::Debug for RemoteLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteLink")
            .field("target", &self.client.target())
            .finish_non_exhaustive()
    }
}

impl RemoteLink {
    /// 用已保存的 token 构造（日常路径）
    #[must_use]
    pub fn new(
        target: impl Into<String>,
        fingerprint: transport::Fingerprint,
        token: &str,
    ) -> Self {
        Self {
            client: Arc::new(Client::new(target, fingerprint).with_token(token)),
        }
    }

    /// 未认证构造（仅连通性探测用；业务帧会被对端拒绝）
    #[must_use]
    pub fn anonymous(target: impl Into<String>, fingerprint: transport::Fingerprint) -> Self {
        Self {
            client: Arc::new(Client::new(target, fingerprint)),
        }
    }

    /// 连接目标
    #[must_use]
    pub fn target(&self) -> &str {
        self.client.target()
    }

    /// 握手信息（不需要认证）
    ///
    /// # Errors
    ///
    /// 建连、TLS 或协议失败时返回 [`TransportError`]
    pub fn hello(&self) -> Result<transport::Greeting, TransportError> {
        self.client.hello()
    }

    /// 执行一个操作，把 `Error` 帧映射成 [`RepositoryError`]
    ///
    /// # Errors
    ///
    /// 传输失败或对端拒绝时返回 [`RepositoryError::Storage`]；载荷解码失败
    /// 返回 [`RepositoryError::Corrupt`]
    pub fn call(&self, request: &Request) -> Result<Response, RepositoryError> {
        self.client.call(request).map_err(remote_error)
    }

    /// 执行一个操作并要求特定载荷；其余响应按协议错误处理
    ///
    /// # Errors
    ///
    /// 见 [`Self::call`]
    pub fn expect(
        &self,
        request: &Request,
        what: &'static str,
    ) -> Result<Response, RepositoryError> {
        let response = self.call(request)?;
        if matches!(response, Response::Error { .. }) {
            return Err(RepositoryError::Storage(Box::new(
                TransportError::Protocol(format!("expected {what}, got an error frame")),
            )));
        }
        Ok(response)
    }
}

/// `ErrorKind` → `RepositoryError`：镜像仓库的错误出口只有这一个
///
/// 镜像仓库是读路径的包装：对端的数据问题与本地记录损坏同类（[`RepositoryError::Corrupt`]），
/// 传输 / 权限 / 对端业务失败统一落 [`RepositoryError::Storage`]，由入口按
/// 链条最底层的 `TransportError` 给出提示与退出码。
pub(crate) fn remote_error(err: TransportError) -> RepositoryError {
    match err {
        TransportError::Remote { kind, message } => match kind.as_str() {
            ErrorKind::NOT_FOUND => RepositoryError::NotFound(message),
            ErrorKind::ALREADY_EXISTS => RepositoryError::AlreadyExists(message),
            ErrorKind::PROTOCOL => RepositoryError::Corrupt(message),
            _ => RepositoryError::Storage(Box::new(TransportError::Remote { kind, message })),
        },
        other => RepositoryError::Storage(Box::new(other)),
    }
}
