//! 内容资源目录：Mod / 资源包 / 数据包 / 光影 / 插件 / 整合包
//!
//! 回答「去哪里找、有哪些版本、下载哪个文件」。只返回本 crate 的契约类型
//! （[`domain::server::content`] 与 [`crate::SourceError`]），实现层可以是
//! Modrinth，也可以是后续接入的其他来源（CurseForge、镜像、本地索引）。
//!
//! 与 [`crate::ServerJarSource`] 的分工：那个trait 服务「安装服务端核心」，
//! 一个变体一个来源；本 trait 服务「给已有服务端装内容」，一个来源可覆盖
//! 多种内容类别，且同一类别可以有多个来源（换源 / 聚合）。
//!
//! # 下载职责
//!
//! 本 trait 只描述文件（地址、大小、哈希），不执行下载。真正的字节传输由
//! [`crate::ArtifactDownloader`] 按 [`ContentFile`] 执行，与核心下载共用同一
//! 套校验 / 断点续传设施。

use async_trait::async_trait;
use domain::server::artifact::Checksum;
use domain::server::content::{ContentCandidate, ContentKind};
use std::path::Path;
use url::Url;

use crate::SourceError;

/// 内容搜索条件
///
/// 与 [`domain::server::content::ContentRequest`] 的区别：那个是「给服务端挑
/// 版本」的适配条件，这个是「给人找项目」的检索条件。两者分开，避免把
/// 「我想要 fabric 1.21.1 的正式版」和「我想搜 sodium」混成一个结构。
#[derive(Debug, Clone, Default)]
pub struct ContentQuery {
    /// 关键词；空表示按条件浏览
    pub keyword: Option<String>,
    /// 只要这些类别（空表示不限）
    pub kinds: Vec<ContentKind>,
    /// 加载器过滤（空表示不限）
    pub loaders: Vec<String>,
    /// 游戏版本过滤（空表示不限）
    pub game_versions: Vec<String>,
    /// 跳过的结果数
    pub offset: u32,
    /// 返回条数
    pub limit: u32,
}

impl ContentQuery {
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

    /// 限定类别
    #[must_use]
    pub fn kinds(mut self, kinds: impl IntoIterator<Item = ContentKind>) -> Self {
        self.kinds = kinds.into_iter().collect();
        self
    }

    /// 限定加载器
    #[must_use]
    pub fn loaders(mut self, loaders: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.loaders = loaders.into_iter().map(Into::into).collect();
        self
    }

    /// 限定游戏版本
    #[must_use]
    pub fn game_versions(mut self, versions: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.game_versions = versions.into_iter().map(Into::into).collect();
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

/// 一个内容项目（搜索结果条目或详情）
///
/// 只用领域口径的 [`ContentKind`]，不暴露上游 `project_type` 字符串：上游
/// 的类别集合与安装落点不是一一对应（`datapack` / `plugin` 是 mod 的额外
/// 用途），差异化口径由实现层决定后在这里落地。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentProject {
    /// 来源内的项目 ID（稳定，可作为长期标识）
    pub id: String,
    /// 来源内的可读标识（slug / 项目名；可缺失）
    pub slug: Option<String>,
    /// 标题
    pub title: String,
    /// 一句话简介
    pub description: String,
    /// 面向安装的类别
    pub kind: ContentKind,
    /// 该项目同时可用的其它类别（如既可当 mod 又可当插件）
    pub also_kinds: Vec<ContentKind>,
    /// 声明支持的加载器
    pub loaders: Vec<String>,
    /// 声明支持的游戏版本
    pub game_versions: Vec<String>,
    /// 作者名
    pub author: Option<String>,
    /// 下载量
    pub downloads: u64,
    /// 图标地址
    pub icon_url: Option<String>,
}

impl ContentProject {
    /// 该项目的全部可用类别（主类别在前）
    #[must_use]
    pub fn kinds(&self) -> Vec<ContentKind> {
        let mut kinds = vec![self.kind];
        kinds.extend(self.also_kinds.iter().copied());
        kinds
    }
}

/// 搜索结果页
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentSearch {
    /// 命中项
    pub hits: Vec<ContentProject>,
    /// 跳过的结果数
    pub offset: u32,
    /// 匹配总数
    pub total: u64,
}

/// 版本文件描述（下载来源）
///
/// 下载执行方（Service）只依赖这个结构，不关心是 Modrinth 还是别的来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentFile {
    /// 下载地址
    pub location: Url,
    /// 保存用的文件名
    pub filename: String,
    /// 字节数
    pub size: u64,
    /// 可用校验和；来源未给可校验哈希时为 `None`
    pub checksum: Option<Checksum>,
}

/// 解析好的下载目标：某个项目的某个版本的某个文件
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedContent {
    /// 项目 ID
    pub project_id: String,
    /// 版本 ID
    pub version_id: String,
    /// 版本号（展示用）
    pub version_number: String,
    /// 要下载的文件
    pub file: ContentFile,
}

