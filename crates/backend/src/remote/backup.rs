//! 远端备份执行：`port::RemoteBackupPort` 的 RPC 实现
//!
//! 与 [`super::supervisor::RemoteSupervisor`] 对偶：跨线只走元数据帧
//! （`WireSnapshot`），扫描、CAS、恢复写盘全部发生在对端——远端的
//! `BackupLayout` 依赖对端目录事实，客户端拼不出来。判定全部在对端
//! `service`（agent 的 ops 分发），这里只做参数搬运、解帧与错误映射。

use crate::remote::RemoteLink;
use port::{
    RemoteBackupCreate, RemoteBackupError, RemoteBackupPort, RemoteCreateOutcome,
    RemoteDeleteOutcome, RemotePruneOutcome, RemoteRestoreOutcome, RemoteSnapshotPage,
    RemoteVerifyOutcome,
};
use protocol::remote::wire::WireSnapshot;
use protocol::remote::{ErrorKind, Request, Response};
use std::sync::Arc;

/// `ErrorKind` → `RemoteBackupError`：远端备份的错误出口只有这一个
fn remote_backup_error(err: transport::TransportError) -> RemoteBackupError {
    match err {
        transport::TransportError::Remote { kind, message } => match kind.as_str() {
            ErrorKind::NOT_FOUND => RemoteBackupError::NotFound(message),
            ErrorKind::INVALID => RemoteBackupError::Invalid(message),
            ErrorKind::PROTOCOL => RemoteBackupError::Protocol(message),
            _ => RemoteBackupError::Io(message),
        },
        other => RemoteBackupError::Unreachable(other.to_string()),
    }
}

/// 远端备份执行器
#[derive(Debug, Clone)]
pub struct RemoteBackups {
    link: RemoteLink,
}

/// 按服务器来源在多台远端之间分派备份
///
/// 与 [`super::router::RoutingSupervisor`] 同构：按服务器 id 查组合仓库
/// 来源，远端条目转给对应 [`RemoteBackups`]。本地条目不该走到这里——
/// `service` 的路由层先按来源分叉，本地走 `BackupStore`。
#[derive(Clone)]
pub struct RoutingBackups {
    remotes: std::collections::HashMap<port::RemoteName, RemoteBackups>,
    /// 组合服务器仓库：id → 来源的唯一真相源
    servers: Arc<dyn port::ServerRepository>,
}

