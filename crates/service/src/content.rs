//! 内容安装编排：搜索 → 挑版本 → 下载 → 落到服务端目录
//!
//! 本层只做编排：判定（类别、落点、候选排序）在 `domain`，查询与解析在
//! `port::content` 的实现（Provider），字节传输在 `port::ArtifactDownloader`。
//! 入口（CLI / API / MCP）只负责把参数搬过来、把结果展示出去。
//!
//! # 幂等与安全
//!
//! - 落点文件名取自上游，但经过 [`port::content::safe_filename`] 去掉路径段，
//!   且再与目标目录拼接后被强制校验仍在目录内——上游伪造 `../` 不能写到目录外
//! - 已存在**同名同版本**的文件不重复下载（`reuse_existing`），换版本时覆盖
//! - 下载走 [`port::ArtifactDownloader`]，校验和与大小由它负责，本层不自己算哈希

use crate::fs_error::FsError;
use domain::server::content::{ContentKind, ContentRequest, rank_candidates};
use port::content::{
    ContentFile, ContentQuery, ContentSearch, ContentSource, ContentSourceRegistry,
    ResolvedContent, safe_filename,
};
use port::{ArtifactDownloader, CancellationToken, DownloadRequest, ProgressSink, SourceError};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use tracing::info;

/// 内容安装失败
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ContentError {
    /// 来源查询 / 解析失败
    #[error(transparent)]
    Source(#[from] SourceError),
    /// 下载或校验失败
    #[error(transparent)]
    Download(#[from] port::DownloadError),
    /// 文件系统失败
    #[error(transparent)]
    Fs(#[from] FsError),
    /// 注册表里没有该来源
    #[error("content source not found: {0}")]
    UnknownSource(String),
    /// 注册表里没有任何来源
    #[error("no content source is configured")]
    NoSource,
    /// 该类别在来源里没有可安装的版本
    #[error("no installable {kind} version for {project} (loader={loader:?}, game={game:?})")]
    NoCandidate {
        /// 项目标识
        project: String,
        /// 类别
        kind: ContentKind,
        /// 要求的加载器
        loader: Option<String>,
        /// 要求的游戏版本
        game: Option<String>,
    },
    /// 光影是客户端资源，装到服务端目录没有意义
    #[error("{kind} is a client-side resource and cannot be installed into a server")]
    ClientOnly {
        /// 被拒绝的类别
        kind: ContentKind,
    },
    /// 安装后的路径逃出了目标目录
    #[error("refusing to install outside {dir}: {target}")]
    Escapes {
        /// 目标目录
        dir: PathBuf,
        /// 计算出的落点
        target: PathBuf,
    },
}

impl ContentError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        FsError::io(path, source).into()
    }
}

/// 一次内容安装的结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentInstall {
    /// 来源名（`modrinth` …）
    pub source: &'static str,
    /// 实际安装的项目 ID
    pub project_id: String,
    /// 项目标题（能取到时）
    pub project_title: Option<String>,
    /// 版本 ID
    pub version_id: String,
    /// 版本号
    pub version_number: String,
    /// 落点
    pub path: PathBuf,
    /// 字节数
    pub size: u64,
    /// 是否命中了已存在的同名文件而跳过下载
    pub reused: bool,
}

/// 整合包安装结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModpackInstall {
    /// 下载并落地的整合包文件
    pub archive: ContentInstall,
}

/// 安装选项：把「装到哪、用哪个来源、哪个版本」收成一个结构
///
/// 单独拆出来是为了让 [`ContentService::install`] 的参数保持在可读范围内；
/// 默认值由 [`InstallOptions::new`] 给出（默认来源、按条件挑版本）。
#[derive(Clone, Copy)]
pub struct InstallOptions<'a> {
    /// 来源名；`None` 用注册表的默认来源
    pub source: Option<&'a str>,
    /// 精确版本 ID；`None` 按条件挑最优候选
    pub version: Option<&'a str>,
    /// 服务端目录
    pub server_dir: &'a Path,
    /// 下载进度观察者
    pub progress: Option<&'a dyn ProgressSink>,
}

impl<'a> InstallOptions<'a> {
    /// 默认选项：默认来源、自动挑版本
    #[must_use]
    pub fn new(server_dir: &'a Path) -> Self {
        Self {
            source: None,
            version: None,
            server_dir,
            progress: None,
        }
    }

    /// 指定来源
    #[must_use]
    pub fn source(mut self, source: &'a str) -> Self {
        self.source = Some(source);
        self
    }

    /// 指定版本
    #[must_use]
    pub fn version(mut self, version: &'a str) -> Self {
        self.version = Some(version);
        self
    }

    /// 指定进度观察者
    #[must_use]
    pub fn progress(mut self, progress: &'a dyn ProgressSink) -> Self {
        self.progress = Some(progress);
        self
    }
}

impl std::fmt::Debug for InstallOptions<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstallOptions")
            .field("source", &self.source)
            .field("version", &self.version)
            .field("server_dir", &self.server_dir)
            .finish_non_exhaustive()
    }
}

