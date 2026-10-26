//! Fabric 官方 Meta 来源
//!
//! 对外导出 [`FabricSource`]（默认对接官方）与 [`FabricSourceBuilder`] /
//! [`Picks`]（第三方镜像与自定义版本策略）。
//!
//! 只有服务端 JAR 下载与版本枚举，不涉及客户端 profile / 映射文件。
//!
//! # 内置实例与扩展点
//!
//! CLI 与库侧组合根都只用 [`FabricSource::new`]，即官方 Meta。要改镜像地址或
//! 固定 loader / installer，用 [`FabricSource::builder`] 自行构造后注入
//! `McsupBuilder::jar_sources`——宿主接管来源装配时才会走到这条路径。
//! 该构造器目前没有配置项驱动，属于宿主侧的
//! 扩展点而非 CLI 可达能力。

mod client;
pub(crate) mod manifest;

pub use client::{FabricSource, FabricSourceBuilder, Picks};
