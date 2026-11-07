//! 整合包开服编排：清单 → 加载器 → overrides → 服务端文件
//!
//! 整合包不是一个「可下载物」，而是一份**服务端布置方案**。安装一台整合包
//! 服务端按以下顺序进行，每一步失败都要能说清「哪一步失败、已完成什么」：
//!
//! 1. 取回 `.mrpack`（[`port::ModpackSource::resolve`] + 下载器）
//! 2. 读清单（[`port::ModpackExtractor::read_index`]），拿到 MC 版本与 loader
//! 3. 装对应加载器核心（复用 `ServerInstaller` 的「来源 + 下载器」能力）
//! 4. 铺 `overrides/`（配置、资源包等不需要联网的文件）
//! 5. 按清单下载服务端需要的文件到各自目录（`mods/`、`config/` …）
//!
//! # 为什么 loader 安装放在第 3 步
//!
//! 加载器核心会写入 `libraries/` 与启动参数文件，而 overrides / mods 只是往
//! 服务端目录里放文件——先装核心可以让「核心装失败」尽早暴露，不必白下几百
//! MB 的 mod。
//!
//! # 与 [`crate::ContentService`] 的分工
//!
//! `ContentService` 给**已有**服务端装单个内容；`ModpackService` 按整合包把
//! 一台服务端布置出来。两者共用下载器与文件落盘口径，但不互相调用。

use crate::fs_error::FsError;
use crate::lifecycle::LifecycleError;
use domain::server::modpack::{ModpackFile, ModpackIndex};
use domain::server::version::ServerVariant;
use futures::StreamExt;
use futures::stream;
use port::modpack::{
    ModpackExtractError, ModpackExtractor, ModpackSource, ModpackSourceRegistry, ResolvedModpack,
};
use port::{ArtifactDownloader, CancellationToken, DownloadRequest, ProgressSink, SourceError};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use thiserror::Error;
use tracing::info;

/// 整合包安装失败
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ModpackError {
    /// 来源查询 / 解析失败
    #[error(transparent)]
    Source(#[from] SourceError),
    /// 下载或校验失败
    #[error(transparent)]
    Download(#[from] port::DownloadError),
    /// 清单解析 / 归档解压失败
    #[error(transparent)]
    Extract(#[from] ModpackExtractError),
    /// 文件系统失败
    #[error(transparent)]
    Fs(#[from] FsError),
    /// 加载器核心安装失败
    #[error(transparent)]
    Lifecycle(#[from] LifecycleError),
    /// 注册表里没有该来源
    #[error("modpack source not found: {0}")]
    UnknownSource(String),
    /// 没有配置任何整合包来源
    #[error("no modpack source is configured")]
    NoSource,
    /// 清单没有声明 Minecraft 版本
    #[error("modpack {0} does not declare a minecraft version")]
    MissingMinecraft(String),
    /// 清单里的加载器没有对应的服务端核心来源
    #[error("no server core source for modpack loader {0}")]
    UnsupportedLoader(String),
    /// 清单里的文件路径逃出了服务端目录
    #[error("refusing to install outside {dir}: {target}")]
    Escapes {
        /// 服务端目录
        dir: PathBuf,
        /// 计算出的落点
        target: PathBuf,
    },
}

impl ModpackError {
    /// 失败等级
    ///
    /// 清单本身不可用（缺 Minecraft 版本 / 加载器无核心来源 / 来源没配）都是
    /// 「这个整合包装不了」，归用法错误；`Extract` 自带细分等级。
    #[must_use]
    pub fn category(&self) -> port::ErrorCategory {
        match self {
            Self::Source(err) => err.category(),
            Self::Download(err) => err.category(),
            Self::Extract(err) => err.category(),
            Self::Fs(err) => err.category(),
            Self::Lifecycle(err) => err.category(),
            Self::UnknownSource(_)
            | Self::NoSource
            | Self::MissingMinecraft(_)
            | Self::UnsupportedLoader(_) => port::ErrorCategory::Usage,
            Self::Escapes { .. } => port::ErrorCategory::Backend,
        }
    }

    fn io(path: &Path, source: std::io::Error) -> Self {
        FsError::io(path, source).into()
    }
}

/// 整合包安装选项
#[derive(Clone, Copy)]
pub struct ModpackInstallOptions<'a> {
    /// 来源名；`None` 用注册表默认来源
    pub source: Option<&'a str>,
    /// 精确版本 ID；`None` 挑最新正式版
    pub version: Option<&'a str>,
    /// 服务端目录
    pub server_dir: &'a Path,
    /// 下载进度观察者
    pub progress: Option<&'a dyn ProgressSink>,
}

impl<'a> ModpackInstallOptions<'a> {
    /// 默认选项
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

impl std::fmt::Debug for ModpackInstallOptions<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModpackInstallOptions")
            .field("source", &self.source)
            .field("version", &self.version)
            .field("server_dir", &self.server_dir)
            .finish_non_exhaustive()
    }
}

/// 安装完成后的结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModpackInstalled {
    /// 来源名
    pub source: &'static str,
    /// 项目 ID
    pub project_id: String,
    /// 版本 ID
    pub version_id: String,
    /// 版本号
    pub version_number: String,
    /// 清单声明的 Minecraft 版本
    pub minecraft: String,
    /// 清单要求的加载器（`None` 表示原版整合包）
    pub loader: Option<(String, String)>,
    /// 服务端需要安装的文件数（含 overrides）
    pub file_count: usize,
    /// 本次下载的字节数
    pub downloaded_bytes: u64,
    /// 已存在而跳过的文件数
    pub reused: usize,
}

/// 整合包开服编排
pub struct ModpackService {
    sources: Arc<ModpackSourceRegistry>,
    extractor: Arc<dyn ModpackExtractor>,
    downloader: Arc<dyn ArtifactDownloader>,
}

impl std::fmt::Debug for ModpackService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModpackService")
            .field("sources", &self.sources.names())
            .finish_non_exhaustive()
    }
}