/// 内容编排
///
/// 持有来源注册表与下载器；来源可按名字选择（换源），也可注入聚合实现。
pub struct ContentService {
    sources: Arc<ContentSourceRegistry>,
    downloader: Arc<dyn ArtifactDownloader>,
}

impl std::fmt::Debug for ContentService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentService")
            .field("sources", &self.sources.names())
            .finish_non_exhaustive()
    }
}

impl ContentService {
    /// 组合来源注册表与下载器
    #[must_use]
    pub fn new(
        sources: Arc<ContentSourceRegistry>,
        downloader: Arc<dyn ArtifactDownloader>,
    ) -> Self {
        Self {
            sources,
            downloader,
        }
    }

    /// 可选来源名
    #[must_use]
    pub fn source_names(&self) -> Vec<&'static str> {
        self.sources.names()
    }

    /// 选取来源：`None` 用默认（第一个），`Some(name)` 按名查
    ///
    /// # Errors
    ///
    /// 没有任何来源，或指定名字不存在时返回 [`ContentError`]
    pub fn select_source(
        &self,
        name: Option<&str>,
    ) -> Result<Arc<dyn ContentSource>, ContentError> {
        match name {
            Some(name) => self
                .sources
                .get(name)
                .ok_or_else(|| ContentError::UnknownSource(name.to_owned())),
            None => self.sources.default_source().ok_or(ContentError::NoSource),
        }
    }

    /// 搜索内容
    ///
    /// # Errors
    ///
    /// 来源选择失败或上游异常时返回 [`ContentError`]
    pub async fn search(
        &self,
        source: Option<&str>,
        query: &ContentQuery,
    ) -> Result<ContentSearch, ContentError> {
        let source = self.select_source(source)?;
        Ok(source.search(query).await?)
    }

    /// 列出某个项目在当前服务端条件下可安装的候选（已按适配度排序）
    ///
    /// # Errors
    ///
    /// 来源选择失败、上游异常或没有可安装版本时返回 [`ContentError`]
    pub async fn candidates(
        &self,
        source: Option<&str>,
        project: &str,
        request: &ContentRequest,
    ) -> Result<Vec<domain::server::content::ContentCandidate>, ContentError> {
        let source = self.select_source(source)?;
        let mut candidates = source.candidates(project, request).await?;
        // 来源可能已经排过序；再排一次是幂等的，并保证注入的实现也能得到
        // 领域口径的顺序
        candidates = rank_candidates(candidates, request);
        if candidates.is_empty() {
            return Err(ContentError::NoCandidate {
                project: project.to_owned(),
                kind: request.kind,
                loader: request.loader.clone(),
                game: request.game_version.clone(),
            });
        }
        Ok(candidates)
    }

    /// 安装一个内容项目到服务端目录
    ///
    /// 版本选择：`options.version` 给出时精确用该版本；否则按
    /// [`ContentRequest`] 的条件挑出最优候选（加载器匹配 → 游戏版本匹配 →
    /// 正式版优先）。
    ///
    /// # Errors
    ///
    /// 类别不允许装到服务端、来源失败、没有候选版本、下载或落盘失败时返回
    /// [`ContentError`]
    pub async fn install(
        &self,
        project: &str,
        request: &ContentRequest,
        options: InstallOptions<'_>,
        cancel: &CancellationToken,
    ) -> Result<ContentInstall, ContentError> {
        let InstallOptions {
            source,
            version,
            server_dir,
            progress,
        } = options;
        if !request.kind.is_server_usable() {
            return Err(ContentError::ClientOnly { kind: request.kind });
        }
        let source = self.select_source(source)?;

        let version_id = if let Some(explicit) = version {
            explicit.to_owned()
        } else {
            let candidates = self
                .candidates(Some(source.name()), project, request)
                .await?;
            // candidates 已按 score 降序；取第一个
            candidates
                .into_iter()
                .next()
                .map(|candidate| candidate.version_id)
                .ok_or_else(|| ContentError::NoCandidate {
                    project: project.to_owned(),
                    kind: request.kind,
                    loader: request.loader.clone(),
                    game: request.game_version.clone(),
                })?
        };

        let resolved = source.resolve(project, &version_id).await?;
        let install = self
            .download_into(server_dir, request.kind, &resolved, progress, cancel)
            .await?;
        info!(
            source = source.name(),
            project = %resolved.project_id,
            version = %resolved.version_id,
            path = %install.path.display(),
            reused = install.reused,
            "content installed"
        );
        Ok(ContentInstall {
            source: source.name(),
            project_id: resolved.project_id,
            project_title: None,
            version_id: resolved.version_id,
            version_number: resolved.version_number,
            path: install.path,
            size: install.size,
            reused: install.reused,
        })
    }

    /// 安装整合包：把 `.mrpack` / zip 下到服务端的 `mods/` 之外的暂存位置
    ///
    /// 这里**只负责取回整包文件**并放到 `<server_dir>/modpack/`；展开 .mrpack、
    /// 按清单下载各个 mod、改写启动器配置属于后续能力，不在本次范围内。
    /// 落点独立于 `mods/`，避免把整包本身当成一个 mod 加载。
    ///
    /// # Errors
    ///
    /// 来源失败、没有正式版、下载或落盘失败时返回 [`ContentError`]
    pub async fn install_modpack(
        &self,
        project: &str,
        options: InstallOptions<'_>,
        cancel: &CancellationToken,
    ) -> Result<ModpackInstall, ContentError> {
        let InstallOptions {
            source,
            version,
            server_dir,
            progress,
        } = options;
        // 整合包不按加载器 / 游戏版本过滤：一个整包自带它的加载器与 MC 版本
        let request = ContentRequest::new(ContentKind::Modpack);
        let source_ref = self.select_source(source)?;
        let version_id = if let Some(explicit) = version {
            explicit.to_owned()
        } else {
            self.candidates(Some(source_ref.name()), project, &request)
                .await?
                .into_iter()
                .next()
                .map(|candidate| candidate.version_id)
                .ok_or_else(|| ContentError::NoCandidate {
                    project: project.to_owned(),
                    kind: ContentKind::Modpack,
                    loader: None,
                    game: None,
                })?
        };
        let resolved = source_ref.resolve(project, &version_id).await?;
        let archive = self
            .download_named_dir(&server_dir.join("modpack"), &resolved, progress, cancel)
            .await?;
        Ok(ModpackInstall {
            archive: ContentInstall {
                source: source_ref.name(),
                project_id: resolved.project_id,
                project_title: None,
                version_id: resolved.version_id,
                version_number: resolved.version_number,
                path: archive.path,
                size: archive.size,
                reused: archive.reused,
            },
        })
    }

    /// 下载到 `{server_dir}/{kind.install_dir()}/`
    async fn download_into(
        &self,
        server_dir: &Path,
        kind: ContentKind,
        resolved: &ResolvedContent,
        progress: Option<&dyn ProgressSink>,
        cancel: &CancellationToken,
    ) -> Result<Downloaded, ContentError> {
        let dir = server_dir.join(kind.install_dir());
        self.download_named_dir(&dir, resolved, progress, cancel)
            .await
    }

    /// 下载到指定目录（落点 = `dir/{safe filename}`）
    async fn download_named_dir(
        &self,
        dir: &Path,
        resolved: &ResolvedContent,
        progress: Option<&dyn ProgressSink>,
        cancel: &CancellationToken,
    ) -> Result<Downloaded, ContentError> {
        let filename = safe_filename(&resolved.file.filename)?;
        let target = dir.join(&filename);
        // 计算出的落点必须留在目标目录内：`safe_filename` 已去掉路径段，
        // 这里是第二道保险（防未来改动绕过它）
        if !target.starts_with(dir) {
            return Err(ContentError::Escapes {
                dir: dir.to_path_buf(),
                target,
            });
        }
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|source| ContentError::io(dir, source))?;

        let existed = tokio::fs::metadata(&target)
            .await
            .is_ok_and(|meta| meta.len() == resolved.file.size && resolved.file.size > 0);
        if existed {
            return Ok(Downloaded {
                path: target,
                size: resolved.file.size,
                reused: true,
            });
        }

        let request = DownloadRequest {
            location: resolved.file.location.clone(),
            destination: target.clone(),
            expected: resolved.file.checksum.clone(),
            expected_size: (resolved.file.size > 0).then_some(resolved.file.size),
        };
        let artifact = self.downloader.download(&request, progress, cancel).await?;
        Ok(Downloaded {
            path: artifact.path,
            size: artifact.size,
            reused: false,
        })
    }
}

/// 内部下载结果
struct Downloaded {
    path: PathBuf,
    size: u64,
    reused: bool,
}

/// 由服务端变体与版本构造内容请求的便捷入口
///
/// 入口（CLI）只传「服务端是什么加载器、什么 MC 版本」，映射规则在这里收敛。
#[must_use]
pub fn request_for(
    kind: ContentKind,
    variant: &domain::server::version::ServerVariant,
    game_version: &str,
) -> ContentRequest {
    let mut request = ContentRequest::new(kind).game_version(game_version);
    if let Some(loader) = variant.content_loader() {
        request = request.loader(loader);
    }
    request
}

/// 供测试与调用方判断：文件是否已在目标位置且大小吻合
///
/// 只读元信息，不参与安装决策（安装流程用同口径的私有判断）。
///
/// # Errors
///
/// 读取元信息失败（非「不存在」）时返回 [`ContentError`]
pub async fn existing_matches(file: &ContentFile, target: &Path) -> Result<bool, ContentError> {
    match tokio::fs::metadata(target).await {
        Ok(meta) => Ok(meta.len() == file.size),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(ContentError::io(target, err)),
    }
}
