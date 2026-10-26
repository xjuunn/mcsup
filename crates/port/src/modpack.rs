//! 整合包来源：搜索「能开服」的整合包、取详情、列版本、下载清单
//!
//! 与 [`crate::content::ContentSource`] 的分工：
//!
//! - `ContentSource` 回答「内容文件在哪」，用于给**已有**服务端装 mod / 资源包
//! - `ModpackSource` 回答「怎么按整合包**建成一台服务端**」，因此它必须能描述
//!   「这份整合包需要哪个 Minecraft 版本、哪个加载器、服务端要装哪些文件」
//!
//! 本 trait 仍只做查询与解析，不写本地文件、不装加载器。展开整合包（解压
//! overrides、按清单下载、装 loader）属于 service 的编排。
//!
//! # 「支持服务端」的判定
//!
//! 整合包不一定能在服务端跑（许多是纯客户端整合）。来源实现必须按自身能力
//! 只返回可开服的整合包，调用方不再二次过滤——判定口径只在一处。

use async_trait::async_trait;
use domain::server::modpack::ModpackIndex;
use std::sync::Arc;

use crate::SourceError;
use crate::content::{ContentFile, ContentSourceInfo};

/// 整合包搜索条件
#[derive(Debug, Clone, Default)]
pub struct ModpackQuery {
    /// 关键词；空表示按条件浏览
    pub keyword: Option<String>,
    /// 只要支持这些 Minecraft 版本（空表示不限）
    pub game_versions: Vec<String>,
    /// 只要支持这些加载器（空表示不限）
    pub loaders: Vec<String>,
    /// 跳过的结果数
    pub offset: u32,
    /// 返回条数
    pub limit: u32,
}

impl ModpackQuery {
    /// 默认分页：前 20 条
    #[must_use]
    pub fn new() -> Self {
        Self {
            limit: 20,
            ..Self::default()
        }
    }

    /// 设置关键词
    #[must_use]
    pub fn keyword(mut self, keyword: impl Into<String>) -> Self {
        self.keyword = Some(keyword.into());
        self
    }

    /// 限定 Minecraft 版本
    #[must_use]
    pub fn game_versions(mut self, versions: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.game_versions = versions.into_iter().map(Into::into).collect();
        self
    }

    /// 限定加载器
    #[must_use]
    pub fn loaders(mut self, loaders: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.loaders = loaders.into_iter().map(Into::into).collect();
        self
    }

    /// 设置分页
    #[must_use]
    pub fn page(mut self, offset: u32, limit: u32) -> Self {
        self.offset = offset;
        self.limit = limit;
        self
    }
}

/// 搜索结果里的整合包摘要
///
/// 比 [`crate::content::ContentProject`] 多了「服务端可跑性」：客户端专属的
/// 整合包不会出现在这里，因此字段只描述能开服的那些。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModpackSummary {
    /// 项目 ID（稳定标识）
    pub id: String,
    /// 可读标识（slug）
    pub slug: Option<String>,
    /// 标题
    pub title: String,
    /// 一句话简介
    pub description: String,
    /// 作者
    pub author: Option<String>,
    /// 声明支持的加载器
    pub loaders: Vec<String>,
    /// 声明支持的 Minecraft 版本
    pub game_versions: Vec<String>,
    /// 下载量
    pub downloads: u64,
    /// 关注数
    pub follows: u64,
    /// 图标地址
    pub icon_url: Option<String>,
    /// 许可证（SPDX / `LicenseRef-*`）
    pub license: Option<String>,
    /// 最近更新时间（ISO-8601）
    pub updated: Option<String>,
}

/// 整合包详情（在摘要之上补充长描述与统计）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModpackDetail {
    /// 摘要字段
    pub summary: ModpackSummary,
    /// 长描述（Markdown）
    pub body: String,
    /// 画廊图片地址
    pub gallery: Vec<String>,
    /// 项目来源页（可展示给用户）
    pub page_url: String,
    /// 出处说明（如 `Modrinth`），用于展示「内容来自哪里」
    pub source_name: &'static str,
}

