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

use port::{RepositoryError, TaskObserver};
use protocol::remote::{ErrorKind, Request, Response};
use std::sync::{Arc, Mutex};
use transport::{Client, Fingerprint, TransportError};

/// 远端地址的来历
///
/// 直连接入的地址在配置里就是最终形态（`host:port`）；SSH 接入的地址要等
/// 隧道建好才知道（远端 agent 每次监听随机端口），因此只能是**延迟解析**：
/// 装配期不连、第一次发帧时才连。这保证「只操作本地资源」的命令不会因为
/// 一台离线远端而卡在 SSH 超时上。
enum Address {
    /// 配置里的地址，直接可用
    Fixed(String),
    /// 每次会话建立后才知道的地址（SSH 隧道）
    Lazy(Arc<dyn Fn() -> Result<String, TransportError> + Send + Sync>),
}

/// 一条到某个远端的连接网关
///
/// `Client` 内部按需建连、断线重试一次；网关只负责把帧发出去。克隆廉价的
/// `Arc<RemoteLink>` 让三个镜像仓库共享同一条连接池。
#[derive(Clone)]
pub struct RemoteLink {
    inner: Arc<Inner>,
}

/// 网关的可共享状态
///
/// `client` 是 `Mutex<Option<_>>` 而不是 `OnceLock`：解析失败（隧道建不起来）
/// 必须是**可重试**的——远端暂时离线不该把这条记录永久判死，下一次命令
/// 应当重新尝试建连。
struct Inner {
    /// 日志与错误信息里指代这台远端的名字（`host:port` 或 `user@host`）
    label: String,
    address: Address,
    fingerprint: Fingerprint,
    token: Option<String>,
    /// 已解析出的客户端（同一进程内复用其连接池）
    client: Mutex<Option<Arc<Client>>>,
}

impl std::fmt::Debug for RemoteLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteLink")
            .field("target", &self.inner.label)
            .finish_non_exhaustive()
    }
}

impl RemoteLink {
    /// 用已保存的 token 构造（日常路径）
    #[must_use]
    pub fn new(target: impl Into<String>, fingerprint: Fingerprint, token: &str) -> Self {
        let target = target.into();
        Self {
            inner: Arc::new(Inner {
                label: target.clone(),
                address: Address::Fixed(target),
                fingerprint,
                token: Some(token.to_owned()),
                client: Mutex::new(None),
            }),
        }
    }

    /// 延迟解析地址的构造：`label` 只用于日志，`resolve` 在第一次发帧时调用
    #[must_use]
    pub fn lazy(
        label: impl Into<String>,
        fingerprint: Fingerprint,
        token: &str,
        resolve: impl Fn() -> Result<String, TransportError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                label: label.into(),
                address: Address::Lazy(Arc::new(resolve)),
                fingerprint,
                token: Some(token.to_owned()),
                client: Mutex::new(None),
            }),
        }
    }

    /// 未认证构造（仅连通性探测用；业务帧会被对端拒绝）
    #[must_use]
    pub fn anonymous(target: impl Into<String>, fingerprint: Fingerprint) -> Self {
        let target = target.into();
        Self {
            inner: Arc::new(Inner {
                label: target.clone(),
                address: Address::Fixed(target),
                fingerprint,
                token: None,
                client: Mutex::new(None),
            }),
        }
    }

    /// 日志里指代这台远端的名字（`host:port` 或 `user@host`，不是隧道地址）
    #[must_use]
    pub fn target(&self) -> &str {
        &self.inner.label
    }

    /// 取（必要时构造）客户端
    ///
    /// 解析地址失败要能被下一次调用重新尝试，因此失败时不缓存任何东西。
    fn client(&self) -> Result<Arc<Client>, TransportError> {
        let mut guard = self
            .inner
            .client
            .lock()
            .map_err(|_| TransportError::Protocol("client cache lock poisoned".to_owned()))?;
        if let Some(client) = guard.as_ref() {
            return Ok(Arc::clone(client));
        }
        let target = match &self.inner.address {
            Address::Fixed(target) => target.clone(),
            Address::Lazy(resolve) => resolve()?,
        };
        let mut client = Client::new(target, self.inner.fingerprint.clone());
        if let Some(token) = self.inner.token.as_deref() {
            client = client.with_token(token);
        }
        let client = Arc::new(client);
        *guard = Some(Arc::clone(&client));
        Ok(client)
    }

    /// 握手信息（不需要认证）
    ///
    /// # Errors
    ///
    /// 建连、TLS 或协议失败时返回 [`TransportError`]
    pub fn hello(&self) -> Result<transport::Greeting, TransportError> {
        self.client()?.hello()
    }

    /// 原始调用：不经过 `RepositoryError` 映射
    ///
    /// supervisor / installer 的错误模型是 [`port::SupervisorError`] 而不是
    /// [`RepositoryError`]，各实现自己做映射；网关只提供未加工的出口。
    ///
    /// # Errors
    ///
    /// 传输失败或对端拒绝时返回 [`TransportError`]
    pub fn client_call(&self, request: &Request) -> Result<Response, TransportError> {
        self.client()?.call(request)
    }

    /// 执行一个操作，把错误帧 / 传输失败映射成 [`RepositoryError`]
    ///
    /// 业务错误帧（`not_found` / `already_exists` / `invalid` …）在这里就转成
    /// 对应的 [`RepositoryError`] 变体，**保留 kind**；调用方不应再看到
    /// `Response::Error`。这样镜像仓库与 supervisor / backup 的错误分类口径一致。
    ///
    /// # Errors
    ///
    /// 传输失败或对端拒绝时返回 [`RepositoryError`]；载荷解码失败返回
    /// [`RepositoryError::Corrupt`]
    pub fn call(&self, request: &Request) -> Result<Response, RepositoryError> {
        let response = self
            .client()
            .map_err(remote_error)?
            .call(request)
            .map_err(remote_error)?;
        match response {
            Response::Error { kind, message } => Err(repository_error(&kind, &message)),
            other => Ok(other),
        }
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
        // `call` 已把业务错误帧转成 Err；这里只防漏网，不丢 kind
        if matches!(response, Response::Error { .. }) {
            return Err(RepositoryError::Storage(Box::new(
                TransportError::Protocol(format!("expected {what}, got an error frame")),
            )));
        }
        Ok(response)
    }

    /// 执行一个长任务操作，把对端回传的中间帧交给 `observer`
    ///
    /// 与 [`Self::call`] 的区别只有一点：读到 `Response::Task` 时不当作结果，
    /// 而是转给同一个 [`TaskObserver`]。本地安装与远端安装因此共用同一套
    /// 渲染代码——观察者是入口侧的同一个对象。
    ///
    /// # Errors
    ///
    /// 见 [`Self::call`]
    pub fn call_streaming(
        &self,
        request: &Request,
        observer: &dyn TaskObserver,
    ) -> Result<Response, RepositoryError> {
        let response = self
            .client()
            .map_err(remote_error)?
            .call_streaming(request, observer)
            .map_err(remote_error)?;
        match response {
            Response::Error { kind, message } => Err(repository_error(&kind, &message)),
            other => Ok(other),
        }
    }

    /// [`Self::expect`] 的流式版本
    ///
    /// # Errors
    ///
    /// 见 [`Self::call_streaming`]
    pub fn expect_streaming(
        &self,
        request: &Request,
        what: &'static str,
        observer: &dyn TaskObserver,
    ) -> Result<Response, RepositoryError> {
        let response = self.call_streaming(request, observer)?;
        if matches!(response, Response::Error { .. }) {
            return Err(RepositoryError::Storage(Box::new(
                TransportError::Protocol(format!("expected {what}, got an error frame")),
            )));
        }
        Ok(response)
    }
}

