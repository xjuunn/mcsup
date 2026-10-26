//! Modrinth（Labrinth v2）资源来源
//!
//! 封装搜索、项目详情、版本枚举与详情、按文件哈希反查、依赖图与标签。这是
//! 本项目后续实现 Mod / Modpack / `ResourcePack` / Datapack / Shader / Plugin
//! 下载功能的统一入口：上层只需面向 [`ModrinthSource`] 与 [`manifest`] 里的
//! 响应模型，不关心 HTTP 细节。
//!
//! # 对外导出
//!
//! - [`ModrinthSource`]：官方默认实例；[`ModrinthSourceBuilder`] 换源 / 镜像
//! - [`SearchQuery`] / [`SearchIndex`] / [`VersionFilter`]：查询条件
//! - [`FacetTree`] / [`Facet`] / [`FacetOp`]：搜索过滤条件构造
//! - [`ProjectType`] / [`VersionChannel`] / [`HashAlgorithm`]：受控枚举（带未知兜底）
//! - [`manifest`]：全部响应模型（[`Project`](manifest::Project)、[`Version`](manifest::Version) 等）
//! - [`primary_file`] / [`file_checksum`]：从版本取主文件 / 取可校验哈希
//!
//! # 内置实例与扩展点
//!
//! 默认 [`ModrinthSource::new`] 对接 `https://api.modrinth.com/v2`。自建
//! Labrinth 或镜像用 [`ModrinthSource::builder`] 改 `base_url` 后注入——与
//! `FabricSource` / `ForgeSource` 的扩展方式一致，不需要改上层。
//!
//! # 下载职责边界
//!
//! 本模块只回答「去哪里拿、拿的是什么」（[`VersionFile`](manifest::VersionFile)
//! 的地址、大小、哈希）。真正的字节下载由 provider 的下载器按该描述执行，与
//! 服务端核心下载共用同一套 `port::ArtifactDownloader` / 字节来源设施。

mod client;
pub mod facets;
pub mod manifest;
mod source;

pub use client::{
    HashAlgorithm, ModrinthSource, ModrinthSourceBuilder, ProjectType, SearchIndex, SearchQuery,
    VersionChannel, VersionFilter, file_checksum, primary_file,
};
pub use facets::{Facet, FacetOp, FacetTree};
