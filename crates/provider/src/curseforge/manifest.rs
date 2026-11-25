//! `CurseForge` v1 的响应模型
//!
//! 只声明本项目用到的字段；上游新增字段不影响解析（serde 忽略未知字段，与
//! `modrinth` / `mojang` 等来源一致）。
//!
//! # 字段口径
//!
//! `CurseForge` 的类别是 `classId` 数值、加载器是 `modLoaderType` 数值，且没有
//! 「客户端 / 服务端支持情况」字段。数值到领域口径的映射只在
//! `super::source` 里做一次，本模块保留上游原值。
//!
//! `gameVersions` 是一个混装列表：MC 版本（`1.20.1`）、两侧（`Client` /
//! `Server`）与加载器名（`Forge` / `NeoForge` / `Fabric` / `Quilt`）都在里面，
//! 因此解析时不能按位置取。

use serde::Deserialize;

/// 单对象响应（`{"data": {...}}`）
#[derive(Debug, Clone, Deserialize)]
pub struct Envelope<T> {
    /// 实际内容
    pub data: T,
}

/// 列表响应（`{"data": [...], "pagination": {...}}`）
#[derive(Debug, Clone, Deserialize)]
pub struct ListEnvelope<T> {
    /// 实际内容
    pub data: Vec<T>,
    /// 分页信息；部分端点不返回
    #[serde(default)]
    pub pagination: Option<Pagination>,
}

/// 分页信息
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pagination {
    /// 匹配总数
    #[serde(default)]
    pub total_count: u64,
    /// 本次返回条数
    #[serde(default)]
    pub result_count: u64,
}

/// 一个项目（搜索结果条目或详情）
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mod {
    /// 项目 ID（数值，可作为长期标识）
    pub id: u64,
    /// 标题
    pub name: String,
    /// 可读标识
    #[serde(default)]
    pub slug: Option<String>,
    /// 一句话简介
    #[serde(default)]
    pub summary: String,
    /// 上游类别 ID
    #[serde(default)]
    pub class_id: u32,
    /// 下载量
    #[serde(default)]
    pub download_count: u64,
    /// 图标
    #[serde(default)]
    pub logo: Option<ModAsset>,
    /// 作者列表
    #[serde(default)]
    pub authors: Vec<Author>,
    /// 最近文件按「游戏版本 × 加载器」的索引
    ///
    /// 搜索结果里带这份索引，可据此填 `ContentProject` 的加载器与游戏版本；
    /// 详情端点同样返回。
    #[serde(default)]
    pub latest_files_indexes: Vec<FileIndex>,
    /// 主页地址（`links.websiteUrl`）
    #[serde(default)]
    pub links: Option<ModLinks>,
}

/// 项目图标 / 截图
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModAsset {
    /// 缩略图地址（小的那张）
    #[serde(default)]
    pub thumbnail_url: Option<String>,
    /// 原图地址
    #[serde(default)]
    pub url: Option<String>,
}

/// 作者
#[derive(Debug, Clone, Deserialize)]
pub struct Author {
    /// 作者名
    pub name: String,
}

/// 项目的对外链接
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModLinks {
    /// 项目主页
    #[serde(default)]
    pub website_url: Option<String>,
}

/// 最近文件索引条目
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileIndex {
    /// 游戏版本
    #[serde(default)]
    pub game_version: String,
    /// 加载器 ID
    #[serde(default)]
    pub mod_loader: Option<u32>,
}

/// 一个文件（版本）
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct File {
    /// 文件 ID（本项目用作版本 ID）
    pub id: u64,
    /// 所属项目 ID
    #[serde(default)]
    pub mod_id: u64,
    /// 落盘文件名
    pub file_name: String,
    /// 展示名
    #[serde(default)]
    pub display_name: Option<String>,
    /// 发布渠道：1 = release，2 = beta，3 = alpha
    #[serde(default)]
    pub release_type: u8,
    /// 发布时间（ISO-8601）
    #[serde(default)]
    pub file_date: String,
    /// 字节数
    #[serde(default)]
    pub file_length: u64,
    /// 上游给出的下载地址（官方 API 在作者未授权分发时为 `null`）
    #[serde(default)]
    pub download_url: Option<String>,
    /// 哈希
    #[serde(default)]
    pub hashes: Vec<FileHash>,
    /// 混装的游戏版本 / 侧别 / 加载器列表
    #[serde(default)]
    pub game_versions: Vec<String>,
}

/// 文件哈希
#[derive(Debug, Clone, Deserialize)]
pub struct FileHash {
    /// 十六进制摘要
    pub value: String,
    /// 算法：1 = sha1，2 = md5
    pub algo: u8,
}

impl File {
    /// 取 sha1 摘要
    ///
    /// 下载器只认 sha1 / sha256，因此只映射 `algo = 1`；md5 不冒充。
    #[must_use]
    pub fn sha1(&self) -> Option<&str> {
        let value = self
            .hashes
            .iter()
            .find(|hash| hash.algo == 1)?
            .value
            .as_str();
        (value.len() == 40 && value.chars().all(|c| c.is_ascii_hexdigit())).then_some(value)
    }
}