impl ModpackService {
    /// 组合各能力
    #[must_use]
    pub fn new(
        sources: Arc<ModpackSourceRegistry>,
        extractor: Arc<dyn ModpackExtractor>,
        downloader: Arc<dyn ArtifactDownloader>,
    ) -> Self {
        Self {
            sources,
            extractor,
            downloader,
        }
    }

    /// 可选来源名
    #[must_use]
    pub fn source_names(&self) -> Vec<&'static str> {
        self.sources.names()
    }

    /// 选取来源
    ///
    /// # Errors
    ///
    /// 指定名字不存在或没有任何来源时返回 [`ModpackError`]
    pub fn select_source(
        &self,
        name: Option<&str>,
    ) -> Result<Arc<dyn ModpackSource>, ModpackError> {
        match name {
            Some(name) => self
                .sources
                .get(name)
                .ok_or_else(|| ModpackError::UnknownSource(name.to_owned())),
            None => self.sources.default_source().ok_or(ModpackError::NoSource),
        }
    }

    /// 取回整合包并读清单，不落任何服务端文件
    ///
    /// 供 `server create` 在**创建服务器之前**拿到 MC 版本与加载器（这两个值
    /// 要写进注册表）。返回暂存目录，调用方后续用 [`Self::apply`] 复用。
    ///
    /// # Errors
    ///
    /// 来源失败、下载失败、清单非法时返回 [`ModpackError`]
    pub async fn prepare(
        &self,
        project: &str,
        options: ModpackInstallOptions<'_>,
        cancel: &CancellationToken,
    ) -> Result<ModpackPrepared, ModpackError> {
        let source = self.select_source(options.source)?;
        let resolved = source.resolve(project, options.version).await?;
        let staging = options.server_dir.join("modpack-staging");
        tokio::fs::create_dir_all(&staging)
            .await
            .map_err(|err| ModpackError::io(&staging, err))?;
        let archive_path = staging.join(&resolved.file.filename);
        self.download_file(&resolved.file, &archive_path, options.progress, cancel)
            .await?;

        let index = self.extractor.read_index(&archive_path)?;
        Ok(ModpackPrepared {
            source: source.name(),
            resolved,
            index,
            archive_path,
        })
    }

    /// 把准备好的整合包布置到服务端目录
    ///
    /// 顺序：装加载器 → 铺 overrides → 下载清单文件。加载器由调用方通过
    /// `install_core` 完成（Service 不持有 registry 与 Java 选择逻辑）——
    /// 这样 `server create` 与 `server install` 复用同一套安装流程。
    ///
    /// # Errors
    ///
    /// 落点越界、overrides 解压失败、文件下载失败时返回 [`ModpackError`]
    pub async fn apply(
        &self,
        prepared: &ModpackPrepared,
        server_dir: &Path,
        progress: Option<&dyn ProgressSink>,
        cancel: &CancellationToken,
    ) -> Result<ModpackApplied, ModpackError> {
        tokio::fs::create_dir_all(server_dir)
            .await
            .map_err(|err| ModpackError::io(server_dir, err))?;

        // 1. overrides：服务端侧配置与静态资源
        self.extractor
            .extract_overrides(&prepared.archive_path, server_dir)?;

        // 2. 仅客户端模组：清单的 env 字段不可全信。整合包作者常把 ETF /
        //    Embeddium 之类仅客户端模组标成 `server: required`，按清单装进
        //    服务端会在启动时加载客户端类并崩溃。能查到权威元数据时以它为准。
        let source = self.select_source(Some(prepared.source))?;
        let mod_hashes: Vec<String> = prepared
            .index
            .downloadable_server_files()
            .into_iter()
            .filter(|file| {
                file.path.starts_with("mods/")
                    && Path::new(&file.path)
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("jar"))
            })
            .filter_map(|file| file.hashes.sha1.clone())
            .collect();
        // 纯配置 / 资源包整合包没有可反查的 jar，直接省掉一次上游请求
        let client_only = if mod_hashes.is_empty() {
            HashSet::new()
        } else {
            source.client_only_hashes(&mod_hashes).await?
        };

        // 3. 清单文件：只装服务端需要的（server: unsupported 已由领域层过滤），
        //    并行下载，进度收敛为整包口径（见 [`Self::download_files`]）
        let (jobs, skipped_client_only) = self
            .classify_files(prepared, server_dir, &client_only)
            .await?;
        let mut applied = self
            .download_files(prepared, source.as_ref(), &jobs, progress, cancel)
            .await?;
        applied.file_count = prepared.index.server_files().len() - skipped_client_only;
        applied.skipped_client_only = skipped_client_only;
        Ok(applied)
    }

    /// 把清单文件分类：要下载的、仅客户端剔除的
    ///
    /// 仅客户端文件顺手清掉旧落盘——重装时留着只会让服务端起不来。
    async fn classify_files<'a>(
        &self,
        prepared: &'a ModpackPrepared,
        server_dir: &Path,
        client_only: &HashSet<String>,
    ) -> Result<(Vec<(&'a ModpackFile, PathBuf)>, usize), ModpackError> {
        let mut skipped_client_only = 0_usize;
        let mut jobs: Vec<(&'a ModpackFile, PathBuf)> = Vec::new();
        for file in prepared.index.downloadable_server_files() {
            let target = safe_target(server_dir, &file.path)?;
            if file
                .hashes
                .sha1
                .as_ref()
                .is_some_and(|hash| client_only.contains(hash))
            {
                // 上一次安装可能已经把它落盘；重装时顺手清掉，避免旧文件
                // 继续让服务端起不来
                if tokio::fs::try_exists(&target).await.unwrap_or(false) {
                    tokio::fs::remove_file(&target)
                        .await
                        .map_err(|err| ModpackError::io(&target, err))?;
                }
                skipped_client_only += 1;
                continue;
            }
            jobs.push((file, target));
        }
        Ok((jobs, skipped_client_only))
    }

    /// 并行下载清单文件，进度收敛为整包口径
    ///
    /// 并行下载：清单文件动辄上百个、彼此独立，串行是纯 IO 等待的浪费。
    /// 并发度有界（[`FILE_CONCURRENCY`]），不放大上游压力；单文件内部
    /// 的分片下载保持不动（file crate 自己决定怎么跑满一条文件）。
    ///
    /// 进度口径收敛为整包：清单里每个文件都带 size，总字节可知；
    /// 已复用（reused）的文件按声明 size 直接计入已完成，其余按下载
    /// 增量累加。上层收到的 `on_progress(done, Some(total))` 从此是
    /// 整包视角，不再被单个文件的进度反复归零。
    async fn download_files(
        &self,
        prepared: &ModpackPrepared,
        source: &dyn ModpackSource,
        jobs: &[(&ModpackFile, PathBuf)],
        progress: Option<&dyn ProgressSink>,
        cancel: &CancellationToken,
    ) -> Result<ModpackApplied, ModpackError> {
        // 总字节与已复用字节：进度条的总长一次算清
        let total_bytes: u64 = jobs.iter().map(|(file, _)| file.size).sum();
        // 先把「已存在且校验一致」的文件找出来：它们的字节直接计入已完成
        let mut reused_bytes = 0_u64;
        let mut to_download: Vec<(&ModpackFile, PathBuf)> = Vec::new();
        for (file, target) in jobs {
            if let Some(parent) = target.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|err| ModpackError::io(parent, err))?;
            }
            if existing_matches(target, file).await {
                reused_bytes += file.size;
            } else {
                to_download.push((file, target.clone()));
            }
        }
        let reused = jobs.len() - to_download.len();

        // 共享计数器：已完成字节（复用 + 下载增量）
        let done = Arc::new(AtomicU64::new(reused_bytes));
        if let Some(sink) = progress {
            sink.on_progress(done.load(Ordering::Relaxed), Some(total_bytes));
        }
        // 清单哈希过期、但上游权威哈希与下载字节一致的文件路径
        let stale_hashes = Mutex::new(Vec::new());
        let source_ref = source;
        let fetched: Vec<Result<FetchedFile, ModpackError>> = {
            let stale_hashes = &stale_hashes;
            stream::iter(to_download.into_iter().map(|(file, target)| {
                let url = first_https(&file.downloads);
                let done = Arc::clone(&done);
                async move {
                    let url = url?;
                    // 每个并发文件一个聚合 sink：把「文件内已传字节」换算成
                    // 整包口径的绝对值上报
                    let aggregate =
                        progress.map(|sink| AggregateSink::new(Arc::clone(&done), sink));
                    let sink_ref = aggregate.as_ref().map(|s| s as &dyn ProgressSink);
                    let fetched = self
                        .fetch_one_file(source_ref, file, url, &target, sink_ref, cancel)
                        .await?;
                    done.fetch_add(fetched.bytes, Ordering::Relaxed);
                    if let Some(sink) = progress {
                        sink.on_progress(done.load(Ordering::Relaxed), Some(total_bytes));
                    }
                    if fetched.stale
                        && let Ok(mut list) = stale_hashes.lock()
                    {
                        list.push(file.path.clone());
                    }
                    Ok(fetched)
                }
            }))
            .buffer_unordered(FILE_CONCURRENCY)
            .collect::<Vec<_>>()
            .await
        };

        let mut downloaded_bytes = 0_u64;
        for result in fetched {
            downloaded_bytes += result?.bytes;
        }
        let stale_hashes = stale_hashes.into_inner().unwrap_or_default();

        info!(
            project = %prepared.resolved.project_id,
            version = %prepared.resolved.version_id,
            files = prepared.index.server_files().len(),
            downloaded_bytes,
            reused,
            stale_hashes = stale_hashes.len(),
            "modpack files downloaded"
        );
        Ok(ModpackApplied {
            file_count: 0,
            downloaded_bytes,
            reused,
            skipped_client_only: 0,
            stale_hashes,
        })
    }

    /// 下载单个文件到指定路径
    async fn download_file(
        &self,
        file: &port::content::ContentFile,
        destination: &Path,
        progress: Option<&dyn ProgressSink>,
        cancel: &CancellationToken,
    ) -> Result<(), ModpackError> {
        let request = DownloadRequest {
            location: file.location.clone(),
            destination: destination.to_path_buf(),
            expected: file.checksum.clone(),
            expected_size: (file.size > 0).then_some(file.size),
        };
        self.downloader.download(&request, progress, cancel).await?;
        Ok(())
    }

    /// 下载单个清单文件，必要时按上游权威哈希修正过期清单哈希
    ///
    /// 先用清单里的 sha1 强校验下载；一旦报 `ChecksumMismatch`，说明上游文件
    /// 被重新上传、清单哈希过期。此时回源取**当前**权威哈希：若与刚下载的字节
    /// 一致则接受并按权威哈希重下一次（仍强校验），否则如实抛出原错误。
    async fn fetch_one_file(
        &self,
        source: &dyn ModpackSource,
        file: &ModpackFile,
        url: url::Url,
        target: &Path,
        progress: Option<&dyn ProgressSink>,
        cancel: &CancellationToken,
    ) -> Result<FetchedFile, ModpackError> {
        let expected_size = (file.size > 0).then_some(file.size);
        let request = DownloadRequest {
            location: url.clone(),
            destination: target.to_path_buf(),
            expected: file.hashes.sha1_checksum(),
            expected_size,
        };
        match self.downloader.download(&request, progress, cancel).await {
            Ok(downloaded) => Ok(FetchedFile {
                bytes: downloaded.size,
                stale: false,
            }),
            Err(port::DownloadError::ChecksumMismatch { expected, actual }) => {
                let Some(authoritative) =
                    authoritative_sha1(source, &file.downloads, &file.path, cancel).await?
                else {
                    return Err(port::DownloadError::ChecksumMismatch { expected, actual }.into());
                };
                if authoritative != actual {
                    return Err(port::DownloadError::ChecksumMismatch { expected, actual }.into());
                }
                info!(
                    path = %file.path,
                    manifest_sha1 = %expected,
                    upstream_sha1 = %actual,
                    "modpack file hash is stale in manifest; accepted upstream hash"
                );
                let retry = DownloadRequest {
                    location: url,
                    destination: target.to_path_buf(),
                    expected: Some(domain::server::artifact::Checksum::new(
                        domain::server::artifact::ChecksumAlgorithm::Sha1,
                        authoritative,
                    )),
                    expected_size,
                };
                let downloaded = self.downloader.download(&retry, progress, cancel).await?;
                Ok(FetchedFile {
                    bytes: downloaded.size,
                    stale: true,
                })
            }
            Err(err) => Err(err.into()),
        }
    }
}

