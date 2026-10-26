//! Modrinth 整合包来源与 `.mrpack` 清单解析
//!
//! 两部分：
//!
//! - [`ModrinthModpackSource`]：`port::ModpackSource` 的实现，搜索「能在服务端
//!   跑」的整合包、取详情、列版本、解析 `.mrpack` 下载目标
//! - [`MrpackExtractor`]：`port::ModpackExtractor` 的实现，读归档里的
//!   `modrinth.index.json` 并铺 `overrides/`
//!
//! # 「支持服务端」的判定
//!
//! Modrinth 的搜索支持 `server_side` facet。客户端专属的整合包会把
//! `server_side` 标成 `unsupported`，因此搜索时加
//! `["server_side:required","server_side:optional"]` 这个 OR 组，只留下能在
//! 服务端运行的整合包。判定只在这里做一次，调用方不再过滤。
//!
//! # 为什么清单解析放在 Provider
//!
//! `.mrpack` 是外部文件格式，解析属于「适配上游格式」的职责；领域层只持有
//! 解析后的 [`ModpackIndex`] 并做路径与格式判定（见 `domain::server::modpack`）。
//! 这样领域层保持零 serde 依赖。

use super::client::{ModrinthSource, SearchIndex, SearchQuery, VersionFilter};
use super::facets::{Facet, FacetTree};
use super::manifest::{Project, SearchHit, Version};
use super::primary_file;
use async_trait::async_trait;
use domain::server::modpack::{
    FileEnv, ModpackDependencies, ModpackFile, ModpackHashes, ModpackIndex,
};
use port::SourceError;
use port::content::ContentSourceInfo;
use port::modpack::{
    ModpackDetail, ModpackExtractError, ModpackExtractor, ModpackQuery, ModpackSearch,
    ModpackSource, ModpackSummary, ModpackVersion, ResolvedModpack,
};
use serde::Deserialize;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use zip::ZipArchive;
use zip::result::ZipError;

/// 搜索分页上限（上游约束）
const SEARCH_LIMIT_MAX: u32 = 100;

/// 项目页根地址（展示用；API 根地址是 `api.modrinth.com/v2`）
const WEB_ROOT: &str = "https://modrinth.com";

impl ContentSourceInfo for ModrinthModpackSource {
    fn name(&self) -> &'static str {
        "modrinth"
    }
}

/// Modrinth 整合包来源
///
/// 内部复用 [`ModrinthSource`] 的 HTTP 客户端与端点封装：两者是同一套 API，
/// 只是翻译成不同的契约。
pub struct ModrinthModpackSource {
    inner: ModrinthSource,
}

impl std::fmt::Debug for ModrinthModpackSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModrinthModpackSource")
            .field("base_url", &self.inner.base_url())
            .finish_non_exhaustive()
    }
}

impl ModrinthModpackSource {
    /// 对接官方
    ///
    /// # Errors
    ///
    /// HTTP 客户端构建失败时返回 [`SourceError::Init`]
    pub fn new() -> Result<Self, SourceError> {
        Ok(Self {
            inner: ModrinthSource::new()?,
        })
    }

    /// 由已有客户端构造（自建 / 镜像时复用同一份 HTTP 配置）
    #[must_use]
    pub fn from_source(inner: ModrinthSource) -> Self {
        Self { inner }
    }

    /// API 根地址
    #[must_use]
    pub fn base_url(&self) -> &str {
        self.inner.base_url()
    }
}

#[async_trait]
impl ModpackSource for ModrinthModpackSource {
    async fn search(&self, query: &ModpackQuery) -> Result<ModpackSearch, SourceError> {
        let mut facets = FacetTree::new().and([Facet::eq("project_type", "modpack")]);
        // 服务端可跑：required 或 optional 都算（optional 表示服务端可用但非必需）
        facets = facets.and([
            Facet::eq("server_side", "required"),
            Facet::eq("server_side", "optional"),
        ]);
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
            .index(SearchIndex::Downloads)
            .page(query.offset, query.limit.min(SEARCH_LIMIT_MAX));
        if let Some(keyword) = query.keyword.as_deref()
            && !keyword.trim().is_empty()
        {
            request = request.query(keyword);
        }
        let results = self.inner.search(&request).await?;
        Ok(ModpackSearch {
            hits: results.hits.iter().map(summary_from_hit).collect(),
            offset: u32::try_from(results.offset).unwrap_or_default(),
            total: u64::try_from(results.total_hits).unwrap_or_default(),
        })
    }

