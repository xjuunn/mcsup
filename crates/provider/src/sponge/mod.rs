//! `SpongeVanilla` 官方 Maven 服务端来源
//!
//! 对外导出 [`SpongeSource`]（默认对接官方）与 [`SpongeSourceBuilder`]（第三方
//! 镜像）。
//!
//! 只有服务端 JAR 的解析与下载地址，不涉及下载（由 file 承担）与启动（由
//! runtime 承担）。
//!
//! `SpongeForge` / `SpongeNeo` 是模组形态（需配合 `Forge` / `NeoForge` 启动），
//! 不在本模块范围内。
//!
//! # 内置实例与扩展点
//!
//! CLI 与库侧组合根都用 [`SpongeSource::new`]，即官方 Maven 仓库。要改镜像地址，
//! 用 [`SpongeSource::builder`] 自行构造后注入 `McsupBuilder::jar_sources`。

mod manifest;
mod source;

pub use source::{SpongeSource, SpongeSourceBuilder, mc_version_of};
