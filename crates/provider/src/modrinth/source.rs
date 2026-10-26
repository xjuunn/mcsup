//! [`ModrinthSource`] 对 [`port::ContentSource`] 的实现
//!
//! 把 Modrinth 的响应模型翻译成 port 的内容契约：类别映射、候选版本、下载
//! 文件。翻译规则（尤其是类别）只写在这里一处，Service 与入口都看不到
//! `project_type` 这类上游字符串。
//!
//! # 类别映射
//!
//! 上游的 `project_type` 只有 `mod` / `modpack` / `resourcepack` / `shader`
//! 四种；`datapack` 与 `plugin` 是**同一份 jar 的额外用途**，体现在
//! `all_project_types` 里。因此：
//!
//! - 主类别取 `project_type`
//! - `all_project_types` 里的 `datapack` / `plugin` 归入「也可用」集合
//! - 调用方明确要装成 datapack / plugin 时，按「也可用」判断能否安装
//!
//! 这样「搜 datapack」能搜到「可当数据包用的 mod」，装到 `datapacks/` 也不会
//! 因为主类别是 `mod` 而被拒绝。
//!
//! # 搜索语义
//!
//! `project_type` facet 对 datapack / plugin 不是精确过滤：上游会把「可当
//! 数据包 / 插件用的 mod」一并返回，命中项的 `all_project_types` 里带该用途。
//! 因此搜索不做二次筛选，`ContentProject::also_kinds` 如实带上这个信息。

use super::client::{ModrinthSource, SearchIndex, SearchQuery, VersionChannel, VersionFilter};
use super::facets::{Facet, FacetTree};
use super::manifest::{Project, SearchHit, Version, VersionFile};
use super::{file_checksum, primary_file};
use async_trait::async_trait;
use domain::server::content::{
    ContentCandidate, ContentKind, ContentRequest, ProviderHint, rank_candidates,
};
use port::SourceError;
use port::content::{
    ContentFile, ContentProject, ContentQuery, ContentSearch, ContentSource, ContentSourceInfo,
    ResolvedContent,
};

/// 搜索分页上限（上游约束）
const SEARCH_LIMIT_MAX: u32 = 100;

impl ContentSourceInfo for ModrinthSource {
    fn name(&self) -> &'static str {
        "modrinth"
    }
}

#[async_trait]
impl ContentSource for ModrinthSource {
    fn supported_kinds(&self) -> Vec<ContentKind> {
        ContentKind::ALL.to_vec()
    }

    async fn search(&self, query: &ContentQuery) -> Result<ContentSearch, SourceError> {
        let mut facets = FacetTree::new();
        if !query.kinds.is_empty() {
            // 多类别是 OR（同组内任一匹配）：搜 mod 与 modpack 都要
            facets = facets.and(
                query
                    .kinds
                    .iter()
                    .map(|kind| Facet::eq("project_type", kind.as_str())),
            );
        }
        if !query.loaders.is_empty() {
            facets = facets.and(
                query
                    .loaders
                    .iter()
                    .map(|loader| Facet::eq("categories", loader.as_str())),
            );
        }
        if !query.game_versions.is_empty() {
            facets = facets.and(
                query
                    .game_versions
                    .iter()
                    .map(|version| Facet::eq("versions", version.as_str())),
            );
        }

        let mut request = SearchQuery::new()
            .facets(facets)
            .index(SearchIndex::Relevance)
            .page(query.offset, query.limit.min(SEARCH_LIMIT_MAX));
        if let Some(keyword) = query.keyword.as_deref()
            && !keyword.trim().is_empty()
        {
            request = request.query(keyword);
        }

        let results = ModrinthSource::search(self, &request).await?;
        Ok(ContentSearch {
            hits: results.hits.iter().map(project_from_hit).collect(),
            offset: u32::try_from(results.offset).unwrap_or_default(),
            total: u64::try_from(results.total_hits).unwrap_or_default(),
        })
    }