/// 内容资源的名称与出处
///
/// 每一个实现都要能自报家门：错误消息、JSON 输出里的 `source` 字段、多源
/// 聚合的去重都依赖它。
pub trait ContentSourceInfo {
    /// 来源标识（如 `modrinth`）
    fn name(&self) -> &'static str;
}

/// 内容资源来源
///
/// 实现要求：
/// - 只做查询与解析，不写本地文件、不执行下载
/// - 未知但可用的上游字段不得导致解析失败（沿用 serde 忽略未知字段的约定）
/// - 错误一律归 [`SourceError`]，让入口统一映射退出码 / HTTP 状态码
#[async_trait]
pub trait ContentSource: ContentSourceInfo + Send + Sync {
    /// 该来源支持的内容类别
    ///
    /// 入口据此判断「这个来源能不能装这类内容」，不写死来源清单。返回空集合
    /// 表示实现尚未声明，调用方按「全部支持」处理（`Kinds::all`）。
    fn supported_kinds(&self) -> Vec<ContentKind> {
        ContentKind::ALL.to_vec()
    }

    /// 搜索项目
    ///
    /// # Errors
    ///
    /// 上游不可达、状态码异常或响应无法解析时返回 [`SourceError`]
    async fn search(&self, query: &ContentQuery) -> Result<ContentSearch, SourceError>;

    /// 按 ID 或 slug 读取项目详情
    ///
    /// # Errors
    ///
    /// 项目不存在返回 [`SourceError::Status`]（404）或 [`SourceError::Upstream`]；
    /// 其余同上
    async fn project(&self, id_or_slug: &str) -> Result<ContentProject, SourceError>;

    /// 列出项目在给定条件下的候选版本（未排序，排序是领域规则）
    ///
    /// # Errors
    ///
    /// 项目不存在或上游异常时返回 [`SourceError`]
    async fn candidates(
        &self,
        id_or_slug: &str,
        request: &domain::server::content::ContentRequest,
    ) -> Result<Vec<ContentCandidate>, SourceError>;

    /// 解析某个候选版本的下载目标
    ///
    /// # Errors
    ///
    /// 版本不存在、版本没有任何文件或上游异常时返回 [`SourceError`]
    async fn resolve(
        &self,
        id_or_slug: &str,
        version_id: &str,
    ) -> Result<ResolvedContent, SourceError>;
}

/// 内容来源注册表：多来源按名字选取
///
/// 换源不修改业务代码：Service 只依赖 [`ContentSource`]，由装配方决定注入
/// 哪个实现。聚合（同一类别多来源优先级）留到需要时再引入，避免过早抽象。
pub struct ContentSourceRegistry {
    sources: Vec<std::sync::Arc<dyn ContentSource>>,
}

impl std::fmt::Debug for ContentSourceRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.sources.iter().map(|s| s.name()).collect();
        f.debug_struct("ContentSourceRegistry")
            .field("sources", &names)
            .finish()
    }
}

impl ContentSourceRegistry {
    /// 由来源列表构造
    #[must_use]
    pub fn new(sources: Vec<std::sync::Arc<dyn ContentSource>>) -> Self {
        Self { sources }
    }

    /// 按名字选取来源
    #[must_use]
    pub fn get(&self, name: &str) -> Option<std::sync::Arc<dyn ContentSource>> {
        self.sources
            .iter()
            .find(|source| source.name() == name)
            .cloned()
    }

    /// 默认来源（第一个）；无来源时为 `None`
    #[must_use]
    pub fn default_source(&self) -> Option<std::sync::Arc<dyn ContentSource>> {
        self.sources.first().cloned()
    }

    /// 全部来源名（注册顺序）
    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        self.sources.iter().map(|source| source.name()).collect()
    }

    /// 能装某类内容的来源
    #[must_use]
    pub fn supporting(&self, kind: ContentKind) -> Vec<std::sync::Arc<dyn ContentSource>> {
        self.sources
            .iter()
            .filter(|source| source.supported_kinds().contains(&kind))
            .cloned()
            .collect()
    }
}

/// 内容安装路径的解析辅助：来源给出的文件名不可信
///
/// 上游的 `filename` 直接拼进安装目录会被 `../` 穿越到目录外。这里只取最后
/// 一个路径段并拒绝空名与 `.` / `..`；结果仍由调用方与目标目录拼接。
///
/// # Errors
///
/// 文件名为空、只有 `/` 或最后一段是 `.` / `..` 时返回 [`SourceError`]
pub fn safe_filename(filename: &str) -> Result<String, SourceError> {
    let name = filename
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(filename)
        .trim();
    if name.is_empty() || name == "." || name == ".." {
        return Err(SourceError::Decode(Box::new(std::io::Error::other(
            format!("unsafe content filename: {filename:?}"),
        ))));
    }
    Ok(name.to_owned())
}

