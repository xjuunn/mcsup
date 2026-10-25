//! Quilt 官方 Meta 服务端来源
//!
//! 对外导出 [`QuiltSource`]（默认对接官方）与 [`QuiltSourceBuilder`] / [`Picks`]
//! （第三方镜像与自定义选版策略）。
//!
//! 只有服务端安装器的解析与下载地址，不涉及安装器执行（由 service 承担）与启动
//! 方式（由 runtime 承担）。
//!
//! # 内置实例与扩展点
//!
//! CLI 与库侧组合根都用 [`QuiltSource::new`]，即官方 Meta + 官方安装器。要改镜像
//! 地址或固定 loader，用 [`QuiltSource::builder`] 自行构造后注入
//! `McsupBuilder::jar_sources`。

mod manifest;
mod source;

pub use source::{Picks, QuiltSource, QuiltSourceBuilder};
