//! 执行位置抽象：回答「这个操作在哪里执行」
//!
//! Backend 只承担 Local / Remote 的**执行位置**差异，不承担「能不能执行」
//! 的领域判定——后者在 `domain` / `service`。它把一组按位置变化的 Port 聚合成
//! 一个对象，`service` 只持有 `Arc<dyn Backend>`，不再逐项注入具体实现，也不
//! 出现 `if remote { ... } else { ... }`。
//!
//! 本 trait 只返回 `port` 内的契约类型与 `Arc<dyn ...>`，不含任何实现层类型，
//! 因此 `backend` 实现 crate 可以自由组合 `runtime` / `storage` / `backup`，
//! 而 `service` 对其一无所知。

use crate::{
    ContentStore, InstallerRunner, ServerRepository, SnapshotRepository, SupervisorPort,
    WorkspaceRepository,
};
use std::path::Path;
use std::sync::Arc;

/// 执行位置：Local（本机）/ Remote（经 transport → RPC → agent）
///
/// 实现层负责按位置装配各能力；共享的业务判定仍只在 `domain` / `service`。
pub trait Backend: Send + Sync {
    /// 进程生命周期（启动 / 停机 / 状态 / 控制台注入）
    fn supervisor(&self) -> &dyn SupervisorPort;

    /// 服务端安装器（Forge / `NeoForge`）
    fn installer(&self) -> &dyn InstallerRunner;

    /// 服务器元数据仓库（解析服务器目录）
    fn servers(&self) -> &dyn ServerRepository;

    /// 工作区元数据仓库
    fn workspaces(&self) -> &dyn WorkspaceRepository;

    /// 备份扫描器（目录 → 相对路径到内容哈希）
    fn scanner(&self) -> &dyn crate::Scanner;

    /// 按工作区备份根目录给出内容寻址存储
    ///
    /// 返回 `Arc` 是为了让 `service` 在一次备份编排内复用同一实例；实现内部
    /// 可以缓存，也可以每次新建。
    fn content_store(&self, backups_root: &Path) -> Arc<dyn ContentStore>;

    /// 按工作区备份根目录给出快照索引
    fn snapshot_repository(&self, backups_root: &Path) -> Arc<dyn SnapshotRepository>;
}