    async fn project(&self, id_or_slug: &str) -> Result<ContentProject, SourceError> {
        let project = ModrinthSource::project(self, id_or_slug).await?;
        Ok(project_from_detail(&project))
    }

    async fn candidates(
        &self,
        id_or_slug: &str,
        request: &ContentRequest,
    ) -> Result<Vec<ContentCandidate>, SourceError> {
        let mut filter = VersionFilter::new().without_changelog();
        if let Some(loader) = request.loader_filter() {
            filter = filter.loaders([loader]);
        }
        if let Some(version) = request.game_version.as_deref() {
            filter = filter.game_versions([version]);
        }
        let versions = ModrinthSource::project_versions(self, id_or_slug, &filter).await?;
        let candidates: Vec<ContentCandidate> =
            versions.iter().map(candidate_from_version).collect();
        Ok(rank_candidates(candidates, request))
    }

    async fn resolve(
        &self,
        id_or_slug: &str,
        version_id: &str,
    ) -> Result<ResolvedContent, SourceError> {
        let version = ModrinthSource::version(self, version_id).await?;
        // 校验版本确实属于该项目：上游按版本 id 查询，不会替我们校验归属，
        // 拼错 / 跨项目的版本 id 会被悄悄接受
        if !belongs_to(&version, id_or_slug) {
            let project = ModrinthSource::project(self, id_or_slug).await?;
            if version.project_id != project.id {
                return Err(SourceError::Upstream {
                    status: 404,
                    message: format!(
                        "version {version_id} does not belong to project {id_or_slug}"
                    ),
                });
            }
        }
        let file = primary_file(&version).ok_or_else(|| SourceError::Upstream {
            status: 404,
            message: format!("version {version_id} has no files"),
        })?;
        Ok(ResolvedContent {
            project_id: version.project_id.clone(),
            version_id: version.id.clone(),
            version_number: version.version_number.clone(),
            file: content_file(file)?,
        })
    }
}

/// 版本是否直接归属给定的项目标识（id 或 slug 命中其一）
///
/// 只做「字符串相等」的快速判定：`id_or_slug` 既可能是项目 id 也可能是 slug，
/// 这里无法分辨，因此不命中时由调用方再查一次项目详情确认。
fn belongs_to(version: &Version, id_or_slug: &str) -> bool {
    version.project_id == id_or_slug
}

/// 上游搜索结果 → 内容项目
fn project_from_hit(hit: &SearchHit) -> ContentProject {
    let kind = kind_of(&hit.project_type, &hit.all_project_types);
    ContentProject {
        id: hit.project_id.clone(),
        slug: hit.slug.clone(),
        title: hit.title.clone(),
        description: hit.description.clone(),
        kind,
        also_kinds: other_kinds(kind, &hit.all_project_types),
        loaders: hit.categories.clone(),
        game_versions: hit.versions.clone(),
        author: Some(hit.author.clone()),
        downloads: u64::try_from(hit.downloads).unwrap_or_default(),
        icon_url: hit.icon_url.clone(),
    }
}

/// 上游项目详情 → 内容项目
///
/// 项目详情没有 `all_project_types`（只有搜索结果有），因此 `also_kinds` 为空：
/// 详情只用于展示，安装类别的最终判定在「按候选版本解析」时仍以请求为准。
fn project_from_detail(project: &Project) -> ContentProject {
    let kind = kind_of(&project.project_type, &[]);
    ContentProject {
        id: project.id.clone(),
        slug: project.slug.clone(),
        title: project.title.clone(),
        description: project.description.clone(),
        kind,
        also_kinds: other_kinds(kind, &[]),
        loaders: project.loaders.clone(),
        game_versions: project.game_versions.clone(),
        author: None,
        downloads: u64::try_from(project.downloads).unwrap_or_default(),
        icon_url: project.icon_url.clone(),
    }
}

