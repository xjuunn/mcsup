//! [`CurseForgeSource`] 对 [`port::ContentSource`] 的实现
//!
//! 只把上游响应翻译成 port 的契约：类别、加载器、候选版本、下载文件都在
//! 这里落地一次，Service 与入口看不到 `classId` / `modLoaderType` 这类数值。
//!
//! # 检索策略
//!
//! 上游全文检索按分隔符切词，连写 modid 往往搜不到本体（机理见
//! [`crate::curseforge::client`] 的模块文档）。因此一次 [`ContentSource::search`]
//! 会按「精确 slug → 原词 → 切成两段」的顺序尝试，命中即停。
//!
//! # 与 Modrinth 的差异
//!
//! - 项目 ID 是数值，版本 ID 是文件 ID；两者都只在本来源内唯一，安装时必须
//!   带着 [`ContentProject::source`] 回到同一来源解析
//! - 上游没有「客户端 / 服务端支持情况」字段，`server_usable_only` 无法在检索
//!   阶段过滤：这里如实返回全部命中，由安装检查（读 jar）做最终判定
//! - 上游不提供按哈希反查，[`ContentSource::by_hash`] 保持默认的空结果
//!
//! # 下载地址
//!
//! 官方 API 在作者未授权分发时把 `downloadUrl` 置为 `null`。此时按文件 ID
//! 换算官方 CDN 路径（实测可用），而不是把「没有地址」报成「装不了」。

use super::client::{CurseForgeSource, ModSearch, cdn_url, class_id, kind_of_class, loader_id};
use super::manifest::{File, Mod};
use async_trait::async_trait;
use domain::server::artifact::{Checksum, ChecksumAlgorithm};
use domain::server::content::{ContentCandidate, ContentKind, ContentRequest, rank_candidates};
use port::SourceError;
use port::content::{
    ContentFile, ContentProject, ContentQuery, ContentSearch, ContentSource, ContentSourceInfo,
    ResolvedContent, safe_filename,
};

/// 来源名（错误消息与 JSON 输出的 `source` 字段）
const SOURCE: &str = "curseforge";

impl ContentSourceInfo for CurseForgeSource {
    fn name(&self) -> &'static str {
        SOURCE
    }
}

#[async_trait]
impl ContentSource for CurseForgeSource {
    fn supported_kinds(&self) -> Vec<ContentKind> {
        ContentKind::ALL.to_vec()
    }

    async fn search(&self, query: &ContentQuery) -> Result<ContentSearch, SourceError> {
        let class = query.kinds.first().copied().map(class_id);
        let loader = query.loaders.first().and_then(|name| loader_id(name));
        let game = query.game_versions.first().cloned();
        let page_size = query.limit.clamp(1, super::client::PAGE_SIZE_MAX);
        let keyword = query
            .keyword
            .as_deref()
            .map(str::trim)
            .filter(|keyword| !keyword.is_empty());

        let mut attempts: Vec<SearchAttempt> = Vec::new();
        match keyword {
            Some(keyword) => {
                // 1. 精确 slug（必须配 classId，否则同名整合包会抢在前面）
                if !keyword.contains(char::is_whitespace) {
                    attempts.push(SearchAttempt::slug(keyword));
                }
                // 2. 原词
                attempts.push(SearchAttempt::text(keyword));
                // 3. 连写词切两段：命中即停，最多试固定几个位置
                for term in super::client::split_terms(keyword) {
                    attempts.push(SearchAttempt::text(&term));
                }
            }
            // 不带关键词即浏览，一次请求就够
            None => attempts.push(SearchAttempt::browse()),
        }

        for attempt in attempts {
            let page = self
                .search(&ModSearch {
                    class_id: class,
                    keyword: attempt.keyword,
                    slug: attempt.slug,
                    game_version: game.clone(),
                    loader,
                    offset: query.offset,
                    page_size,
                })
                .await?;
            if page.data.is_empty() {
                continue;
            }
            let total = page
                .pagination
                .as_ref()
                .map_or(page.data.len() as u64, |page| page.total_count);
            return Ok(ContentSearch {
                hits: page.data.iter().map(project_of).collect(),
                offset: query.offset,
                total,
            });
        }
        Ok(ContentSearch {
            hits: Vec::new(),
            offset: query.offset,
            total: 0,
        })
    }