    async fn detail(&self, id_or_slug: &str) -> Result<ModpackDetail, SourceError> {
        let project = self.inner.project(id_or_slug).await?;
        let summary = summary_from_project(&project);
        let slug = project.slug.as_deref().unwrap_or(&project.id);
        Ok(ModpackDetail {
            summary,
            body: project.body.clone(),
            gallery: project
                .gallery
                .iter()
                .map(|image| image.url.clone())
                .collect(),
            page_url: format!("{WEB_ROOT}/modpack/{slug}"),
            source_name: "Modrinth",
        })
    }

    async fn versions(&self, id_or_slug: &str) -> Result<Vec<ModpackVersion>, SourceError> {
        // 只取正式版与测试版；changelog 关掉，详情页按需再查
        let filter = VersionFilter::new().without_changelog();
        let versions = self.inner.project_versions(id_or_slug, &filter).await?;
        Ok(versions.iter().map(version_from_upstream).collect())
    }

    async fn resolve(
        &self,
        id_or_slug: &str,
        version_id: Option<&str>,
    ) -> Result<ResolvedModpack, SourceError> {
        let version = if let Some(id) = version_id {
            self.inner.version(id).await?
        } else {
            let all = self.versions(id_or_slug).await?;
            let picked = all
                .iter()
                .find(|version| version.is_release())
                .or_else(|| all.first())
                .ok_or_else(|| SourceError::Upstream {
                    status: 404,
                    message: format!("modpack {id_or_slug} has no versions"),
                })?;
            self.inner.version(&picked.id).await?
        };
        let file = primary_file(&version).ok_or_else(|| SourceError::Upstream {
            status: 404,
            message: format!("modpack version {} has no files", version.id),
        })?;
        crate::modrinth::source::content_file(file).map(|content_file| ResolvedModpack {
            project_id: version.project_id.clone(),
            version_id: version.id.clone(),
            version_number: version.version_number.clone(),
            file: content_file,
        })
    }
}

/// 搜索结果 → 摘要
fn summary_from_hit(hit: &SearchHit) -> ModpackSummary {
    ModpackSummary {
        id: hit.project_id.clone(),
        slug: hit.slug.clone(),
        title: hit.title.clone(),
        description: hit.description.clone(),
        author: Some(hit.author.clone()),
        loaders: hit.categories.clone(),
        game_versions: hit.versions.clone(),
        downloads: u64::try_from(hit.downloads).unwrap_or_default(),
        follows: u64::try_from(hit.follows).unwrap_or_default(),
        icon_url: hit.icon_url.clone(),
        license: Some(hit.license.clone()),
        updated: Some(hit.date_modified.clone()),
    }
}

/// 项目详情 → 摘要
fn summary_from_project(project: &Project) -> ModpackSummary {
    ModpackSummary {
        id: project.id.clone(),
        slug: project.slug.clone(),
        title: project.title.clone(),
        description: project.description.clone(),
        author: None,
        loaders: project.loaders.clone(),
        game_versions: project.game_versions.clone(),
        downloads: u64::try_from(project.downloads).unwrap_or_default(),
        follows: u64::try_from(project.followers).unwrap_or_default(),
        icon_url: project.icon_url.clone(),
        license: Some(project.license.id.clone()),
        updated: Some(project.updated.clone()),
    }
}

/// 上游版本 → 整合包版本
fn version_from_upstream(version: &Version) -> ModpackVersion {
    ModpackVersion {
        id: version.id.clone(),
        version_number: version.version_number.clone(),
        name: version.name.clone(),
        channel: version.channel().as_str().to_owned(),
        loaders: version.loaders.clone(),
        game_versions: version.game_versions.clone(),
        published: version.date_published.clone(),
        downloads: u64::try_from(version.downloads).unwrap_or_default(),
        changelog: version.changelog.clone(),
    }
}

