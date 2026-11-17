//! 任务级接口的错误
//!
//! 错误分类由各层自己给出（[`KitError::category`] 逐层委托），入口只把分类
//! 翻成退出码 / HTTP 状态码。kit 不重写判定：它只补两类错误——「调用方少给了
//! 必填项」与「这个实例没有承载该变体的来源」，两者都是用法问题。

use port::ErrorCategory;
use service::{BackupError, LifecycleError, ModError, ServerRegistryError, WorkspaceError};
use thiserror::Error;

/// 任务级接口失败
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum KitError {
    /// 工作目录操作失败
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    /// 服务器注册表操作失败
    #[error(transparent)]
    Registry(#[from] ServerRegistryError),
    /// 生命周期 / 安装 / 起服失败
    #[error(transparent)]
    Lifecycle(#[from] LifecycleError),
    /// 备份失败
    #[error(transparent)]
    Backup(#[from] BackupError),
    /// mod 编排失败
    #[error(transparent)]
    Mod(#[from] ModError),
    /// 没有给服务器名称
    #[error("a server name is required: call CreateServer::new(name)")]
    MissingName,
    /// 本实例没有承载该变体的来源
    #[error(
        "no server jar source is registered for variant `{variant}`; \
         inject one with McsupBuilder::jar_sources"
    )]
    UnsupportedVariant {
        /// 变体的稳定机器名
        variant: String,
    },
    /// 需要拉起 supervisor，但没有可执行文件，且本实例没有注入 supervisor
    #[error(
        "starting a server needs a supervisor executable; set KitConfig::supervisor_exe \
         or inject your own SupervisorPort with McsupBuilder::supervisor"
    )]
    MissingSupervisorExe,
    /// 建服即起服，但没有同意 EULA
    #[error(
        "creating a server with start = true needs an accepted EULA; \
         set accept_eula on the call or in KitConfig (the library never accepts it for you)"
    )]
    EulaNotAccepted,
}

impl KitError {
    /// 失败分类
    ///
    /// 包装变体一律委托给下层：kit 再判一次就会出现两套口径。
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        match self {
            Self::Workspace(err) => err.category(),
            Self::Registry(err) => err.category(),
            Self::Lifecycle(err) => err.category(),
            Self::Backup(err) => err.category(),
            Self::Mod(err) => err.category(),
            // 三项都是「调用方给的输入不成立」，改参数即可成功
            Self::MissingName | Self::UnsupportedVariant { .. } | Self::EulaNotAccepted => {
                ErrorCategory::Usage
            }
            // 缺 supervisor 是可配置项错误，与「后端不可用」不同：补配置即可
            Self::MissingSupervisorExe => ErrorCategory::Usage,
        }
    }
}
