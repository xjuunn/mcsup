//! 远端 supervisor：`port::SupervisorPort` 的 RPC 实现
//!
//! 与 `runtime::LocalSupervisor` 对偶：本地走 IPC 守护进程，远端走 transport
//! 帧。判定全部在对端 `service`（agent 的 ops 分发），这里只做参数搬运、
//! 解帧与错误映射——`ErrorKind → SupervisorError` 的映射出口只有
//! `remote_supervisor_error` 一个。
//!
//! `status_many` 覆写成**一次** `StatusMany` RPC（trait 默认实现是逐台
//! `status`，远端会变 N+1）；顺序与入参一致由帧契约保证。

use crate::remote::RemoteLink;
use port::{
    ServerRef, StopOutcome, StopSpec, SupervisorError, SupervisorPort, SupervisorStartOutcome,
    SupervisorStartRequest, SupervisorStatus,
};
use protocol::remote::wire::WireStatusEntry;
use protocol::remote::{ErrorKind, Request, Response};

/// `ErrorKind` → `SupervisorError`：远端 supervisor 的错误出口只有这一个
///
/// `busy`（含 `StopTimedOut` 的分类）→ `AlreadyRunning` 不成立——`BUSY` 也
/// 可能是「还在运行无法删除」这类状态冲突，按文案归 [`SupervisorError::Remote`]，
/// 由 `service` 的 `LifecycleError` 映射层再细分；这里只拆「确实能归位」的：
/// `not_found` / 协议错误 / 其余统一远端错误。
pub(crate) fn remote_supervisor_error(err: transport::TransportError) -> SupervisorError {
    match err {
        transport::TransportError::Remote { kind, message } => match kind.as_str() {
            ErrorKind::NOT_FOUND => SupervisorError::Unreachable(format!("not found: {message}")),
            ErrorKind::PROTOCOL => SupervisorError::Protocol(message),
            _ => SupervisorError::Remote { kind, message },
        },
        other => SupervisorError::Unreachable(other.to_string()),
    }
}

/// 服务器身份 → 线上 ID 文本
fn id_of(server: &ServerRef<'_>) -> String {
    server.id.as_str().to_owned()
}

/// 远端执行位置上的 supervisor
#[derive(Debug, Clone)]
pub struct RemoteSupervisor {
    link: RemoteLink,
}

impl RemoteSupervisor {
    /// 用一条远端连接构造
    #[must_use]
    pub fn new(link: RemoteLink) -> Self {
        Self { link }
    }

    /// 发一帧并要求非 `Error` 响应；`Error` 帧映射成 [`SupervisorError`]
    fn call(&self, request: &Request) -> Result<Response, SupervisorError> {
        let response = self
            .link
            .client_call(request)
            .map_err(remote_supervisor_error)?;
        match response {
            Response::Error { kind, message } => Err(SupervisorError::Remote { kind, message }),
            other => Ok(other),
        }
    }

    /// 测试专用：原始调用（不发业务判定，直接拿响应帧）
    #[cfg(test)]
    pub(crate) fn call_for_test(&self, request: &Request) -> Result<Response, SupervisorError> {
        self.link
            .client_call(request)
            .map_err(remote_supervisor_error)
    }
}

impl SupervisorPort for RemoteSupervisor {
    fn start(
        &self,
        request: SupervisorStartRequest,
    ) -> Result<SupervisorStartOutcome, SupervisorError> {
        let response = self.call(&Request::Start {
            server_id: request.server_id.as_str().to_owned(),
            ram_mib: request.ram_mib,
            accept_eula: request.accept_eula,
        })?;
        match response {
            Response::Started { outcome } => Ok(outcome.into()),
            other => Err(SupervisorError::Protocol(format!(
                "server.start returned an unexpected frame: {other:?}"
            ))),
        }
    }

    fn stop(&self, server: &ServerRef<'_>, spec: StopSpec) -> Result<StopOutcome, SupervisorError> {
        let response = self.call(&Request::Stop {
            server_id: id_of(server),
            grace_secs: spec.grace.as_secs(),
            force: spec.force,
        })?;
        match response {
            Response::Stopped { outcome } => Ok(outcome.into()),
            other => Err(SupervisorError::Protocol(format!(
                "server.stop returned an unexpected frame: {other:?}"
            ))),
        }
    }