// ---------------------------------------------------------------------------
// mrpack 解压
// ---------------------------------------------------------------------------

/// `.mrpack` 读取实现
///
/// 直接解析 zip（不调用系统 `unzip`）：归档里的非 ASCII 文件名不会被系统
/// locale 改写，解压失败也不会停在 `unzip` 的交互式提问上。
#[derive(Debug, Default)]
pub struct MrpackExtractor;

impl MrpackExtractor {
    /// 新建
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl ModpackExtractor for MrpackExtractor {
    fn read_index(&self, archive: &Path) -> Result<ModpackIndex, ModpackExtractError> {
        let mut zip = open_archive(archive)?;
        let mut text = String::new();
        match zip.by_name(domain::server::modpack::INDEX_FILE) {
            Ok(mut entry) => {
                entry.read_to_string(&mut text).map_err(|err| {
                    ModpackExtractError::Extract(format!(
                        "read {}: {err}",
                        domain::server::modpack::INDEX_FILE
                    ))
                })?;
            }
            Err(ZipError::FileNotFound) => {
                return Err(ModpackExtractError::MissingIndex(
                    domain::server::modpack::INDEX_FILE,
                ));
            }
            Err(err) => {
                return Err(ModpackExtractError::Extract(format!(
                    "read {}: {err}",
                    domain::server::modpack::INDEX_FILE
                )));
            }
        }
        let raw: RawIndex = serde_json::from_str(&text)
            .map_err(|err| ModpackExtractError::Index(serde_err(&err)))?;
        let mut files = Vec::with_capacity(raw.files.len());
        for file in raw.files {
            files.push(ModpackFile {
                path: file.path,
                downloads: file.downloads,
                size: file.file_size,
                hashes: ModpackHashes {
                    sha1: file.hashes.sha1,
                    sha512: file.hashes.sha512,
                },
                env: file.env.map(|env| FileEnv {
                    client: env.client.into(),
                    server: env.server.into(),
                }),
            });
        }
        let mut dependencies = std::collections::BTreeMap::new();
        for (key, value) in raw.dependencies {
            dependencies.insert(key, value);
        }
        ModpackIndex::new(
            raw.format_version,
            raw.game,
            raw.version_id,
            raw.name,
            ModpackDependencies { raw: dependencies },
            files,
        )
        .map_err(ModpackExtractError::Index)
    }

    fn extract_overrides(&self, archive: &Path, dest: &Path) -> Result<(), ModpackExtractError> {
        // `server-overrides/` 优先：它专门描述服务端侧文件。没有时退回
        // `overrides/`（格式规范里两者都可能有；`client-overrides/` 只给
        // 客户端，从不铺到服务端）。
        std::fs::create_dir_all(dest)
            .map_err(|err| ModpackExtractError::Extract(err.to_string()))?;
        let mut zip = open_archive(archive)?;
        let (server, plain) = override_entries(&mut zip)?;
        let has_server = !server.is_empty();
        for index in server {
            extract_entry(&mut zip, index, dest, "server-overrides/", false)?;
        }
        for index in plain {
            // server-overrides 已经写过的文件不覆盖
            extract_entry(&mut zip, index, dest, "overrides/", has_server)?;
        }
        Ok(())
    }
}

/// 打开归档并给出统一的错误文本
fn open_archive(archive: &Path) -> Result<ZipArchive<File>, ModpackExtractError> {
    let file = File::open(archive)
        .map_err(|err| ModpackExtractError::Extract(format!("open archive: {err}")))?;
    ZipArchive::new(file)
        .map_err(|err| ModpackExtractError::Extract(format!("read archive: {err}")))
}

/// 归档里 overrides 两类前缀的成员下标
fn override_entries(
    zip: &mut ZipArchive<File>,
) -> Result<(Vec<usize>, Vec<usize>), ModpackExtractError> {
    let mut server = Vec::new();
    let mut plain = Vec::new();
    for index in 0..zip.len() {
        let name = zip
            .by_index_raw(index)
            .map_err(|err| ModpackExtractError::Extract(format!("read archive entry: {err}")))?
            .name()
            .map_err(|err| ModpackExtractError::Extract(format!("read archive entry name: {err}")))?
            .into_owned();
        if name.starts_with("server-overrides/") {
            server.push(index);
        } else if name.starts_with("overrides/") {
            plain.push(index);
        }
    }
    Ok((server, plain))
}

/// 解开一个成员到 `dest`，去掉 `prefix` 这一层
///
/// `skip_existing` 用于「server-overrides 优先」的第二遍：同名文件已由
/// 服务端专属覆盖写好后不再改写。
fn extract_entry(
    zip: &mut ZipArchive<File>,
    index: usize,
    dest: &Path,
    prefix: &str,
    skip_existing: bool,
) -> Result<(), ModpackExtractError> {
    let mut entry = zip
        .by_index(index)
        .map_err(|err| ModpackExtractError::Extract(format!("read archive entry: {err}")))?;
    let name = entry
        .name()
        .map_err(|err| ModpackExtractError::Extract(format!("read archive entry name: {err}")))?
        .into_owned();
    let Some(relative) = name.strip_prefix(prefix) else {
        return Ok(());
    };
    if relative.is_empty() || entry.is_dir() {
        return Ok(());
    }
    // 外部归档的成员名：与清单路径用同一套安全规则
    domain::server::modpack::validate_relative_path(relative)
        .map_err(ModpackExtractError::Index)?;
    let target = dest.join(relative);
    if skip_existing && target.exists() {
        return Ok(());
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| ModpackExtractError::Extract(err.to_string()))?;
    }
    let mut out = File::create(&target).map_err(|err| {
        ModpackExtractError::Extract(format!("create {}: {err}", target.display()))
    })?;
    std::io::copy(&mut entry, &mut out).map_err(|err| {
        ModpackExtractError::Extract(format!("write {}: {err}", target.display()))
    })?;
    Ok(())
}

