//! 远端实现：连接网关 + 三个只读镜像仓库
//!
//! 归属 `backend` 的原因：`docs/architecture.md`「Backend」——`backend` 是
//! 唯一允许组合实现层的 crate，远端一侧在这里把 `transport`（搬字节）与
//! `protocol`（帧集）组合成 `port` 同名能力的另一种实现，`service` 不感知
//! 执行位置。
//!
//! 模块拆分：
//!
//! - [`link`]：一条远端连接的网关（`RemoteLink`），错误映射的唯一出口
//! - [`mirror`]：三个只读镜像仓库（workspace / server / java）
//! - [`cache`]：镜像结果的本地缓存（离线读命令不再等建连超时）
//! - [`refresh`]：一次完整同步（拉四份列表 → 原子写缓存）
//!
//! [`RemoteBackend`] 是 `port::Backend` 的远端实现：supervisor 走 RPC
//! （[`RemoteSupervisor`]），installer 在 Installer 帧落地前不可用。

pub mod backup;
pub mod cache;
pub mod combined;
pub mod link;
pub mod mirror;
pub mod refresh;
pub mod router;
pub mod supervisor;

#[cfg(test)]
mod backup_tests;
#[cfg(test)]
mod router_tests;
#[cfg(test)]
mod supervisor_tests;

#[cfg(test)]
pub(crate) mod testkit;

pub use backup::{RemoteBackups, RoutingBackups};
pub use cache::{RefreshLock, RemoteCache, Snapshot, SyncState, TTL, now_secs};
pub use combined::{
    CombinedJavaRepository, CombinedServerRepository, CombinedWorkspaceRepository, RemoteMember,
    RemoteMembers,
};
pub use link::RemoteLink;
pub use mirror::{
    RemoteJavaRepository, RemoteModpackResult, RemoteServerRepository, RemoteWorkspaceRepository,
};
pub use router::RoutingSupervisor;
pub use supervisor::RemoteSupervisor;

/// 远端执行位置：`port::Backend` 的远端实现
///
/// supervisor 走 [`RemoteSupervisor`]；installer（`Forge` / `NeoForge`
/// 安装器走对端 `Installer` 帧）在本阶段未上线——对端帧未定义前返回
/// 协议错误，调用方按「远端不支持安装」处理。
#[derive(Debug, Clone)]
pub struct RemoteBackend {
    supervisor: RemoteSupervisor,
}

impl RemoteBackend {
    /// 用一条远端连接构造
    #[must_use]
    pub fn new(link: RemoteLink) -> Self {
        Self {
            supervisor: RemoteSupervisor::new(link),
        }
    }

    /// 连接网关（经 supervisor 取用）
    #[must_use]
    pub fn supervisor_rpc(&self) -> &RemoteSupervisor {
        &self.supervisor
    }
}

impl port::Backend for RemoteBackend {
    fn supervisor(&self) -> &dyn port::SupervisorPort {
        &self.supervisor
    }

    fn installer(&self) -> &dyn port::InstallerRunner {
        unreachable!("remote installer is not available until the Installer frames land")
    }
}