/// 业务错误帧（`ErrorKind` + message）→ [`RepositoryError`]
///
/// 镜像仓库与 supervisor / backup 共用的唯一分类出口：按 `kind` 映射，不看文案。
fn repository_error(kind: &str, message: &str) -> RepositoryError {
    match kind {
        ErrorKind::NOT_FOUND => RepositoryError::NotFound(message.to_owned()),
        ErrorKind::ALREADY_EXISTS => RepositoryError::AlreadyExists(message.to_owned()),
        ErrorKind::PROTOCOL => RepositoryError::Corrupt(message.to_owned()),
        _ => RepositoryError::Storage(Box::new(TransportError::Remote {
            kind: kind.to_owned(),
            message: message.to_owned(),
        })),
    }
}

/// `ErrorKind` → `RepositoryError`：镜像仓库的错误出口只有这一个
///
/// 镜像仓库是读路径的包装：对端的数据问题与本地记录损坏同类（[`RepositoryError::Corrupt`]），
/// 传输 / 权限 / 对端业务失败统一落 [`RepositoryError::Storage`]，由入口按
/// 链条最底层的 `TransportError` 给出提示与退出码。
pub(crate) fn remote_error(err: TransportError) -> RepositoryError {
    match err {
        TransportError::Remote { kind, message } => repository_error(&kind, &message),
        other => RepositoryError::Storage(Box::new(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::testkit::PairingHarness;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 延迟解析的地址只在第一次发帧时算，之后复用同一个客户端
    ///
    /// SSH 接入靠这条性质：一条命令里组合仓库、supervisor、备份执行器都要
    /// 用同一条连接，重复解析等于重复起 ssh 会话。
    #[test]
    fn lazy_link_resolves_once_and_reuses_the_client() {
        let harness = PairingHarness::spawn();
        let target = harness.agent.target.clone();
        let resolves = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&resolves);
        let link = RemoteLink::lazy(
            "ssh-box",
            harness.agent.fingerprint.clone(),
            &harness.agent.token,
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(target.clone())
            },
        );

        link.client_call(&Request::Ping).expect("first ping");
        link.client_call(&Request::Ping).expect("second ping");
        assert_eq!(
            resolves.load(Ordering::SeqCst),
            1,
            "地址只应解析一次（重复解析等于重复建会话）"
        );
        assert_eq!(
            link.target(),
            "ssh-box",
            "日志名必须是 SSH 目标而不是隧道端口"
        );
    }

    /// 解析失败不缓存：远端恢复后下一次调用重新解析
    ///
    /// 离线是常态（`docs/remote.md`「设计原则 4」），一次失败不能把这条记录
    /// 在进程内永久判死。
    #[test]
    fn lazy_link_retries_resolution_after_a_failure() {
        let harness = PairingHarness::spawn();
        let target = harness.agent.target.clone();
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let link = RemoteLink::lazy(
            "ssh-box",
            harness.agent.fingerprint.clone(),
            &harness.agent.token,
            move || {
                if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err(TransportError::Protocol("tunnel failed".to_owned()));
                }
                Ok(target.clone())
            },
        );

        let first = link.client_call(&Request::Ping);
        assert!(first.is_err(), "第一次解析失败必须报错，不能静默重试");
        link.client_call(&Request::Ping).expect("retry succeeds");
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            2,
            "失败后必须重新解析，而不是把失败结果缓存下来"
        );
    }
}
