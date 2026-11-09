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

use super::client::{HashAlgorithm, ModrinthSource, SearchIndex, SearchQuery, VersionFilter};
use super::facets::{Facet, FacetTree};
use super::manifest::{Project, SearchHit, Version};
use super::{server_archive, server_file};
use async_trait::async_trait;
use domain::server::modpack::{
    FileEnv, ModpackDependencies, ModpackFile, ModpackHashes, ModpackIndex, PackMemberGraph,
};
use port::SourceError;
use port::content::ContentSourceInfo;
use port::modpack::{
    ModpackDetail, ModpackExtractError, ModpackExtractor, ModpackQuery, ModpackSearch,
    ModpackSource, ModpackSummary, ModpackVersion, PackMemberVerdicts, ResolvedModpack,
};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use tracing::debug;
use zip::ZipArchive;
use zip::result::ZipError;

/// 搜索分页上限（上游约束）
const SEARCH_LIMIT_MAX: u32 = 100;

/// 项目页根地址（展示用；API 根地址是 `api.modrinth.com/v2`）
const WEB_ROOT: &str = "https://modrinth.com";

/// 批量哈希反查的单批上限
///
/// `POST /version_files` 的请求体是一次性数组；按批发送，避免大整合包
/// （常见 300+ 个 jar）一次打爆请求体。
const HASH_QUERY_BATCH: usize = 100;

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
        facets = facets.and(super::facets::server_usable());
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
        // facet 是「粗筛」：实测带 `server_side:required|optional` 的查询仍会
        // 返回 `server_side=unsupported` 的整合包（纯客户端整合包出现在服务端
        // 列表里）。返回体里的字段才是逐条事实，因此在这里按字段再筛一遍——
        // 不额外发请求，因为判定要的数据已经在手里。
        let mut hits = Vec::with_capacity(results.hits.len());
        let mut dropped = 0usize;
        for hit in &results.hits {
            if hit.server_side.as_deref() == Some("unsupported") {
                dropped += 1;
                continue;
            }
            hits.push(summary_from_hit(hit));
        }
        if dropped > 0 {
            // 上游 facet 与字段不一致时留痕：这不是本项目的判定错误，
            // 而是能观测到上游数据自相矛盾
            debug!(
                dropped,
                keyword = query.keyword.as_deref().unwrap_or_default(),
                "upstream returned client-only modpacks despite the server_side facet"
            );
        }
        let total = u64::try_from(results.total_hits).unwrap_or_default();
        Ok(ModpackSearch {
            hits,
            offset: u32::try_from(results.offset).unwrap_or_default(),
            // 被剔除的条目来自本页，命中总数按它下调：明细由字段给出，
            // 总数只能给个下限，多报会让「共 N 个」与翻页对不上
            total: total.saturating_sub(dropped as u64),
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
            // 默认版本优先选带服务端归档的正式版：最新正式版可能只上传了
            // 客户端包（仅客户端模组被标成 server: required，装到服务端会崩）
            let picked = all
                .iter()
                .filter(|version| version.is_release())
                .find(|version| version.has_server_file)
                .or_else(|| all.iter().find(|version| version.is_release()))
                .or_else(|| all.iter().find(|version| version.has_server_file))
                .or_else(|| all.first())
                .ok_or_else(|| SourceError::Upstream {
                    status: 404,
                    message: format!("modpack {id_or_slug} has no versions"),
                })?;
            self.inner.version(&picked.id).await?
        };
        let file = server_file(&version).ok_or_else(|| SourceError::Upstream {
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

    async fn client_only_hashes(&self, hashes: &[String]) -> Result<HashSet<String>, SourceError> {
        Ok(self.pack_member_verdicts(hashes).await?.client_only)
    }

    async fn pack_member_verdicts(
        &self,
        hashes: &[String],
    ) -> Result<PackMemberVerdicts, SourceError> {
        if hashes.is_empty() {
            return Ok(PackMemberVerdicts::default());
        }
        let mut verdicts = PackMemberVerdicts::default();
        // 整包一次性看：依赖关系是包内成员之间的，分批就看不到跨批的依赖
        let mut all_versions: HashMap<String, Version> = HashMap::new();
        for chunk in hashes.chunks(HASH_QUERY_BATCH) {
            let versions = self
                .inner
                .versions_from_hashes(chunk, HashAlgorithm::Sha1)
                .await?;
            all_versions.extend(versions);
        }
        if all_versions.is_empty() {
            // 一个哈希都没命中（第三方站下载的 jar），没有可判定的东西
            return Ok(verdicts);
        }
        let project_ids: Vec<String> = all_versions
            .values()
            .map(|version| version.project_id.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let projects = self.inner.projects(&project_ids).await?;
        verdicts.client_only = client_only_from(&all_versions, &projects);
        // 「谁被依赖」不能一次统计：被跳过的成员不该为别人提供保护。判定
        // 需要迭代到不动点，放在领域层，这里只把上游事实摊成依赖图。
        let graph = member_graph(&all_versions, &verdicts.client_only);
        verdicts.depended_on = graph.depended_on().into_iter().collect();
        Ok(verdicts)
    }

    async fn authoritative_hashes(
        &self,
        hints: &[(String, String)],
    ) -> Result<HashMap<String, String>, SourceError> {
        // 清单里的下载地址形如
        //   https://cdn.modrinth.com/data/{proj}/versions/{version_id}/{filename}
        // 版本 ID 是权威哈希的入口：拿到它即可查 `GET /version/{id}`，逐文件
        // 取当前 sha1。按版本 ID 归并，避免同一版本被查多次。
        let mut by_version: HashMap<String, Vec<String>> = HashMap::new();
        for (url, filename) in hints {
            if let Some(version_id) = modrinth_version_id_from_url(url) {
                by_version
                    .entry(version_id)
                    .or_default()
                    .push(filename.clone());
            }
        }
        let mut out = HashMap::new();
        for (version_id, filenames) in by_version {
            let Ok(version) = self.inner.version(&version_id).await else {
                // 单个版本查失败不应让整次安装失败：调用方据此退回原
                // ChecksumMismatch，行为与未实现本能力一致
                continue;
            };
            let current: HashMap<&str, &super::manifest::VersionFile> = version
                .files
                .iter()
                .map(|file| (file.filename.as_str(), file))
                .collect();
            for filename in filenames {
                if let Some(sha1) = current
                    .get(filename.as_str())
                    .and_then(|file| file.hashes.sha1.as_deref())
                    && sha1.len() == 40
                    && sha1.chars().all(|c| c.is_ascii_hexdigit())
                {
                    out.insert(filename, sha1.to_ascii_lowercase());
                }
            }
        }
        Ok(out)
    }
}

/// 从 Modrinth CDN 下载地址里取出 version ID
///
/// 只认 `cdn.modrinth.com/data/{proj}/versions/{id}/{file}` 形状；其它来源
/// （第三方站、自建镜像）返回 `None`，由调用方退回原错误。
#[must_use]
fn modrinth_version_id_from_url(url: &str) -> Option<String> {
    let url = url.parse::<url::Url>().ok()?;
    if url.host_str() != Some("cdn.modrinth.com") {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    // data / {proj} / versions / {id} / {file}
    match segments.as_slice() {
        ["data", _proj, "versions", id, ..] => Some((*id).to_owned()),
        _ => None,
    }
}

/// 把上游元数据摊成领域层的成员依赖图
///
/// 只看 `dependency_type == required`：`optional` / `incompatible` 不构成
/// 「必须有它才能加载」。依赖以**项目 ID** 表达，因此图里同时保留
/// 「文件 → 项目」与「文件 → 必需依赖的项目」两张表。
///
/// 这一步不能靠读 jar：判定发生在下载之前，此时本地没有文件可读。上游的
/// 版本元数据是唯一权威来源，也正好是 `versions_from_hashes` 已经拿回来的
/// 数据，不额外发请求。
///
/// 真正的「谁留下」由 [`PackMemberGraph::retained`] 算——那是需要迭代到
/// 不动点的判定，收敛在领域层，来源只负责把事实摆好。
fn member_graph(
    versions: &HashMap<String, Version>,
    client_only: &HashSet<String>,
) -> PackMemberGraph {
    let mut graph = PackMemberGraph::default();
    for (hash, version) in versions {
        graph
            .project_of
            .insert(hash.clone(), version.project_id.clone());
        let requires: Vec<String> = version
            .dependencies
            .iter()
            .filter(|dep| dep.dependency_type == "required")
            .filter_map(|dep| dep.project_id.clone())
            .collect();
        graph.requires.insert(hash.clone(), requires);
        if client_only.contains(hash) {
            graph.client_only.insert(hash.clone());
        }
    }
    graph
}

/// 从「哈希 → 版本」与「项目详情」里挑出仅客户端条目
///
/// 只有上游明确标注 `server_side = unsupported` 的项目才算仅客户端；字段缺失
/// 或取值未知都按「可在服务端运行」处理——上游没有回填时宁可多装一个文件，
/// 也不能把真正的服务端模组删掉。
fn client_only_from(versions: &HashMap<String, Version>, projects: &[Project]) -> HashSet<String> {
    let unsupported: HashSet<&str> = projects
        .iter()
        .filter(|project| project.is_server_unsupported())
        .map(|project| project.id.as_str())
        .collect();
    versions
        .iter()
        .filter(|(_hash, version)| unsupported.contains(version.project_id.as_str()))
        .map(|(hash, _version)| hash.clone())
        .collect()
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
        has_server_file: server_archive(version).is_some(),
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
            // 项目 / 版本 ID 由领域层从下载地址里推导并保存，这里只管搬运
            files.push(ModpackFile::new(
                file.path,
                file.downloads,
                file.file_size,
                ModpackHashes {
                    sha1: file.hashes.sha1,
                    sha512: file.hashes.sha512,
                },
                file.env.map(|env| FileEnv {
                    client: env.client.into(),
                    server: env.server.into(),
                }),
            ));
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

    /// 构造一条指向指定项目的最小版本记录
    fn version_in(project_id: &str) -> Version {
        let mut version = version("release");
        version.project_id = project_id.to_owned();
        version
    }

    /// 构造项目详情：只关心 id 与 `server_side`
    fn project(id: &str, server_side: Option<&str>) -> Project {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "team": "T",
            "title": "t",
            "description": "d",
            "body": "b",
            "status": "approved",
            "project_type": "mod",
            "license": {"id": "MIT"},
            "published": "2024-01-01T00:00:00Z",
            "updated": "2024-01-01T00:00:00Z",
            "downloads": 0,
            "followers": 0,
            "server_side": server_side,
        }))
        .expect("project 夹具")
    }

    /// 只有 `server_side = unsupported` 才算仅客户端；
    /// `required` / `optional` / 缺失都不标记
    #[test]
    fn client_only_from_marks_only_unsupported_projects() {
        let mut versions = HashMap::new();
        versions.insert("client-hash".to_owned(), version_in("P-CLIENT"));
        versions.insert("server-hash".to_owned(), version_in("P-SERVER"));
        versions.insert("unknown-hash".to_owned(), version_in("P-UNKNOWN"));
        let projects = vec![
            project("P-CLIENT", Some("unsupported")),
            project("P-SERVER", Some("required")),
            project("P-UNKNOWN", None),
        ];

        let mut marked: Vec<String> = client_only_from(&versions, &projects).into_iter().collect();
        marked.sort();
        assert_eq!(
            marked,
            vec!["client-hash".to_owned()],
            "只应标记上游标注 server_side=unsupported 的文件，实际：{marked:?}"
        );
    }

    /// 项目详情查不到（上游没返回该 id）时不删任何文件
    #[test]
    fn client_only_from_without_project_details_marks_nothing() {
        let mut versions = HashMap::new();
        versions.insert("client-hash".to_owned(), version_in("P-CLIENT"));
        assert!(
            client_only_from(&versions, &[]).is_empty(),
            "没有项目详情时不得把文件判成仅客户端"
        );
    }

    /// 空哈希列表在发请求前短路：地址不可达也不会报错
    #[tokio::test]
    async fn empty_hashes_do_not_touch_network() {
        let source = ModrinthModpackSource::from_source(
            ModrinthSource::for_test("http://127.0.0.1:1/v2").expect("source"),
        );
        let marked = source
            .client_only_hashes(&[])
            .await
            .expect("空输入不应发请求");
        assert!(marked.is_empty());
    }

    /// CDN 下载地址 → version ID：只认 cdn.modrinth.com 的规范形状
    #[test]
    fn version_id_from_url_parses_canonical_cdn() {
        assert_eq!(
            modrinth_version_id_from_url(
                "https://cdn.modrinth.com/data/40FYwb4z/versions/mRry0DgY/caelus-forge-3.2.0%2B1.20.1.jar"
            ),
            Some("mRry0DgY".to_owned())
        );
        // 查询串 / 额外路径段不影响
        assert_eq!(
            modrinth_version_id_from_url(
                "https://cdn.modrinth.com/data/A/versions/vID/file.jar?download=1"
            ),
            Some("vID".to_owned())
        );
    }

    /// 非 cdn.modrinth.com 或非规范路径一律返回 None（第三方站 / 镜像）
    #[test]
    fn version_id_from_url_rejects_foreign_urls() {
        assert_eq!(
            modrinth_version_id_from_url("https://example.com/data/A/versions/vID/x.jar"),
            None
        );
        assert_eq!(
            modrinth_version_id_from_url("https://cdn.modrinth.com/data/A/x.jar"),
            None
        );
        assert_eq!(modrinth_version_id_from_url("not a url"), None);
    }
}
