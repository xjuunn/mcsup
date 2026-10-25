//! `PaperMC` Fill v3 服务端来源（`Paper` / `Folia` / `Velocity` / `Waterfall`）
//!
//! 对外导出 [`PaperSource`]（默认对接官方）与 [`PaperProject`]<sup>†</sup> /
//! [`PaperSourceBuilder`] / [`Picks`]。
//!
//! † 一个 [`PaperSource`] 只服务一个 [`PaperProject`]，四个 Paper 系变体因此共用
//! 同一份实现，新增分支只需给枚举加项。
//!
//! 只有服务端 JAR 的解析与下载地址，不涉及下载（由 file 承担）与启动（由
//! runtime 承担）。
//!
//! # 内置实例与扩展点
//!
//! CLI 与库侧组合根对四个项目各用一次 [`PaperSource::new`]，即官方 Fill v3。要改
//! 镜像地址或固定 build，用 [`PaperSource::builder`] 自行构造后注入
//! `McsupBuilder::jar_sources`。

mod manifest;
mod source;

pub use source::{PaperProject, PaperSource, PaperSourceBuilder, Picks};
