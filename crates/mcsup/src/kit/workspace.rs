//! 工作目录任务：确保某个目录已登记
//!
//! 判空、marker 与注册都在 `service::WorkspaceService::init`，本模块只把
//! 「四种成功分支都是同一个结果」这一步收拢成 `Workspace`，让调用点不必
//! 每次都写一遍 `match`。

use super::error::KitError;
use domain::workspace::Workspace;
use service::InitOutcome;
use std::path::Path;

/// 工作目录任务句柄
///
/// 由 [`Kit::workspaces`](crate::kit::Kit::workspaces) 取得。工作目录任务不读
/// 默认策略，因此句柄只借组合根——不要为了「与其它句柄同形」塞一个用不到的
/// 配置字段进去。
#[derive(Clone, Copy, Debug)]
pub struct Workspaces<'a> {
    core: &'a crate::bootstrap::Mcsup,
}

impl<'a> Workspaces<'a> {
    pub(super) fn new(core: &'a crate::bootstrap::Mcsup) -> Self {
        Self { core }
    }
}

impl Workspaces<'_> {
    /// 确保 `dir` 是一个已登记的工作目录，返回它
    ///
    /// 目录已有合法 marker 或已登记时不会重复创建（`init` 自带恢复分支），
    /// 因此可以安全地重复调用。
    ///
    /// # Errors
    ///
    /// 目录非空且未登记、名称冲突、marker / 注册表写入失败时返回 [`KitError`]
    pub fn ensure(&self, dir: &Path, name: Option<&str>) -> Result<Workspace, KitError> {
        ensure(self.core, dir, name)
    }
}

/// 建服内部也要用同一条路径，因此逻辑落成自由函数，两处共用一份
pub(super) fn ensure(
    core: &crate::bootstrap::Mcsup,
    dir: &Path,
    name: Option<&str>,
) -> Result<Workspace, KitError> {
    let outcome = core.workspaces.init(dir, name)?;
    Ok(match outcome {
        InitOutcome::Created(workspace)
        | InitOutcome::Recovered(workspace)
        | InitOutcome::AlreadyRegistered(workspace)
        | InitOutcome::MarkerRestored(workspace) => workspace,
    })
}