/// 整合包的一个版本
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModpackVersion {
    /// 版本 ID
    pub id: String,
    /// 版本号（如 `14.1.1`）
    pub version_number: String,
    /// 版本名（可读）
    pub name: String,
    /// 发布渠道（`release` / `beta` / `alpha`）
    pub channel: String,
    /// 该版本支持的加载器
    pub loaders: Vec<String>,
    /// 该版本支持的 Minecraft 版本
    pub game_versions: Vec<String>,
    /// 发布时间（ISO-8601）
    pub published: String,
    /// 下载量
    pub downloads: u64,
    /// 更新日志（可能为空）
    pub changelog: Option<String>,
}

impl ModpackVersion {
    /// 是否为正式版
    #[must_use]
    pub fn is_release(&self) -> bool {
        self.channel == "release"
    }
}

/// 解析好的一份整合包下载目标：清单文件与它声明的索引
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModpack {
    /// 项目 ID
    pub project_id: String,
    /// 版本 ID
    pub version_id: String,
    /// 版本号
    pub version_number: String,
    /// 要下载的 `.mrpack` 文件
    pub file: ContentFile,
}

/// 解压后的整合包内容索引
///
/// 由 Provider 在本地解压 `.mrpack` 后产出：清单 + overrides 目录。Service 据此
/// 下载清单文件、铺 overrides、装加载器。
#[derive(Debug, Clone)]
pub struct ExtractedModpack {
    /// 解析并校验过的清单
    pub index: ModpackIndex,
    /// overrides 目录（已解压到本地；`None` 表示整合包没有 overrides）
    pub overrides_dir: Option<std::path::PathBuf>,
}

/// 整合包来源
///
/// 实现必须保证：**搜索与列表只返回能在服务端运行的整合包**。
#[async_trait]
pub trait ModpackSource: ContentSourceInfo + Send + Sync {
    /// 搜索可开服的整合包
    ///
    /// # Errors
    ///
    /// 上游不可达、状态码异常或响应无法解析时返回 [`SourceError`]
    async fn search(&self, query: &ModpackQuery) -> Result<ModpackSearch, SourceError>;

    /// 读取整合包详情
    ///
    /// # Errors
    ///
    /// 项目不存在或上游异常时返回 [`SourceError`]
    async fn detail(&self, id_or_slug: &str) -> Result<ModpackDetail, SourceError>;

    /// 列出某个整合包的版本（新在前由实现保证，Service 不再排序）
    ///
    /// # Errors
    ///
    /// 项目不存在或上游异常时返回 [`SourceError`]
    async fn versions(&self, id_or_slug: &str) -> Result<Vec<ModpackVersion>, SourceError>;

    /// 解析某版本的 `.mrpack` 下载目标
    ///
    /// `version_id` 为 `None` 时由来源挑选最新正式版。
    ///
    /// # Errors
    ///
    /// 项目 / 版本不存在、版本没有可下载文件或上游异常时返回 [`SourceError`]
    async fn resolve(
        &self,
        id_or_slug: &str,
        version_id: Option<&str>,
    ) -> Result<ResolvedModpack, SourceError>;
}

/// 整合包搜索页
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModpackSearch {
    /// 命中项
    pub hits: Vec<ModpackSummary>,
    /// 跳过的结果数
    pub offset: u32,
    /// 匹配总数
    pub total: u64,
}

