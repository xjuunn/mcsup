//! 任务级接口：一行完成多步编排
//!
//! [`crate::bootstrap::Mcsup`] 回答「装好整套能力」，本模块回答「按什么顺序
//! 用它们」。每个方法都是一条完整任务，例如
//! [`Servers::create`](crate::kit::Servers::create) = 建目录 + 写 marker + 入库 + 装核心 +（可选）起服。
//!
//! # 按领域取句柄
//!
//! [`Kit`](crate::kit::Kit) 本身只持有「同一套服务 + 一份默认策略」，任务按
//! 领域分组挂在访问器上，避免一个类型上堆满所有能力：
//!
//! | 访问器 | 句柄 | 覆盖任务 |
//! | --- | --- | --- |
//! | `kit.servers()` | [`Servers`](crate::kit::Servers) | 建服 / 整合包开服 / 装核心 / 起停重启 / 状态 / 删服 |
//! | `kit.backups()` | [`Backups`](crate::kit::Backups) | 建快照 / 列表 / 恢复 / 校验 / 删除 / 保留策略清理 |
//! | `kit.content()` | [`Content`](crate::kit::Content) | 装 mod / 检查更新 / 移除 |
//! | `kit.java()` | [`Java`](crate::kit::Java) | 装运行时 / 列出本机运行时 |
//! | `kit.workspaces()` | [`Workspaces`](crate::kit::Workspaces) | 工作目录初始化 |
//!
//! 子句柄只借用 `Kit`，可以连写：
//!
//! ```no_run
//! use std::path::Path;
//! use mcsup::bootstrap::Mcsup;
//! use mcsup::kit::{CreateServer, InstallMod, KitConfig};
//!
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! let mcsup = Mcsup::open(Path::new("/srv/mcsup"))?;
//! let kit = mcsup.kit(KitConfig {
//!     accept_eula: true,
//!     ram_mib: Some(4096),
//!     ..KitConfig::default() // 其余保持默认
//! });
//!
//! // 建服 + 装核心
//! let created = kit
//!     .servers()
//!     .create(CreateServer::new("survival").version("1.21.4"))
//!     .await?;
//! // 装一个 mod（具体版本与落点由 plan 决定）
//! let installed = kit
//!     .content()
//!     .install("survival", "fabric-api", InstallMod::default())
//!     .await?;
//! println!(
//!     "{} {} -> {}",
//!     created.core.server.meta.name, installed.plan.version_number, installed.plan.file
//! );
//! # Ok(()) }
//! ```
//!
//! # 三层配置
//!
//! ```text
//! 单次调用显式设置（CreateServer::version 等）
//!   ↓ 没设置
//! KitConfig（实例级默认）
//!   ↓ 没有默认
//! 内置默认（KitConfig::default）
//! ```
//!
//! 单次调用只写要改的字段，其余留 `None` 由 [`KitConfig`](crate::kit::KitConfig) 补齐；`KitConfig`
//! 自身用 `..Default::default()` 只改个别项。两层都遵循「没配置就用默认」，
//! 不需要为每个组合写一个构造函数。
//!
//! # 边界
//!
//! - **不承载判定**：状态机、EULA、安装可行性、运行中拒改磁盘都在
//!   `domain` / `service`；kit 只有调用顺序与默认值，没有第二套规则。
//! - **不吞掉可观测性**：长任务接受 [`RunOptions`](crate::kit::RunOptions) 的观察者与取消令牌，
//!   进度经 `port::TaskObserver` 出去，入口自己决定怎么渲染。
//! - **不藏副作用**：要顺手建工作目录必须显式写
//!   [`CreateServer::workspace_dir`](crate::kit::CreateServer::workspace_dir)；要代接受 EULA 必须显式打开。
//! - **不替代 [`crate::bootstrap::Mcsup`]**：kit 没包的操作仍可直接走
//!   `Mcsup` 暴露的各 service（[`Kit::core`](crate::kit::Kit::core)）。

mod backup;
mod config;
mod content;
mod error;
mod java;
mod options;
mod server;
mod workspace;

pub use backup::{BackupNow, Backups, ListBackups, PruneBackups, RestoreBackup};
pub use config::KitConfig;
pub use content::{
    Content, InstallMod, InstallModOutcome, RemoveMod, UpdateMods, UpdateModsOutcome,
};
pub use error::KitError;
pub use java::{InstallJava, Java};
pub use options::RunOptions;
pub use server::{
    CreateServer, CreatedServer, InstallCore, InstallModpack, ModpackInstallOutcome, RemoveServer,
    Servers, StartServer, StopServer,
};
pub use workspace::Workspaces;

use crate::bootstrap::Mcsup;

/// 任务级接口句柄
///
/// 由 [`Mcsup::kit`] 构造。句柄本身不持有资源，也不改变 `Mcsup` 的装配；
/// 它只是「同一套服务 + 一份默认策略」，因此可以按需派生不同配置的句柄。
///
/// 任务按领域经访问器取得（见[模块文档](crate::kit)），本类型只留策略与
/// 访问器；这样新增任务时落在明确的模块里，而不是继续堆在一个 `impl` 上。
#[derive(Clone, Debug)]
pub struct Kit<'a> {
    core: &'a Mcsup,
    config: KitConfig,
}

impl<'a> Kit<'a> {
    /// 用给定策略包住一个已装配的 [`Mcsup`]
    #[must_use]
    pub fn new(core: &'a Mcsup, config: KitConfig) -> Self {
        Self { core, config }
    }

    /// 当前使用的默认策略
    #[must_use]
    pub fn config(&self) -> &KitConfig {
        &self.config
    }

    /// 换一份默认策略，得到同源的新句柄
    #[must_use]
    pub fn with_config(&self, config: KitConfig) -> Self {
        Self {
            core: self.core,
            config,
        }
    }

    /// 底层的组合根：kit 没覆盖的操作从这里直接取 service
    #[must_use]
    pub fn core(&self) -> &'a Mcsup {
        self.core
    }

    /// 服务器任务：建服、核心、起停、状态、删服
    #[must_use]
    pub fn servers(&self) -> Servers<'_> {
        Servers::new(self.core, &self.config)
    }

    /// 备份任务：建快照、列表、恢复、校验、删除、清理
    #[must_use]
    pub fn backups(&self) -> Backups<'_> {
        Backups::new(self.core, &self.config)
    }

    /// 内容任务：装 mod / 插件、检查更新、移除
    #[must_use]
    pub fn content(&self) -> Content<'_> {
        Content::new(self.core, &self.config)
    }

    /// Java 任务：装运行时、列出本机可用运行时
    #[must_use]
    pub fn java(&self) -> Java<'_> {
        Java::new(self.core, &self.config)
    }

    /// 工作目录任务：初始化 / 复用工作目录
    #[must_use]
    pub fn workspaces(&self) -> Workspaces<'_> {
        Workspaces::new(self.core)
    }
}

impl Mcsup {
    /// 以 `config` 为默认策略创建任务级句柄
    ///
    /// `config` 传 [`KitConfig::default()`] 即全默认；只改个别项时用
    /// `KitConfig { ..Default::default() }`。
    #[must_use]
    pub fn kit(&self, config: KitConfig) -> Kit<'_> {
        Kit::new(self, config)
    }
}
