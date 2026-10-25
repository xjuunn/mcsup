//! Fabric 官方 Meta 来源
//!
//! 对外导出 [`FabricSource`]（默认对接官方）与 [`FabricSourceBuilder`] /
//! [`Picks`]（第三方镜像与自定义版本策略）。
//!
//! 只有服务端 JAR 下载与版本枚举，不涉及客户端 profile / 映射文件。

mod client;
pub(crate) mod manifest;

pub use client::{FabricSource, FabricSourceBuilder, Picks};