/// 单文件下载结果
struct FetchedFile {
    /// 本次写入的字节数
    bytes: u64,
    /// 是否因清单哈希过期、按上游权威哈希接受
    stale: bool,
}

/// 清单文件并发下载上限
///
/// 清单文件通常是几十到几百个小 jar：并发太高（几十路）只会放大对 CDN
/// 的压力，收益递减；8 路在常见家庭带宽上已能把吞吐跑满，同时单文件
/// 内部的分片下载仍在 file crate 里按需并发，两层不叠加失控。
const FILE_CONCURRENCY: usize = 8;

/// 并发下载的整包进度聚合
///
/// 每个并发文件各自收到的是「本文件已传字节」（下载器回调口径），
/// 直接画到进度条上就是用户看到的反复归零。聚合层把每个文件的上报
/// 换算成整包口径：`base + 文件内 transferred`，其中 base 是共享计数器
/// 的当前值——同一时刻多个文件各自上报，绝对值取「共享计数 + 自身增量」，
/// 进度单调不回退。
struct AggregateSink<'a> {
    /// 整包已完成字节（含复用与其他并发文件的已落盘部分）
    done: Arc<AtomicU64>,
    /// 上层 sink（CLI 进度条）
    upstream: &'a dyn ProgressSink,
}

impl<'a> AggregateSink<'a> {
    fn new(done: Arc<AtomicU64>, upstream: &'a dyn ProgressSink) -> Self {
        Self { done, upstream }
    }
}

