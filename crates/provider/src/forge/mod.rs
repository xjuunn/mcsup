//! Forge 官方 Maven 来源
//!
//! 对外导出 [`ForgeSource`]（默认对接官方）与 [`ForgeSourceBuilder`] /
//! [`Picks`]（第三方镜像与自定义版本策略）。
//!
//! 只有服务端 installer 的解析与下载地址，不涉及安装器执行（由 service 承担）
//! 与启动方式（由 runtime 承担）。
//!
//! # 内置实例与扩展点
//!
//! CLI 与库侧组合根都只用 [`ForgeSource::new`]，即官方 Maven + 文件站。要改
//! 镜像地址或固定 build，用 [`ForgeSource::builder`] 自行构造后注入
//! `McsupBuilder::jar_sources`——宿主接管来源装配时才会走到这条路径。
//! 该构造器目前没有配置项驱动，属于宿主侧的扩展点而非 CLI 可达能力。

mod client;
pub(crate) mod manifest;

pub use client::{ForgeSource, ForgeSourceBuilder, Picks};
