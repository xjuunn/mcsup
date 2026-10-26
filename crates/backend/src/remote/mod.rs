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
//!
//! [`RemoteBackend`]（`port::Backend` 的远端实现：supervisor / installer 走
//! RPC）在阶段 8 落地，同属本模块。

pub mod link;
pub mod mirror;

#[cfg(test)]
pub(crate) mod testkit;

pub use link::RemoteLink;
pub use mirror::{RemoteJavaRepository, RemoteServerRepository, RemoteWorkspaceRepository};

/// 远端执行位置（阶段 8：`SupervisorPort` / `InstallerRunner` 走 RPC）
///
/// 现在先占住名字与装配口，让组合仓库可以提前引用；能力在阶段 8 补齐。
#[derive(Debug, Clone)]
pub struct RemoteBackend {
    link: RemoteLink,
}

impl RemoteBackend {
    /// 用一条远端连接构造
    #[must_use]
    pub fn new(link: RemoteLink) -> Self {
        Self { link }
    }

    /// 连接网关
    #[must_use]
    pub fn link(&self) -> &RemoteLink {
        &self.link
    }
}