impl ProgressSink for AggregateSink<'_> {
    fn on_progress(&self, transferred: u64, total: Option<u64>) {
        // 文件内 transferred 是本文件从 0 开始的进度；整包口径 =
        // 共享计数（其他文件已计入的部分 + 复用）+ 本文件当前增量
        self.upstream
            .on_progress(self.done.load(Ordering::Relaxed) + transferred, total);
    }
}

/// 已取回并解析的整合包
///
/// 持有暂存路径与清单，供 [`ModpackService::apply`] 布置。
#[derive(Debug)]
pub struct ModpackPrepared {
    /// 来源名（`apply` 据此复用同一来源查询元数据）
    pub source: &'static str,
    /// 来源解析结果
    pub resolved: ResolvedModpack,
    /// 清单
    pub index: ModpackIndex,
    /// 下载到本地的 `.mrpack`
    pub archive_path: PathBuf,
}

impl ModpackPrepared {
    /// 清单要求的 Minecraft 版本
    ///
    /// # Errors
    ///
    /// 清单没有声明时返回 [`ModpackError::MissingMinecraft`]
    pub fn minecraft_version(&self) -> Result<&str, ModpackError> {
        self.index
            .minecraft_version()
            .map_err(|_| ModpackError::MissingMinecraft(self.resolved.project_id.clone()))
    }

