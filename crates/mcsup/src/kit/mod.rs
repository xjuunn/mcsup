//! 任务级接口：一行完成多步编排
//!
//! [`crate::bootstrap::Mcsup`] 回答「装好整套能力」，本模块回答「按什么顺序
//! 用它们」。每个方法都是一条完整任务，例如
//! [`Kit::create_server`](crate::kit::Kit::create_server) = 建目录 + 写 marker + 入库 + 装核心 +（可选）起服。
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
//! // 建服 + 装核心 + 起服
//! let created = kit
//!     .create_server(CreateServer::new("survival").version("1.21.4"))
//!     .await?;
//! // 装一个 mod（具体版本与落点由 plan 决定）
//! let installed = kit
//!     .install_mod("survival", "fabric-api", InstallMod::default())
//!     .await?;
//! println!("{} {} -> {}", created.core.server.meta.name, installed.plan.version_number, installed.plan.file);
//! # Ok(()) }
//! ```
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
mod error;
mod java;
mod mod_;
mod options;
mod server;

pub use backup::{BackupNow, ListBackups, PruneBackups, RestoreBackup};
pub use config::KitConfig;
pub use error::KitError;
pub use java::InstallJava;
pub use mod_::{InstallMod, InstallModOutcome, RemoveMod, UpdateMods, UpdateModsOutcome};
pub use options::RunOptions;
pub use server::{
    CreateServer, CreatedServer, InstallCore, InstallModpack, ModpackInstallOutcome, RemoveServer,
    StartServer, StopServer,
};

use crate::bootstrap::Mcsup;

/// 任务级接口句柄
///
/// 由 [`Mcsup::kit`] 构造。句柄本身不持有资源，也不改变 `Mcsup` 的装配；
/// 它只是「同一套服务 + 一份默认策略」，因此可以按需派生不同配置的句柄。
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