impl std::fmt::Debug for RoutingBackups {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoutingBackups")
            .field("remotes", &self.remotes.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl RoutingBackups {
    /// 装配：远端名 → 执行器，加查来源用的组合仓库
    #[must_use]
    pub fn new(
        remotes: std::collections::HashMap<port::RemoteName, RemoteBackups>,
        servers: Arc<dyn port::ServerRepository>,
    ) -> Self {
        Self { remotes, servers }
    }

    /// 按 id 解析远端执行器
    fn resolve(
        &self,
        id: &domain::server::meta::ServerId,
    ) -> Result<&RemoteBackups, RemoteBackupError> {
        let sourced = self
            .servers
            .list_sourced()
            .map_err(|err| RemoteBackupError::Unreachable(err.to_string()))?;
        let origin = sourced
            .into_iter()
            .find(|entry| entry.value.id == *id)
            .map(|entry| entry.origin)
            .ok_or_else(|| {
                RemoteBackupError::NotFound(format!("no such server: {}", id.as_str()))
            })?;
        match origin {
            port::Origin::Local => Err(RemoteBackupError::Unreachable(format!(
                "server {} is local; remote backups do not apply",
                id.as_str()
            ))),
            port::Origin::Remote(name) => self.remotes.get(&name).ok_or_else(|| {
                RemoteBackupError::Unreachable(format!(
                    "server {} belongs to remote '{}', which is not reachable",
                    id.as_str(),
                    name.as_str()
                ))
            }),
        }
    }
}

impl RemoteBackupPort for RoutingBackups {
    fn create(
        &self,
        server_id: &domain::server::meta::ServerId,
        options: &RemoteBackupCreate,
    ) -> Result<RemoteCreateOutcome, RemoteBackupError> {
        self.resolve(server_id)?.create(server_id, options)
    }

    fn list(
        &self,
        server_id: &domain::server::meta::ServerId,
        page: domain::backup::Page,
    ) -> Result<RemoteSnapshotPage, RemoteBackupError> {
        self.resolve(server_id)?.list(server_id, page)
    }

    fn restore(
        &self,
        server_id: &domain::server::meta::ServerId,
        snapshot_id: &str,
        to_new: Option<&str>,
    ) -> Result<RemoteRestoreOutcome, RemoteBackupError> {
        self.resolve(server_id)?
            .restore(server_id, snapshot_id, to_new)
    }

    fn delete(
        &self,
        server_id: &domain::server::meta::ServerId,
        snapshot_id: &str,
    ) -> Result<RemoteDeleteOutcome, RemoteBackupError> {
        self.resolve(server_id)?.delete(server_id, snapshot_id)
    }

    fn verify(
        &self,
        server_id: &domain::server::meta::ServerId,
        snapshot_id: &str,
    ) -> Result<RemoteVerifyOutcome, RemoteBackupError> {
        self.resolve(server_id)?.verify(server_id, snapshot_id)
    }

    fn prune(
        &self,
        server_id: &domain::server::meta::ServerId,
        policy: Option<domain::backup::RetentionPolicy>,
        dry_run: bool,
    ) -> Result<RemotePruneOutcome, RemoteBackupError> {
        self.resolve(server_id)?.prune(server_id, policy, dry_run)
    }
}

impl RemoteBackups {
    /// 用一条远端连接构造
    #[must_use]
    pub fn new(link: RemoteLink) -> Self {
        Self { link }
    }

    /// 发一帧并解 `Error` 帧；协议之外的响应交给调用方按帧族处理
    fn call(&self, request: &Request) -> Result<Response, RemoteBackupError> {
        let response = self
            .link
            .client_call(request)
            .map_err(remote_backup_error)?;
        match response {
            Response::Error { kind, message } => Err(match kind.as_str() {
                ErrorKind::NOT_FOUND => RemoteBackupError::NotFound(message),
                ErrorKind::INVALID => RemoteBackupError::Invalid(message),
                ErrorKind::PROTOCOL => RemoteBackupError::Protocol(message),
                _ => RemoteBackupError::Io(message),
            }),
            other => Ok(other),
        }
    }

    /// 响应里必带的快照：解码失败按对端协议错误上报
    fn decode_snapshot(wire: WireSnapshot) -> Result<domain::backup::Snapshot, RemoteBackupError> {
        domain::backup::Snapshot::try_from(wire)
            .map_err(|err| RemoteBackupError::Protocol(err.to_string()))
    }
}

impl RemoteBackupPort for RemoteBackups {
    fn create(
        &self,
        server_id: &domain::server::meta::ServerId,
        options: &RemoteBackupCreate,
    ) -> Result<RemoteCreateOutcome, RemoteBackupError> {
        let response = self.call(&Request::BackupCreate {
            server_id: server_id.as_str().to_owned(),
            label: options.label.clone(),
            scope: options.scope.as_ref().map(|scope| scope.entries().to_vec()),
            no_reuse: options.no_reuse,
            flush_wait_ms: options.flush_wait_ms,
        })?;
        match response {
            Response::SnapshotCreated { snapshot, skipped } => Ok(RemoteCreateOutcome {
                snapshot: Self::decode_snapshot(snapshot)?,
                skipped,
            }),
            other => Err(RemoteBackupError::Protocol(format!(
                "backup.create returned an unexpected frame: {other:?}"
            ))),
        }
    }

    fn list(
        &self,
        server_id: &domain::server::meta::ServerId,
        page: domain::backup::Page,
    ) -> Result<RemoteSnapshotPage, RemoteBackupError> {
        let response = self.call(&Request::BackupList {
            server_id: server_id.as_str().to_owned(),
            page: page.number,
            page_size: page.size,
        })?;
        match response {
            Response::Snapshots {
                items,
                total,
                page,
                page_size,
            } => {
                let mut out = Vec::with_capacity(items.len());
                for wire in items {
                    out.push(Self::decode_snapshot(wire)?);
                }
                Ok(RemoteSnapshotPage {
                    items: out,
                    total,
                    page,
                    page_size,
                })
            }
            other => Err(RemoteBackupError::Protocol(format!(
                "backup.list returned an unexpected frame: {other:?}"
            ))),
        }
    }

    fn restore(
        &self,
        server_id: &domain::server::meta::ServerId,
        snapshot_id: &str,
        to_new: Option<&str>,
    ) -> Result<RemoteRestoreOutcome, RemoteBackupError> {
        let response = self.call(&Request::BackupRestore {
            server_id: server_id.as_str().to_owned(),
            snapshot: snapshot_id.to_owned(),
            to_new: to_new.map(str::to_owned),
        })?;
        match response {
            Response::Restored {
                snapshot,
                destination,
                files,
                bytes,
                safety_snapshot,
                deleted,
            } => Ok(RemoteRestoreOutcome {
                snapshot: Self::decode_snapshot(snapshot)?,
                destination,
                files,
                bytes,
                safety_snapshot: safety_snapshot
                    .map(|raw| {
                        domain::backup::SnapshotId::parse(&raw)
                            .map_err(|err| RemoteBackupError::Protocol(err.to_string()))
                    })
                    .transpose()?,
                deleted,
            }),
            other => Err(RemoteBackupError::Protocol(format!(
                "backup.restore returned an unexpected frame: {other:?}"
            ))),
        }
    }

    fn delete(
        &self,
        server_id: &domain::server::meta::ServerId,
        snapshot_id: &str,
    ) -> Result<RemoteDeleteOutcome, RemoteBackupError> {
        let response = self.call(&Request::BackupDelete {
            server_id: server_id.as_str().to_owned(),
            snapshot: snapshot_id.to_owned(),
        })?;
        match response {
            Response::SnapshotDeleted { snapshot, orphaned } => Ok(RemoteDeleteOutcome {
                snapshot: Self::decode_snapshot(snapshot)?,
                orphaned,
            }),
            other => Err(RemoteBackupError::Protocol(format!(
                "backup.delete returned an unexpected frame: {other:?}"
            ))),
        }
    }

    fn verify(
        &self,
        server_id: &domain::server::meta::ServerId,
        snapshot_id: &str,
    ) -> Result<RemoteVerifyOutcome, RemoteBackupError> {
        let response = self.call(&Request::BackupVerify {
            server_id: server_id.as_str().to_owned(),
            snapshot: snapshot_id.to_owned(),
        })?;
        match response {
            Response::SnapshotVerified {
                snapshot,
                objects,
                bytes,
                broken,
            } => {
                let mut hashes = Vec::with_capacity(broken.len());
                for raw in broken {
                    hashes.push(
                        domain::backup::ContentHash::parse(&raw)
                            .map_err(|err| RemoteBackupError::Protocol(err.to_string()))?,
                    );
                }
                Ok(RemoteVerifyOutcome {
                    snapshot: Self::decode_snapshot(snapshot)?,
                    objects,
                    bytes,
                    broken: hashes,
                })
            }
            other => Err(RemoteBackupError::Protocol(format!(
                "backup.verify returned an unexpected frame: {other:?}"
            ))),
        }
    }

    fn prune(
        &self,
        server_id: &domain::server::meta::ServerId,
        policy: Option<domain::backup::RetentionPolicy>,
        dry_run: bool,
    ) -> Result<RemotePruneOutcome, RemoteBackupError> {
        let (keep, older_than_secs) = match policy {
            Some(domain::backup::RetentionPolicy::Keep(n)) => (Some(n), None),
            Some(domain::backup::RetentionPolicy::OlderThan(secs)) => (None, Some(secs)),
            None => (None, None),
        };
        let response = self.call(&Request::BackupPrune {
            server_id: server_id.as_str().to_owned(),
            keep,
            older_than_secs,
            dry_run,
        })?;
        match response {
            Response::Pruned {
                removed,
                objects_reclaimed,
                bytes_reclaimed,
                dry_run,
            } => {
                let mut out = Vec::with_capacity(removed.len());
                for wire in removed {
                    out.push(Self::decode_snapshot(wire)?);
                }
                Ok(RemotePruneOutcome {
                    removed: out,
                    objects_reclaimed,
                    bytes_reclaimed,
                    dry_run,
                })
            }
            other => Err(RemoteBackupError::Protocol(format!(
                "backup.prune returned an unexpected frame: {other:?}"
            ))),
        }
    }
}