    /// 清单对应的服务端变体
    ///
    /// 已知加载器名映射到 [`ServerVariant`]；清单里没有加载器依赖（纯数据包
    /// 整合包，或只有未来才出现的未知键）视为原版——这两种情况都只能用原版
    /// 核心承载，报错反而挡住了能装的情形。
    #[must_use]
    pub fn variant(&self) -> ServerVariant {
        let Some((loader, _version)) = self.index.loader() else {
            return ServerVariant::Vanilla;
        };
        variant_for_loader(loader).unwrap_or(ServerVariant::Vanilla)
    }

    /// 清单要求的加载器版本（用于固定核心来源的选择）
    ///
    /// 与 [`Self::variant`] 不同：这里**不兜底**。清单写了已知加载器但装配层
    /// 没有对应核心来源时，要如实报错而不是退回原版——否则会装出一个与原版
    /// 加载器版本不符、启动必崩的服务端。
    ///
    /// # Errors
    ///
    /// 清单声明的加载器名不在已知集合时返回 [`ModpackError::UnsupportedLoader`]
    pub fn require_loader(&self) -> Result<ServerVariant, ModpackError> {
        match self.index.loader() {
            None => Ok(ServerVariant::Vanilla),
            Some((loader, _version)) => variant_for_loader(loader)
                .ok_or_else(|| ModpackError::UnsupportedLoader(loader.to_owned())),
        }
    }
}