    fn status(&self, server: &ServerRef<'_>) -> Result<Option<SupervisorStatus>, SupervisorError> {
        // 单台也走批量帧：一次往返，语义与 status_many 完全一致
        let statuses = self.status_many(std::slice::from_ref(server));
        statuses
            .into_iter()
            .next()
            .unwrap_or_else(|| Err(SupervisorError::Protocol("empty status reply".to_owned())))
    }

    fn status_many(
        &self,
        servers: &[ServerRef<'_>],
    ) -> Vec<Result<Option<SupervisorStatus>, SupervisorError>> {
        if servers.is_empty() {
            return Vec::new();
        }
        let ids: Vec<String> = servers
            .iter()
            .map(|server| server.id.as_str().to_owned())
            .collect();
        let response = match self.link.client_call(&Request::StatusMany { ids }) {
            Ok(response) => response,
            Err(err) => {
                let err = remote_supervisor_error(err);
                return servers.iter().map(|_| Err(clone_error(&err))).collect();
            }
        };
        let items = match response {
            Response::Statuses { items } => items,
            other => {
                let err = SupervisorError::Protocol(format!(
                    "status.many returned an unexpected frame: {other:?}"
                ));
                return servers.iter().map(|_| Err(clone_error(&err))).collect();
            }
        };
        if items.len() != servers.len() {
            let err = SupervisorError::Protocol(format!(
                "status.many returned {} entries for {} servers",
                items.len(),
                servers.len()
            ));
            return servers.iter().map(|_| Err(clone_error(&err))).collect();
        }
        servers
            .iter()
            .zip(items)
            .map(|(_, entry)| entry_into_status(entry))
            .collect()
    }

    fn send_line(&self, server: &ServerRef<'_>, line: &str) -> Result<(), SupervisorError> {
        let response = self.call(&Request::SendLine {
            server_id: id_of(server),
            line: line.to_owned(),
        })?;
        match response {
            Response::Ok => Ok(()),
            other => Err(SupervisorError::Protocol(format!(
                "server.send returned an unexpected frame: {other:?}"
            ))),
        }
    }

    fn shutdown(&self, server: &ServerRef<'_>) -> Result<(), SupervisorError> {
        // 通知本地关停：宽限 30 秒、不强杀，与本地实现的缺省口径一致
        self.stop(server, StopSpec::default_grace()).map(|_| ())
    }

    fn logs_tail(
        &self,
        server: &ServerRef<'_>,
        lines: usize,
    ) -> Result<Vec<String>, SupervisorError> {
        let response = self.call(&Request::LogsTail {
            server_id: id_of(server),
            lines,
        })?;
        match response {
            Response::LogLines { lines } => Ok(lines),
            other => Err(SupervisorError::Protocol(format!(
                "server.logs returned an unexpected frame: {other:?}"
            ))),
        }
    }
}

/// 克隆一个 supervisor 错误（`SupervisorError` 未实现 `Clone`，按变体重造）
fn clone_error(err: &SupervisorError) -> SupervisorError {
    match err {
        SupervisorError::AlreadyRunning => SupervisorError::AlreadyRunning,
        SupervisorError::Spawn(io) | SupervisorError::Io(io) => {
            SupervisorError::Spawn(std::io::Error::new(io.kind(), io.to_string()))
        }
        SupervisorError::Unreachable(text) => SupervisorError::Unreachable(text.clone()),
        SupervisorError::Protocol(text) => SupervisorError::Protocol(text.clone()),
        SupervisorError::Remote { kind, message } => SupervisorError::Remote {
            kind: kind.clone(),
            message: message.clone(),
        },
        _ => SupervisorError::Protocol(err.to_string()),
    }
}

/// 一条状态项 → 状态结果
fn entry_into_status(entry: WireStatusEntry) -> Result<Option<SupervisorStatus>, SupervisorError> {
    entry.into_result().map_err(|message| {
        // 单台查询失败：对端给的文案已含上下文；「不存在」归 Unreachable
        if message.starts_with("server not found") {
            SupervisorError::Unreachable(message)
        } else {
            SupervisorError::Remote {
                kind: ErrorKind::IO.to_owned(),
                message,
            }
        }
    })
}