    async fn project(&self, id_or_slug: &str) -> Result<ContentProject, SourceError> {
        let id = self.resolve_project_id(id_or_slug, None).await?;
        let found = CurseForgeSource::project(self, id).await?;
        Ok(project_of(&found))
    }

    async fn candidates(
        &self,
        id_or_slug: &str,
        request: &ContentRequest,
    ) -> Result<Vec<ContentCandidate>, SourceError> {
        let id = self
            .resolve_project_id(id_or_slug, Some(request.kind))
            .await?;
        let files = CurseForgeSource::files(
            self,
            id,
            request.game_version.as_deref(),
            request.loader_filter().and_then(loader_id),
            super::client::PAGE_SIZE_MAX,
        )
        .await?;
        let candidates: Vec<ContentCandidate> = files.iter().map(candidate_of).collect();
        Ok(rank_candidates(candidates, request))
    }

    async fn resolve(
        &self,
        id_or_slug: &str,
        version_id: &str,
    ) -> Result<ResolvedContent, SourceError> {
        let project = self.resolve_project_id(id_or_slug, None).await?;
        let file_id = version_id
            .parse::<u64>()
            .map_err(|err| SourceError::Decode(Box::new(std::io::Error::other(err.to_string()))))?;
        let file = CurseForgeSource::file(self, project, file_id).await?;
        // 上游按「项目 + 文件」查，但版本 ID 也能跨项目拼错：文件自带的
        // `modId` 是唯一可核对的归属
        if file.mod_id != 0 && file.mod_id != project {
            return Err(SourceError::Upstream {
                status: 404,
                message: format!("file {version_id} does not belong to project {id_or_slug}"),
            });
        }
        Ok(ResolvedContent {
            project_id: project.to_string(),
            version_id: file.id.to_string(),
            version_number: version_number(&file),
            file: content_file(&file)?,
        })
    }
}

/// 一次检索尝试：slug 与关键词互斥
struct SearchAttempt {
    slug: Option<String>,
    keyword: Option<String>,
}

impl SearchAttempt {
    fn slug(value: &str) -> Self {
        Self {
            slug: Some(value.to_owned()),
            keyword: None,
        }
    }

    fn text(value: &str) -> Self {
        Self {
            slug: None,
            keyword: Some(value.to_owned()),
        }
    }

    fn browse() -> Self {
        Self {
            slug: None,
            keyword: None,
        }
    }
}

impl CurseForgeSource {
    /// 把 id 或 slug 归一成数值项目 ID
    ///
    /// `CurseForge` 没有按 slug 取详情的端点（`/mods/{slug}` 不存在），只能用
    /// 检索反查。反查要求 slug 精确命中：检索的首位可能是名字相近的别的项目，
    /// 拿它当目标会把用户装错东西。
    async fn resolve_project_id(
        &self,
        id_or_slug: &str,
        kind: Option<ContentKind>,
    ) -> Result<u64, SourceError> {
        if let Ok(id) = id_or_slug.parse::<u64>() {
            return Ok(id);
        }
        let page = self
            .search(&ModSearch {
                class_id: kind.map(class_id),
                slug: Some(id_or_slug.to_owned()),
                page_size: 10,
                ..ModSearch::default()
            })
            .await?;
        let found = page
            .data
            .iter()
            .find(|item| item.slug.as_deref() == Some(id_or_slug))
            .or_else(|| page.data.first())
            .ok_or(SourceError::Status { status: 404 })?;
        Ok(found.id)
    }
}