/// 布置结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModpackApplied {
    /// 实际安装的服务端文件数
    pub file_count: usize,
    /// 本次下载的字节数
    pub downloaded_bytes: u64,
    /// 已存在而跳过的文件数
    pub reused: usize,
    /// 按上游元数据判定为仅客户端、已跳过的文件数
    pub skipped_client_only: usize,
    /// 清单哈希已过期、但按上游权威哈希接受的文件路径
    pub stale_hashes: Vec<String>,
}

/// 加载器名 → 服务端变体
///
/// 与 `ServerVariant::content_loader` 是反向映射；未知加载器返回 `None`，
/// 由调用方报「没有对应核心来源」，而不是猜一个变体。
#[must_use]
pub fn variant_for_loader(loader: &str) -> Option<ServerVariant> {
    match loader {
        "fabric" => Some(ServerVariant::Fabric),
        "forge" => Some(ServerVariant::Forge),
        "neoforge" => Some(ServerVariant::NeoForge),
        "quilt" => Some(ServerVariant::Quilt),
        "paper" | "bukkit" => Some(ServerVariant::Paper),
        "vanilla" => Some(ServerVariant::Vanilla),
        _ => None,
    }
}

/// 清单路径 → 服务端目录下的绝对路径，并校验不越界
fn safe_target(server_dir: &Path, relative: &str) -> Result<PathBuf, ModpackError> {
    // 词法归一：`join("../x")` 不会折叠 `..`，必须显式判断「归一后是否仍在
    // 目标目录下」。领域层已在清单解析时拒绝 `..`，这里是针对调用方传入的
    // 任意路径的第二道保险。
    let mut normalized = PathBuf::new();
    for component in Path::new(relative).components() {
        use std::path::Component;
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ModpackError::Escapes {
                    dir: server_dir.to_path_buf(),
                    target: server_dir.join(relative),
                });
            }
        }
    }
    let target = server_dir.join(normalized);
    // 领域层已在 `ModpackIndex::new` 校验过路径，这里是第二道保险：防未来
    // 改动绕过它，或 `join` 在极端输入下的归一化差异
    if !target.starts_with(server_dir) {
        return Err(ModpackError::Escapes {
            dir: server_dir.to_path_buf(),
            target,
        });
    }
    Ok(target)
}

/// 已有文件是否与清单一致（大小相同即认为已就位）
///
/// 清单常在几百个文件量级，逐个重算哈希会把安装时间拉长数倍；大小匹配是
/// 足够强的信号——真正的完整性校验由下载器在**新下载**时完成。
async fn existing_matches(path: &Path, file: &ModpackFile) -> bool {
    match tokio::fs::metadata(path).await {
        Ok(meta) if file.size > 0 => meta.len() == file.size,
        _ => false,
    }
}