/// 清单原始形状（只做反序列化）
#[derive(Deserialize)]
struct RawIndex {
    #[serde(rename = "formatVersion")]
    format_version: u32,
    game: String,
    #[serde(rename = "versionId")]
    version_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    files: Vec<RawFile>,
    #[serde(default)]
    dependencies: std::collections::BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct RawFile {
    path: String,
    #[serde(default)]
    downloads: Vec<String>,
    #[serde(default, rename = "fileSize")]
    file_size: u64,
    #[serde(default)]
    hashes: RawHashes,
    #[serde(default)]
    env: Option<RawEnv>,
}

/// 环境标记的线上形态
///
/// 领域层不依赖 serde，因此这里另建一份可反序列化的镜像类型，再映射过去。
#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum RawEnvSupport {
    Required,
    Optional,
    Unsupported,
}

impl From<RawEnvSupport> for domain::server::modpack::EnvSupport {
    fn from(raw: RawEnvSupport) -> Self {
        match raw {
            RawEnvSupport::Required => Self::Required,
            RawEnvSupport::Optional => Self::Optional,
            RawEnvSupport::Unsupported => Self::Unsupported,
        }
    }
}

fn default_env() -> RawEnvSupport {
    RawEnvSupport::Optional
}

#[derive(Deserialize)]
struct RawEnv {
    #[serde(default = "default_env")]
    client: RawEnvSupport,
    #[serde(default = "default_env")]
    server: RawEnvSupport,
}

#[derive(Deserialize, Default)]
struct RawHashes {
    #[serde(default)]
    sha1: Option<String>,
    #[serde(default)]
    sha512: Option<String>,
}

/// 把 serde 错误包装成领域错误
///
/// 领域层不依赖 serde，因此这里把错误文本转成 `ModpackError::Parse`；它保留
/// 原始 message（含行号列号），足够排查。
fn serde_err(err: &serde_json::Error) -> domain::server::modpack::ModpackError {
    domain::server::modpack::ModpackError::Parse(err.to_string())
}