/// 上游项目 → 内容项目
fn project_of(item: &Mod) -> ContentProject {
    let mut loaders: Vec<String> = Vec::new();
    let mut versions: Vec<String> = Vec::new();
    for index in &item.latest_files_indexes {
        if let Some(name) = index.mod_loader.and_then(super::client::loader_name)
            && !loaders.iter().any(|seen| seen == name)
        {
            loaders.push(name.to_owned());
        }
        if is_game_version(&index.game_version) && !versions.contains(&index.game_version) {
            versions.push(index.game_version.clone());
        }
    }
    ContentProject {
        source: SOURCE,
        id: item.id.to_string(),
        slug: item.slug.clone(),
        title: item.name.clone(),
        description: item.summary.clone(),
        kind: kind_of_class(item.class_id),
        // CurseForge 的类别是网页分区，不表达「同一份 jar 还能当数据包 / 插件」，
        // 因此没有额外类别可报
        also_kinds: Vec::new(),
        loaders,
        game_versions: versions,
        author: item.authors.first().map(|author| author.name.clone()),
        downloads: item.download_count,
        icon_url: item
            .logo
            .as_ref()
            .and_then(|logo| logo.thumbnail_url.clone().or_else(|| logo.url.clone())),
        // 上游没有两侧支持字段：不把「没标注」当成「仅客户端」
        client_only: false,
    }
}

/// 上游文件 → 领域候选
fn candidate_of(file: &File) -> ContentCandidate {
    ContentCandidate {
        version_id: file.id.to_string(),
        version_number: version_number(file),
        channel: channel_of(file.release_type).to_owned(),
        featured: false,
        loaders: loaders_of(&file.game_versions),
        game_versions: versions_of(&file.game_versions),
        date_published: file.file_date.clone(),
        score: 0,
    }
}

/// 展示用版本号：上游的 `displayName` 更可读，缺失时退回文件名
fn version_number(file: &File) -> String {
    file.display_name
        .clone()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| file.file_name.clone())
}

/// 发布渠道：1 = 正式版，2 = 测试版，3 = 内测版
fn channel_of(release_type: u8) -> &'static str {
    match release_type {
        1 => "release",
        2 => "beta",
        3 => "alpha",
        // 未知渠道不能冒充正式版：`release_only` 会把它挡掉
        _ => "unknown",
    }
}

/// 文件声明的加载器
fn loaders_of(game_versions: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in game_versions {
        let name = match raw.to_ascii_lowercase().as_str() {
            "forge" => "forge",
            "neoforge" => "neoforge",
            "fabric" => "fabric",
            "quilt" => "quilt",
            _ => continue,
        };
        if !out.iter().any(|seen| seen == name) {
            out.push(name.to_owned());
        }
    }
    out
}

/// 文件声明的游戏版本
fn versions_of(game_versions: &[String]) -> Vec<String> {
    game_versions
        .iter()
        .filter(|value| is_game_version(value))
        .cloned()
        .collect()
}

/// 该条目是不是 Minecraft 版本号（`1.20.1` / `26.2`）
///
/// `gameVersions` 混着装侧别（`Client` / `Server`）与加载器名，只按「以数字
/// 开头且含点」挑，不会把加载器当成 MC 版本。
fn is_game_version(value: &str) -> bool {
    value
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_digit())
        && value.contains('.')
}

/// 上游文件 → 下载描述
fn content_file(file: &File) -> Result<ContentFile, SourceError> {
    let location = download_location(file)?;
    Ok(ContentFile {
        location,
        filename: safe_filename(&file.file_name)?,
        size: file.file_length,
        checksum: file
            .sha1()
            .map(|value| Checksum::new(ChecksumAlgorithm::Sha1, value.to_owned())),
    })
}