/// 回源取某文件的当前权威 sha1
///
/// 清单哈希过期时调用：用下载地址 + 文件名作为线索向来源查询上游**当前**
/// 哈希。返回 `None` 表示来源不支持、或查不到该文件，此时调用方应退回原来的
/// `ChecksumMismatch`，不做任何放宽。
///
/// 查询本身失败（网络 / 上游异常）不视为硬错误：这只是一次尽力而为的核对，
/// 失败时同样返回 `None`，让原错误如实抛出。
async fn authoritative_sha1(
    source: &dyn ModpackSource,
    downloads: &[String],
    path: &str,
    _cancel: &CancellationToken,
) -> Result<Option<String>, ModpackError> {
    let Some(url) = downloads.first() else {
        return Ok(None);
    };
    let filename = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
        .to_owned();
    let hints = [(url.clone(), filename.clone())];
    match source.authoritative_hashes(&hints).await {
        Ok(mut map) => Ok(map.remove(&filename)),
        Err(_) => Ok(None),
    }
}

/// 取第一个 `https` 下载地址
///
/// 上游理论上都是 https；把非 https 当可下载目标会造成明文降级，因此直接拒绝。
fn first_https(downloads: &[String]) -> Result<url::Url, ModpackError> {
    let raw = downloads.first().ok_or_else(|| {
        ModpackError::Source(SourceError::Decode(Box::new(std::io::Error::other(
            "modpack file has no download url",
        ))))
    })?;
    let url = raw
        .parse::<url::Url>()
        .map_err(|err| ModpackError::Source(SourceError::Decode(Box::new(err))))?;
    if url.scheme() != "https" {
        return Err(ModpackError::Source(SourceError::Decode(Box::new(
            std::io::Error::other(format!("modpack file is not served over https: {raw}")),
        ))));
    }
    Ok(url)
}

/// 供装配层判断：某变体是否由整合包承载
#[must_use]
pub fn is_modpack_variant(variant: &ServerVariant) -> bool {
    matches!(variant, ServerVariant::ModrinthModpack)
}