/// 归档解压失败（整合包 `.mrpack` 解析用）
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ModpackExtractError {
    /// 归档读取 / 解压失败
    #[error("extract modpack: {0}")]
    Extract(String),
    /// 归档里没有清单
    #[error("modpack archive has no {0}")]
    MissingIndex(&'static str),
    /// 清单非法
    #[error(transparent)]
    Index(#[from] domain::server::modpack::ModpackError),
}

/// 整合包归档读取能力
///
/// 与 `port::ArchiveExtractor` 分开：那个只负责「解到目录」，整合包还要在解压
/// 后从归档里取清单并校验。实现可以复用同一套解压工具。
pub trait ModpackExtractor: Send + Sync {
    /// 读取归档里的清单文本并解析
    ///
    /// # Errors
    ///
    /// 归档不可读、没有清单或清单非法时返回 [`ModpackExtractError`]
    fn read_index(&self, archive: &std::path::Path) -> Result<ModpackIndex, ModpackExtractError>;

    /// 把 `overrides/` 与 `client-overrides/` 之外的内容解到 `dest`
    ///
    /// 只铺 overrides：清单文件与加载器由 Service 另行处理。`server-overrides/`
    /// 与服务端同构，优先铺它；没有时退回 `overrides/`。
    ///
    /// # Errors
    ///
    /// 解压失败时返回 [`ModpackExtractError`]
    fn extract_overrides(
        &self,
        archive: &std::path::Path,
        dest: &std::path::Path,
    ) -> Result<(), ModpackExtractError>;
}

/// 整合包来源注册表
///
/// 目前只有一个默认来源；保留注册表形态让「换源 / 多源」不需要改 Service。
pub struct ModpackSourceRegistry {
    sources: Vec<Arc<dyn ModpackSource>>,
}

impl std::fmt::Debug for ModpackSourceRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.sources.iter().map(|source| source.name()).collect();
        f.debug_struct("ModpackSourceRegistry")
            .field("sources", &names)
            .finish()
    }
}

impl ModpackSourceRegistry {
    /// 由来源列表构造
    #[must_use]
    pub fn new(sources: Vec<Arc<dyn ModpackSource>>) -> Self {
        Self { sources }
    }

    /// 按名选取来源
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn ModpackSource>> {
        self.sources
            .iter()
            .find(|source| source.name() == name)
            .cloned()
    }

    /// 默认来源（第一个）
    #[must_use]
    pub fn default_source(&self) -> Option<Arc<dyn ModpackSource>> {
        self.sources.first().cloned()
    }

    /// 全部来源名
    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        self.sources.iter().map(|source| source.name()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_builder_accumulates() {
        let query = ModpackQuery::new()
            .keyword("optimized")
            .game_versions(["1.21.1"])
            .loaders(["fabric"])
            .page(20, 5);
        assert_eq!(query.keyword.as_deref(), Some("optimized"));
        assert_eq!(query.game_versions, vec!["1.21.1".to_owned()]);
        assert_eq!(query.loaders, vec!["fabric".to_owned()]);
        assert_eq!(query.offset, 20);
        assert_eq!(query.limit, 5);
    }

    #[test]
    fn default_query_pages_twenty() {
        assert_eq!(ModpackQuery::new().limit, 20);
    }

    fn version(channel: &str) -> ModpackVersion {
        ModpackVersion {
            id: "V".to_owned(),
            version_number: "1.0.0".to_owned(),
            name: "v1".to_owned(),
            channel: channel.to_owned(),
            loaders: vec!["fabric".to_owned()],
            game_versions: vec!["1.21.1".to_owned()],
            published: "2024-01-01T00:00:00Z".to_owned(),
            downloads: 0,
            changelog: None,
        }
    }

    #[test]
    fn release_flag() {
        assert!(version("release").is_release());
        assert!(!version("beta").is_release());
    }

    struct Stub;

    #[async_trait]
    impl ContentSourceInfo for Stub {
        fn name(&self) -> &'static str {
            "stub"
        }
    }

    #[async_trait]
    impl ModpackSource for Stub {
        async fn search(&self, _query: &ModpackQuery) -> Result<ModpackSearch, SourceError> {
            Ok(ModpackSearch {
                hits: Vec::new(),
                offset: 0,
                total: 0,
            })
        }
        async fn detail(&self, _id: &str) -> Result<ModpackDetail, SourceError> {
            Err(SourceError::Status { status: 404 })
        }
        async fn versions(&self, _id: &str) -> Result<Vec<ModpackVersion>, SourceError> {
            Ok(Vec::new())
        }
        async fn resolve(
            &self,
            _id: &str,
            _version: Option<&str>,
        ) -> Result<ResolvedModpack, SourceError> {
            Err(SourceError::Status { status: 404 })
        }
    }

    #[test]
    fn registry_selects_by_name() {
        let registry = ModpackSourceRegistry::new(vec![Arc::new(Stub)]);
        assert_eq!(registry.names(), vec!["stub"]);
        assert!(registry.get("stub").is_some());
        assert!(registry.get("absent").is_none());
        assert!(registry.default_source().is_some());
    }
}