/// 供测试：构造一个只含清单的 mrpack 目录路径占位
#[doc(hidden)]
#[must_use]
pub fn mrpack_staging_dir(base: &Path) -> PathBuf {
    base.join("modpack-staging")
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::server::modpack::EnvSupport;

    fn hit(slug: &str, server_side: Option<&str>) -> SearchHit {
        SearchHit {
            project_id: "P".to_owned(),
            project_type: "modpack".to_owned(),
            all_project_types: vec!["modpack".to_owned()],
            title: "Pack".to_owned(),
            description: "Desc".to_owned(),
            author: "author".to_owned(),
            categories: vec!["fabric".to_owned()],
            display_categories: Vec::new(),
            versions: vec!["1.21.1".to_owned()],
            downloads: 100,
            follows: 5,
            icon_url: None,
            date_created: "2024-01-01T00:00:00Z".to_owned(),
            date_modified: "2024-02-01T00:00:00Z".to_owned(),
            latest_version: None,
            license: "MIT".to_owned(),
            environment: vec!["client_and_server".to_owned()],
            client_side: Some("required".to_owned()),
            server_side: server_side.map(str::to_owned),
            disclosure_types: Vec::new(),
            gallery: Vec::new(),
            slug: Some(slug.to_owned()),
            author_id: None,
            organization: None,
            organization_id: None,
            featured_gallery: None,
            color: None,
        }
    }

    #[test]
    fn summary_maps_search_hit_fields() {
        let summary = summary_from_hit(&hit("pack", Some("required")));
        assert_eq!(summary.id, "P");
        assert_eq!(summary.slug.as_deref(), Some("pack"));
        assert_eq!(summary.title, "Pack");
        assert_eq!(summary.author.as_deref(), Some("author"));
        assert_eq!(summary.downloads, 100);
        assert_eq!(summary.follows, 5);
        assert_eq!(summary.license.as_deref(), Some("MIT"));
        assert_eq!(summary.game_versions, vec!["1.21.1".to_owned()]);
    }

    fn version(channel: &str) -> Version {
        Version {
            name: format!("v-{channel}"),
            version_number: "1.0.0".to_owned(),
            changelog: None,
            dependencies: Vec::new(),
            game_versions: vec!["1.21.1".to_owned()],
            version_type: channel.to_owned(),
            loaders: vec!["fabric".to_owned()],
            featured: false,
            status: "listed".to_owned(),
            id: format!("ID-{channel}"),
            project_id: "P".to_owned(),
            author_id: "A".to_owned(),
            date_published: "2024-01-01T00:00:00Z".to_owned(),
            downloads: 7,
            environment: None,
            files: Vec::new(),
        }
    }

    #[test]
    fn version_maps_upstream_fields() {
        let mapped = version_from_upstream(&version("release"));
        assert_eq!(mapped.id, "ID-release");
        assert_eq!(mapped.channel, "release");
        assert!(mapped.is_release());
        assert_eq!(mapped.downloads, 7);
    }

    #[test]
    fn extract_error_wraps_domain_error() {
        // 领域层拒绝的格式版本在这里被包装成 Index 错误
        let err = ModpackIndex::new(
            9,
            "minecraft",
            "1",
            "P",
            ModpackDependencies::default(),
            Vec::new(),
        )
        .expect_err("应拒绝");
        let wrapped = ModpackExtractError::Index(err);
        assert!(matches!(wrapped, ModpackExtractError::Index(_)));
    }

    #[test]
    fn raw_env_support_maps_to_domain() {
        assert_eq!(
            EnvSupport::from(RawEnvSupport::Required),
            EnvSupport::Required
        );
        assert_eq!(EnvSupport::from(default_env()), EnvSupport::Optional);
        assert_eq!(
            EnvSupport::from(RawEnvSupport::Unsupported),
            EnvSupport::Unsupported
        );
    }

    #[test]
    fn file_hashes_are_kept_verbatim() {
        let hashes = RawHashes {
            sha1: Some("a".repeat(40)),
            sha512: Some("b".repeat(128)),
        };
        let mapped = ModpackHashes {
            sha1: hashes.sha1.clone(),
            sha512: hashes.sha512.clone(),
        };
        assert!(mapped.sha1_checksum().is_some());
        assert_eq!(mapped.sha512, hashes.sha512);
    }

    #[test]
    fn mrpack_staging_dir_is_under_base() {
        let base = Path::new("/srv/mc");
        assert!(mrpack_staging_dir(base).starts_with(base));
    }
}