/// 从清单的加载器版本构造 `ServerJarSource` 的版本参数
///
/// `ServerJarSource::artifact` 只接受 MC 版本；加载器版本由来源的 `Picks`
/// 决定。整合包要求固定加载器版本时，装配层据此构造带 `Fixed` 的来源实例。
#[must_use]
pub fn loader_pin(index: &ModpackIndex) -> Option<(&'static str, String)> {
    index
        .loader()
        .map(|(loader, version)| (loader, version.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::server::modpack::{ModpackDependencies, ModpackFile, ModpackHashes};

    #[test]
    fn variant_for_loader_maps_known_loaders() {
        assert_eq!(variant_for_loader("fabric"), Some(ServerVariant::Fabric));
        assert_eq!(variant_for_loader("forge"), Some(ServerVariant::Forge));
        assert_eq!(
            variant_for_loader("neoforge"),
            Some(ServerVariant::NeoForge)
        );
        assert_eq!(variant_for_loader("quilt"), Some(ServerVariant::Quilt));
        assert_eq!(variant_for_loader("paper"), Some(ServerVariant::Paper));
        assert_eq!(variant_for_loader("vanilla"), Some(ServerVariant::Vanilla));
        assert_eq!(variant_for_loader("unknown-loader"), None);
    }

    #[test]
    fn is_modpack_variant_matches_only_modrinth() {
        assert!(is_modpack_variant(&ServerVariant::ModrinthModpack));
        assert!(!is_modpack_variant(&ServerVariant::Fabric));
    }

    #[test]
    fn safe_target_rejects_escape() {
        let dir = Path::new("/srv/mc");
        assert_eq!(
            safe_target(dir, "mods/a.jar").expect("ok"),
            Path::new("/srv/mc/mods/a.jar")
        );
        // 领域层已拦，但这里必须再兜一次
        let escaped = safe_target(dir, "../evil.jar");
        // `join` 后不归一化 ../，但 starts_with 仍能发现它不在 dir 下
        assert!(escaped.is_err(), "应拒绝越界: {escaped:?}");
    }

    #[test]
    fn first_https_requires_https() {
        let ok = first_https(&["https://cdn/x.jar".to_owned()]).expect("https");
        assert_eq!(ok.scheme(), "https");
        assert!(first_https(&["http://cdn/x.jar".to_owned()]).is_err());
        assert!(first_https(&[]).is_err());
    }

    fn index_with(loader: Option<(&str, &str)>, mc: Option<&str>) -> ModpackIndex {
        let mut deps = std::collections::BTreeMap::new();
        if let Some(mc) = mc {
            deps.insert("minecraft".to_owned(), mc.to_owned());
        }
        if let Some((key, version)) = loader {
            deps.insert(key.to_owned(), version.to_owned());
        }
        ModpackIndex::new(
            1,
            "minecraft",
            "1.0.0",
            "Pack",
            ModpackDependencies { raw: deps },
            Vec::new(),
        )
        .expect("index")
    }

    fn prepared(index: ModpackIndex) -> ModpackPrepared {
        ModpackPrepared {
            source: "fake",
            resolved: ResolvedModpack {
                project_id: "P".to_owned(),
                version_id: "V".to_owned(),
                version_number: "1.0.0".to_owned(),
                file: port::content::ContentFile {
                    location: "https://cdn/x.mrpack".parse().expect("url"),
                    filename: "x.mrpack".to_owned(),
                    size: 1,
                    checksum: None,
                },
            },
            index,
            archive_path: PathBuf::from("/tmp/x.mrpack"),
        }
    }

    #[test]
    fn prepared_reports_minecraft_and_variant() {
        let p = prepared(index_with(
            Some(("fabric-loader", "0.16.0")),
            Some("1.21.1"),
        ));
        assert_eq!(p.minecraft_version().expect("mc"), "1.21.1");
        assert_eq!(p.variant(), ServerVariant::Fabric);
    }

    #[test]
    fn prepared_without_loader_is_vanilla() {
        let p = prepared(index_with(None, Some("1.21.1")));
        assert_eq!(p.variant(), ServerVariant::Vanilla);
    }

    #[test]
    fn prepared_without_minecraft_is_error() {
        let p = prepared(index_with(Some(("fabric-loader", "0.16.0")), None));
        assert!(matches!(
            p.minecraft_version(),
            Err(ModpackError::MissingMinecraft(_))
        ));
    }

    /// `variant()` 对未知加载器兜底为原版（能装就不挡）
    #[test]
    fn prepared_with_unknown_loader_falls_back_to_vanilla() {
        let p = prepared(index_with(Some(("future-loader", "1.0")), Some("1.21.1")));
        assert_eq!(p.variant(), ServerVariant::Vanilla);
    }

    /// 清单里的未知依赖键不会被当作加载器（领域层只认已知键），
    /// 因此 `require_loader` 也不会误报「不支持的加载器」
    #[test]
    fn unknown_dependency_key_is_not_a_loader() {
        let p = prepared(index_with(Some(("future-loader", "1.0")), Some("1.21.1")));
        assert!(matches!(p.require_loader(), Ok(ServerVariant::Vanilla)));
        assert_eq!(loader_pin(&p.index), None);
    }

    /// `require_loader` 对已知加载器给出对应变体
    #[test]
    fn require_loader_maps_known_loader() {
        let p = prepared(index_with(
            Some(("fabric-loader", "0.16.0")),
            Some("1.21.1"),
        ));
        assert!(matches!(p.require_loader(), Ok(ServerVariant::Fabric)));
    }

    #[test]
    fn loader_pin_returns_version() {
        let index = index_with(Some(("fabric-loader", "0.16.0")), Some("1.21.1"));
        assert_eq!(loader_pin(&index), Some(("fabric", "0.16.0".to_owned())));
        let vanilla = index_with(None, Some("1.21.1"));
        assert_eq!(loader_pin(&vanilla), None);
    }

    #[test]
    fn unsafe_download_url_is_rejected() {
        let file = ModpackFile {
            path: "mods/a.jar".to_owned(),
            downloads: vec!["http://cdn/a.jar".to_owned()],
            size: 1,
            hashes: ModpackHashes::default(),
            env: None,
        };
        assert!(first_https(&file.downloads).is_err());
    }
}