/// 上游 `project_type` → 安装类别
///
/// 无法识别的上游类型按 `mod` 兜底是不对的：宁可判为 `mod` 也不该丢失项目，
/// 但 `datapack` / `plugin` 不是独立 `project_type`，因此它们只会出现在
/// 「也可用」集合里。
fn kind_of(project_type: &str, all: &[String]) -> ContentKind {
    if let Some(kind) = ProviderHint::guess_kind(project_type) {
        return kind;
    }
    // 上游可能新增 `minecraft_java_server` 之类；按第一个已知的额外用途归类，
    // 再不行按 mod 兜底，保证项目仍可展示
    all.iter()
        .find_map(|raw| ProviderHint::guess_kind(raw))
        .unwrap_or(ContentKind::Mod)
}

/// `all_project_types` 里与主类别不同的、可额外安装的类别
fn other_kinds(primary: ContentKind, all: &[String]) -> Vec<ContentKind> {
    let mut out: Vec<ContentKind> = Vec::new();
    for raw in all {
        let Some(kind) = ProviderHint::guess_kind(raw) else {
            continue;
        };
        if kind != primary && !out.contains(&kind) {
            out.push(kind);
        }
    }
    out
}

/// 上游版本 → 领域候选
fn candidate_from_version(version: &Version) -> ContentCandidate {
    ContentCandidate {
        version_id: version.id.clone(),
        version_number: version.version_number.clone(),
        channel: channel_name(&version.channel()),
        featured: version.featured,
        loaders: version.loaders.clone(),
        game_versions: version.game_versions.clone(),
        date_published: version.date_published.clone(),
        score: 0,
    }
}

/// 发布渠道的稳定名
fn channel_name(channel: &VersionChannel) -> String {
    channel.as_str().to_owned()
}

/// 上游文件 → 下载描述
///
/// 地址必须是 `https`：上游理论上都走 CDN 的 https，但把「非 https」当成
/// 可下载目标会让校验链出现明文降级，因此在这里拦下。
pub(crate) fn content_file(file: &VersionFile) -> Result<ContentFile, SourceError> {
    let location = file
        .url
        .parse::<url::Url>()
        .map_err(|e| SourceError::Decode(Box::new(e)))?;
    if location.scheme() != "https" {
        return Err(SourceError::Decode(Box::new(std::io::Error::other(
            format!("content file is not served over https: {}", file.url),
        ))));
    }
    Ok(ContentFile {
        location,
        filename: port::content::safe_filename(&file.filename)?,
        size: file.size,
        checksum: file_checksum(file),
    })
}

/// 测试辅助：内容文件的安全名
#[cfg(test)]
mod tests {
    use super::super::manifest::FileHashes;
    use super::*;

    fn version(id: &str, project: &str, files: Vec<VersionFile>) -> Version {
        Version {
            name: "v".to_owned(),
            version_number: "1.0.0".to_owned(),
            changelog: None,
            dependencies: Vec::new(),
            game_versions: vec!["1.21.1".to_owned()],
            version_type: "release".to_owned(),
            loaders: vec!["fabric".to_owned()],
            featured: false,
            status: "listed".to_owned(),
            id: id.to_owned(),
            project_id: project.to_owned(),
            author_id: "A".to_owned(),
            date_published: "2024-01-01T00:00:00Z".to_owned(),
            downloads: 0,
            environment: None,
            files,
        }
    }

    fn file(url: &str, name: &str) -> VersionFile {
        VersionFile {
            hashes: FileHashes {
                sha1: Some("a".repeat(40)),
                sha512: None,
            },
            url: url.to_owned(),
            filename: name.to_owned(),
            primary: true,
            size: 100,
            file_type: None,
        }
    }

