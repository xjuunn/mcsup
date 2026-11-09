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
//!
//! 注意上游的 facet 只是粗筛：Modrinth 带 `server_side:required|optional` 的
//! 查询实测仍会返回 `server_side=unsupported` 的整合包。因此来源不能只靠
//! facet，还要按响应体里的逐条字段再筛一次。

use async_trait::async_trait;
use domain::server::modpack::ModpackIndex;
use std::collections::HashSet;
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
    /// 该版本是否提供了服务端归档（带 `[server]` 标记的 `.mrpack`）
    ///
    /// 服务端安装默认优先选有该标记的版本；客户端包把仅客户端模组标成
    /// `server: required` 是常见情况，直接装会启动崩溃。
    pub has_server_file: bool,
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

/// 整合包内各成员的上游判定
///
/// 两个集合都以**文件 sha1** 为键，Service 据此决定某个清单文件要不要装到
/// 服务端。见 [`ModpackSource::pack_member_verdicts`]。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackMemberVerdicts {
    /// 上游标注仅客户端（`server_side: unsupported`）的文件
    pub client_only: HashSet<String>,
    /// 被整合包内其他成员的**必需依赖**指向的文件
    ///
    /// 这类文件即使自己被标成仅客户端也不能跳过：上游的项目级标注描述的是
    /// 「这个项目自己需不需要服务端」，而库类项目（`satin` / `melody` 一类
    /// 提供 API 的）常常标 `unsupported`，却是别的 mod 跑起来的硬前提。
    /// 服务端加载器不加载它们，但依赖声明要求它们**存在于 `mods/`**。
    ///
    /// 注意只有**会装**的成员的依赖才算数：被跳过的成员不该为别人提供保护。
    /// 这是不动点问题，不能一次统计——算法在
    /// [`domain::server::modpack::PackMemberGraph::depended_on`]，来源只需把上游
    /// 事实摆成图。
    pub depended_on: HashSet<String>,
}

impl PackMemberVerdicts {
    /// 某个文件是否应当跳过（不装到服务端）
    ///
    /// 仅客户端 **且** 无人依赖才跳过。判定收敛在这里，Service 与各入口
    /// 都调用它，不各写一份。
    #[must_use]
    pub fn skips(&self, sha1: &str) -> bool {
        self.client_only.contains(sha1) && !self.depended_on.contains(sha1)
    }
}

/// 整合包来源
///
/// 实现必须保证：**搜索与列表只返回能在服务端运行的整合包**。
///
/// 上游 facet 不可全信（见模块文档），实现要按响应体里的字段复核。
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

    /// 按文件哈希查上游的「仅客户端」标记
    ///
    /// 返回哈希集合：命中的文件在服务端不应安装。默认实现返回空集合，
    /// 让不提供该能力的来源保持「按清单 env 字段」的旧行为。
    ///
    /// 整合包作者常把仅客户端模组写成 `server: required`；只信清单会在
    /// 服务端启动时加载客户端类并崩溃。来源能查到权威元数据时由它覆盖。
    ///
    /// # Errors
    ///
    /// 上游不可达、状态码异常或响应无法解析时返回 [`SourceError`]
    async fn client_only_hashes(&self, _hashes: &[String]) -> Result<HashSet<String>, SourceError> {
        Ok(HashSet::new())
    }

    /// 按文件哈希查上游判定，并带上整合包内部的依赖关系
    ///
    /// 与 [`Self::client_only_hashes`] 的区别在于它不只回答「谁仅客户端」，
    /// 还回答「谁被别的成员依赖」。这两个信息缺一不可：
    ///
    /// - 只看「仅客户端」会把 `satin-api` 判成客户端专属而跳过，但它是
    ///   `environmentz` 的**硬依赖**，跳过之后服务端直接起不来
    ///   （`HARD_DEP_NO_CANDIDATE`）
    /// - 只看「被依赖」会把整合包里根本没装的普通客户端 mod 也留下
    ///
    /// 因此判定改为：**既是仅客户端、又没有会装的成员依赖它**才跳过。默认实现
    /// 退化为 [`Self::client_only_hashes`]，不认识依赖关系的来源行为不变。
    ///
    /// # Errors
    ///
    /// 上游不可达、状态码异常或响应无法解析时返回 [`SourceError`]
    async fn pack_member_verdicts(
        &self,
        hashes: &[String],
    ) -> Result<PackMemberVerdicts, SourceError> {
        let client_only = self.client_only_hashes(hashes).await?;
        Ok(PackMemberVerdicts {
            client_only,
            depended_on: HashSet::new(),
        })
    }

    /// 按清单里的下载地址重新取上游的**权威**文件哈希
    ///
    /// `.mrpack` 里的 sha1 是打包时写死的快照，上游文件被重新上传（作者修
    /// 版本、替换文件）后就会过期，使本应成功的下载卡在 `ChecksumMismatch`。
    /// 当某文件用清单哈希校验失败时，Service 用本方法按下载地址回到上游取
    /// **当前**哈希：若新哈希与下载到的字节一致，则接受该文件并告警；都不
    /// 一致才判失败。
    ///
    /// `hints` 是 `(下载地址, 文件名)` 列表；返回 `文件名 → 当前 sha1`（十六进
    /// 制小写）。上游不支持该能力时返回空 map，让调用方按原样报错。
    ///
    /// # Errors
    ///
    /// 上游不可达或响应异常时返回 [`SourceError`]（调用方据此退回原错误）
    async fn authoritative_hashes(
        &self,
        _hints: &[(String, String)],
    ) -> Result<std::collections::HashMap<String, String>, SourceError> {
        Ok(std::collections::HashMap::new())
    }
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

impl ModpackExtractError {
    /// 失败等级
    ///
    /// 清单非法是用法问题（用户选错了整合包文件），解压失败是后端问题——
    /// 但两者都是「这个归档用不了」，入口按后端失败呈现更贴近实际。
    #[must_use]
    pub fn category(&self) -> crate::ErrorCategory {
        match self {
            Self::Index(_) => crate::ErrorCategory::Usage,
            Self::Extract(_) | Self::MissingIndex(_) => crate::ErrorCategory::Backend,
        }
    }
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
            has_server_file: false,
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
