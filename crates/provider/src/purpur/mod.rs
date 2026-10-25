//! Purpur 官方 API 服务端来源
//!
//! 对外导出 [`PurpurSource`]（默认对接官方）与 [`PurpurSourceBuilder`] /
//! [`Picks`]（第三方镜像与自定义选版策略）。
//!
//! 只有服务端 JAR 的解析与下载地址，不涉及下载（由 file 承担）与启动（由
//! runtime 承担）。
//!
//! # 为什么不复用 `PaperMC` 来源
//!
//! Purpur 虽是 Paper 的下游分支，但发布渠道独立：Paper 系四变体共用 `PaperMC`
//! Fill v3，Purpur 走自有 `api.purpurmc.org/v2`。两者响应结构、版本顺序、哈希
//! 能力都不同，合并会让 Paper 来源里塞满分支。共用的是更底层的
//! `crate::shared` 脚手架。
//!
//! # 内置实例与扩展点
//!
//! CLI 与库侧组合根都用 [`PurpurSource::new`]，即官方 API。要改镜像地址或固定
//! build，用 [`PurpurSource::builder`] 自行构造后注入 `McsupBuilder::jar_sources`。

mod manifest;
mod source;

pub use source::{Picks, PurpurSource, PurpurSourceBuilder};