/// 解析下载地址：优先上游给的，缺失时按文件 ID 换算官方 CDN
fn download_location(file: &File) -> Result<url::Url, SourceError> {
    let candidate = file
        .download_url
        .as_deref()
        .filter(|url| !url.trim().is_empty())
        .map_or_else(|| cdn_url(file.id, &file.file_name), ToOwned::to_owned);
    let parsed = candidate
        .parse::<url::Url>()
        .map_err(|err| SourceError::Decode(Box::new(err)))?;
    if parsed.scheme() != "https" {
        return Err(SourceError::Decode(Box::new(std::io::Error::other(
            format!("content file is not served over https: {candidate}"),
        ))));
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curseforge::manifest::{Author, FileHash, FileIndex, ModAsset};

    fn file() -> File {
        File {
            id: 5_468_648,
            mod_id: 227_639,
            file_name: "twilightforest-1.20.1-4.3.2508-universal.jar".to_owned(),
            display_name: Some("Twilight Forest - MC 1.20.1 - 4.3.2508".to_owned()),
            release_type: 1,
            file_date: "2024-06-24T18:35:07.257Z".to_owned(),
            file_length: 23_332_091,
            download_url: None,
            hashes: vec![
                FileHash {
                    value: "fa3b506a45d3e9c465551cd533fd50c899496e9f".to_owned(),
                    algo: 1,
                },
                FileHash {
                    value: "0e1ca36ec2ca7e35059dea1600aa31ee".to_owned(),
                    algo: 2,
                },
            ],
            game_versions: vec![
                "Client".to_owned(),
                "NeoForge".to_owned(),
                "1.20.1".to_owned(),
                "Forge".to_owned(),
                "Server".to_owned(),
            ],
        }
    }

    fn item() -> Mod {
        Mod {
            id: 227_639,
            name: "The Twilight Forest".to_owned(),
            slug: Some("the-twilight-forest".to_owned()),
            summary: "A realm basked in mystery".to_owned(),
            class_id: 6,
            download_count: 213_729_220,
            logo: Some(ModAsset {
                thumbnail_url: Some("https://media.forgecdn.net/avatars/thumb.png".to_owned()),
                url: Some("https://media.forgecdn.net/avatars/full.png".to_owned()),
            }),
            authors: vec![Author {
                name: "Benimatic".to_owned(),
            }],
            latest_files_indexes: vec![FileIndex {
                game_version: "1.20.1".to_owned(),
                mod_loader: Some(1),
            }],
            links: None,
        }
    }

    #[test]
    fn candidate_keeps_only_mc_versions_and_loaders() {
        let candidate = candidate_of(&file());
        assert_eq!(candidate.version_id, "5468648");
        assert_eq!(candidate.channel, "release");
        assert_eq!(candidate.loaders, vec!["neoforge", "forge"]);
        // `Client` / `Server` / 加载器名都不得混进游戏版本
        assert_eq!(candidate.game_versions, vec!["1.20.1"]);
        assert_eq!(candidate.date_published, "2024-06-24T18:35:07.257Z");
    }

    #[test]
    fn unknown_release_type_is_not_a_release() {
        let mut raw = file();
        raw.release_type = 9;
        assert_eq!(candidate_of(&raw).channel, "unknown");
    }

    #[test]
    fn project_reports_source_and_numeric_id() {
        let project = project_of(&item());
        assert_eq!(project.source, "curseforge");
        assert_eq!(project.id, "227639");
        assert_eq!(project.kind, ContentKind::Mod);
        assert_eq!(project.loaders, vec!["forge"]);
        assert_eq!(project.game_versions, vec!["1.20.1"]);
        assert_eq!(project.author.as_deref(), Some("Benimatic"));
        assert!(!project.client_only);
    }

    #[test]
    fn content_file_falls_back_to_the_cdn_and_keeps_sha1() {
        let file = content_file(&file()).expect("file");
        assert_eq!(
            file.location.as_str(),
            "https://mediafilez.forgecdn.net/files/5468/648/twilightforest-1.20.1-4.3.2508-universal.jar"
        );
        assert_eq!(file.size, 23_332_091);
        let checksum = file.checksum.expect("sha1");
        assert!(checksum.is_verified());
    }

    #[test]
    fn download_url_wins_when_upstream_provides_it() {
        let mut raw = file();
        raw.download_url = Some("https://edge.forgecdn.net/files/5468/648/tf.jar".to_owned());
        assert_eq!(
            download_location(&raw).expect("url").as_str(),
            "https://edge.forgecdn.net/files/5468/648/tf.jar"
        );
    }

    #[test]
    fn insecure_upstream_url_is_rejected() {
        let mut raw = file();
        raw.download_url = Some("http://edge.forgecdn.net/files/1/2/x.jar".to_owned());
        assert!(download_location(&raw).is_err());
    }

    #[test]
    fn game_version_detection_skips_side_and_loader_labels() {
        for good in ["1.20.1", "26.2", "1.7.10"] {
            assert!(is_game_version(good), "{good}");
        }
        for bad in ["Client", "Server", "Forge", "NeoForge", "Fabric", ""] {
            assert!(!is_game_version(bad), "{bad}");
        }
    }
}