    fn hit(project_type: &str, all: &[&str]) -> SearchHit {
        SearchHit {
            project_id: "P".to_owned(),
            project_type: project_type.to_owned(),
            all_project_types: all.iter().map(|s| (*s).to_owned()).collect(),
            title: "T".to_owned(),
            description: "D".to_owned(),
            author: "A".to_owned(),
            categories: vec!["fabric".to_owned()],
            display_categories: Vec::new(),
            versions: vec!["1.21.1".to_owned()],
            downloads: 10,
            follows: 1,
            icon_url: None,
            date_created: "2024-01-01T00:00:00Z".to_owned(),
            date_modified: "2024-01-01T00:00:00Z".to_owned(),
            latest_version: None,
            license: "MIT".to_owned(),
            environment: Vec::new(),
            client_side: None,
            server_side: None,
            disclosure_types: Vec::new(),
            gallery: Vec::new(),
            slug: Some("p".to_owned()),
            author_id: None,
            organization: None,
            organization_id: None,
            featured_gallery: None,
            color: None,
        }
    }

    #[test]
    fn kind_of_maps_primary_types() {
        assert_eq!(kind_of("mod", &[]), ContentKind::Mod);
        assert_eq!(kind_of("modpack", &[]), ContentKind::Modpack);
        assert_eq!(kind_of("resourcepack", &[]), ContentKind::ResourcePack);
        assert_eq!(kind_of("shader", &[]), ContentKind::Shader);
    }

    #[test]
    fn other_kinds_picks_datapack_and_plugin() {
        let all = vec!["mod".to_owned(), "datapack".to_owned(), "plugin".to_owned()];
        let kinds = other_kinds(ContentKind::Mod, &all);
        assert_eq!(kinds, vec![ContentKind::DataPack, ContentKind::Plugin]);
        // 主类别不会重复出现在额外集合里
        assert!(!kinds.contains(&ContentKind::Mod));
        // 未知值被忽略
        assert!(other_kinds(ContentKind::Mod, &["minecraft_java_server".to_owned()]).is_empty());
    }

    #[test]
    fn search_hit_maps_to_project() {
        let project = project_from_hit(&hit("mod", &["mod", "datapack"]));
        assert_eq!(project.kind, ContentKind::Mod);
        assert_eq!(project.also_kinds, vec![ContentKind::DataPack]);
        assert_eq!(
            project.kinds(),
            vec![ContentKind::Mod, ContentKind::DataPack]
        );
        assert_eq!(project.loaders, vec!["fabric".to_owned()]);
        assert_eq!(project.author.as_deref(), Some("A"));
    }

    #[test]
    fn unknown_project_type_falls_back_to_extra_kind_then_mod() {
        assert_eq!(
            kind_of("brand_new", &["datapack".to_owned()]),
            ContentKind::DataPack
        );
        assert_eq!(kind_of("brand_new", &[]), ContentKind::Mod);
    }

    #[test]
    fn candidate_maps_version_fields() {
        let candidate = candidate_from_version(&version("V", "P", Vec::new()));
        assert_eq!(candidate.version_id, "V");
        assert_eq!(candidate.channel, "release");
        assert_eq!(candidate.loaders, vec!["fabric".to_owned()]);
    }

    #[test]
    fn content_file_requires_https_and_safe_name() {
        let ok = content_file(&file("https://cdn.modrinth.com/a.jar", "a.jar")).expect("ok");
        assert_eq!(ok.filename, "a.jar");
        assert!(ok.checksum.is_some());

        let insecure = content_file(&file("http://cdn.modrinth.com/a.jar", "a.jar"));
        assert!(matches!(insecure, Err(SourceError::Decode(_))));

        let traversal = content_file(&file("https://cdn.modrinth.com/a.jar", "../a.jar"));
        assert_eq!(traversal.expect("应取最后一段").filename, "a.jar");
        assert!(content_file(&file("https://cdn.modrinth.com/a.jar", "..")).is_err());
    }

    #[test]
    fn belongs_to_matches_project_id() {
        let version = version("V", "P", Vec::new());
        assert!(belongs_to(&version, "P"));
        assert!(!belongs_to(&version, "other"));
    }
}
