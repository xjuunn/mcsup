//! Leaf 官方 API 服务端来源
//!
//! 对外导出 [`LeafSource`]（默认对接官方）与 [`LeafSourceBuilder`] / [`Picks`]
//! （第三方镜像与自定义选版策略）。
//!
//! 只有服务端 JAR 的解析与下载地址，不涉及下载（由 file 承担）与启动（由
//! runtime 承担）。
//!
//! # 内置实例与扩展点
//!
//! CLI 与库侧组合根都用 [`LeafSource::new`]，即官方 API。要改镜像地址或固定
//! build，用 [`LeafSource::builder`] 自行构造后注入 `McsupBuilder::jar_sources`。

mod manifest;
mod source;

pub use source::{LeafSource, LeafSourceBuilder, Picks};
