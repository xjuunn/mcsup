//! `CurseForge` 内容来源
//!
//! 覆盖 `Mod` / `ResourcePack` / `DataPack` / `Shader` / `Modpack` / `Plugin`
//! 六类内容的搜索、项目详情、候选版本与下载解析。上层只面向
//! [`port::content::ContentSource`]，看不到 `classId` / `modLoaderType` 等上游字段。
//!
//! # 为什么默认走社区镜像
//!
//! 官方 `api.curseforge.com` 不提供匿名访问（无 key 一律 `403`），而多数用户
//! 不会先去申请一个 key。镜像 `https://mod.mcimirror.top/curseforge/v1` 与官方
//! v1 同构、匿名可用，因此默认指向它；设置 `CURSEFORGE_API_KEY` 后装配方会切
//! 到官方地址（见 [`crate::curseforge::client::CurseForgeSource::builder`]）。
//!
//! # 分发边界
//!
//! 本模块只查询与解析，不写盘、不下载。文件由 `port::ArtifactDownloader` 按
//! [`port::content::ContentFile`] 的描述传输；下载地址缺失时按文件 ID 换算
//! 官方 CDN 路径，不把「作者未授权 API 分发」误报成「无法安装」。

mod client;
pub mod manifest;
mod source;

pub use client::{
    CurseForgeSource, CurseForgeSourceBuilder, GAME_ID_MINECRAFT, MCIM_API, OFFICIAL_API, cdn_url,
    class_id, kind_of_class, loader_id, loader_name, split_terms,
};
