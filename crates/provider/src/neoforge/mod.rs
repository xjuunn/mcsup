//! `NeoForge` 模组加载器服务端来源
//!
//! 对外导出 [`NeoForgeSource`]（默认对接官方）与 [`NeoForgeSourceBuilder`] /
//! [`Picks`]（第三方镜像与自定义选版策略）。
//!
//! 只有服务端 installer 的解析与下载地址，不涉及安装器执行（由 service 承担）
//! 与启动方式（由 runtime 承担）。
//!
//! # 内置实例与扩展点
//!
//! CLI 与库侧组合根都用 [`NeoForgeSource::new`]，即官方 Maven。要改镜像地址或
//! 固定 build，用 [`NeoForgeSource::builder`] 自行构造后注入
//! `McsupBuilder::jar_sources`——宿主接管来源装配时才走这条路径。该构造器目前
//! 没有配置项驱动，属于宿主侧的扩展点而非 CLI 可达能力。

mod manifest;
mod source;

pub use source::{NeoForgeSource, NeoForgeSourceBuilder, Picks};