/// 安装目标：服务端目录 + 内容类别 → 具体落点
///
/// 只算路径，不建目录。
#[must_use]
pub fn install_path(server_dir: &Path, kind: ContentKind, filename: &str) -> std::path::PathBuf {
    server_dir.join(kind.install_dir()).join(filename)
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::server::artifact::ChecksumAlgorithm;

    #[test]
    fn safe_filename_strips_directories() {
        assert_eq!(safe_filename("mod.jar").expect("ok"), "mod.jar");
        assert_eq!(safe_filename("a/b/c.jar").expect("ok"), "c.jar");
        assert_eq!(safe_filename("a\\b\\c.jar").expect("ok"), "c.jar");
    }

    #[test]
    fn safe_filename_rejects_traversal() {
        for bad in ["", "  ", "..", ".", "a/..", "a/./", "dir/"] {
            assert!(safe_filename(bad).is_err(), "{bad:?} 应被拒绝");
        }
    }

    #[test]
    fn install_path_joins_kind_dir() {
        let dir = Path::new("/srv/mc");
        assert_eq!(
            install_path(dir, ContentKind::Mod, "sodium.jar"),
            Path::new("/srv/mc/mods/sodium.jar")
        );
        assert_eq!(
            install_path(dir, ContentKind::Plugin, "ess.jar"),
            Path::new("/srv/mc/plugins/ess.jar")
        );
    }

    #[test]
    fn content_project_kinds_puts_primary_first() {
        let project = ContentProject {
            id: "P".to_owned(),
            slug: None,
            title: "t".to_owned(),
            description: "d".to_owned(),
            kind: ContentKind::Mod,
            also_kinds: vec![ContentKind::Plugin, ContentKind::DataPack],
            loaders: Vec::new(),
            game_versions: Vec::new(),
            author: None,
            downloads: 0,
            icon_url: None,
        };
        assert_eq!(
            project.kinds(),
            vec![ContentKind::Mod, ContentKind::Plugin, ContentKind::DataPack]
        );
    }

    #[test]
    fn query_builder_accumulates() {
        let query = ContentQuery::new()
            .keyword("sodium")
            .kinds([ContentKind::Mod])
            .loaders(["fabric"])
            .game_versions(["1.21.1"])
            .page(20, 5);
        assert_eq!(query.keyword.as_deref(), Some("sodium"));
        assert_eq!(query.kinds, vec![ContentKind::Mod]);
        assert_eq!(query.loaders, vec!["fabric".to_owned()]);
        assert_eq!(query.game_versions, vec!["1.21.1".to_owned()]);
        assert_eq!(query.offset, 20);
        assert_eq!(query.limit, 5);
    }

    #[test]
    fn content_file_holds_checksum() {
        let file = ContentFile {
            location: "https://example.invalid/a.jar".parse().expect("url"),
            filename: "a.jar".to_owned(),
            size: 12,
            checksum: Some(Checksum::new(ChecksumAlgorithm::Sha1, "a".repeat(40))),
        };
        assert!(file.checksum.expect("checksum").is_verified());
    }

    struct StubSource {
        kinds: Vec<ContentKind>,
    }

    #[async_trait]
    impl ContentSourceInfo for StubSource {
        fn name(&self) -> &'static str {
            "stub"
        }
    }

    #[async_trait]
    impl ContentSource for StubSource {
        fn supported_kinds(&self) -> Vec<ContentKind> {
            self.kinds.clone()
        }

        async fn search(&self, _query: &ContentQuery) -> Result<ContentSearch, SourceError> {
            Ok(ContentSearch {
                hits: Vec::new(),
                offset: 0,
                total: 0,
            })
        }

        async fn project(&self, _id: &str) -> Result<ContentProject, SourceError> {
            Err(SourceError::Status { status: 404 })
        }

        async fn candidates(
            &self,
            _id: &str,
            _request: &domain::server::content::ContentRequest,
        ) -> Result<Vec<ContentCandidate>, SourceError> {
            Ok(Vec::new())
        }

        async fn resolve(&self, _id: &str, _version: &str) -> Result<ResolvedContent, SourceError> {
            Err(SourceError::Status { status: 404 })
        }
    }

    #[test]
    fn registry_selects_by_name_and_kind() {
        let mods = std::sync::Arc::new(StubSource {
            kinds: vec![ContentKind::Mod],
        });
        let packs = std::sync::Arc::new(StubSource {
            kinds: vec![ContentKind::Modpack],
        });
        let registry = ContentSourceRegistry::new(vec![
            mods as std::sync::Arc<dyn ContentSource>,
            packs as std::sync::Arc<dyn ContentSource>,
        ]);
        assert_eq!(registry.names(), vec!["stub", "stub"]);
        assert!(registry.get("absent").is_none());
        assert!(registry.default_source().is_some());
        assert_eq!(registry.supporting(ContentKind::Mod).len(), 1);
        assert_eq!(registry.supporting(ContentKind::Shader).len(), 0);
    }
}
