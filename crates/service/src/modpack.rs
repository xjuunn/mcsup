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
use port::modpack::{
    ModpackExtractError, ModpackExtractor, ModpackSource, ModpackSourceRegistry, ResolvedModpack,
};
use port::{ArtifactDownloader, CancellationToken, DownloadRequest, ProgressSink, SourceError};
use std::path::{Path, PathBuf};
use std::sync::Arc;
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

        // 2. 清单文件：只装服务端需要的（server: unsupported 已由领域层过滤）
        let mut reused = 0;
        let mut downloaded_bytes = 0;
        for file in prepared.index.downloadable_server_files() {
            let target = safe_target(server_dir, &file.path)?;
            if let Some(parent) = target.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|err| ModpackError::io(parent, err))?;
            }
            if existing_matches(&target, file).await {
                reused += 1;
                continue;
            }
            let url = first_https(&file.downloads)?;
            let request = DownloadRequest {
                location: url,
                destination: target,
                expected: file.hashes.sha1_checksum(),
                expected_size: (file.size > 0).then_some(file.size),
            };
            let downloaded = self.downloader.download(&request, progress, cancel).await?;
            downloaded_bytes += downloaded.size;
        }

        info!(
            project = %prepared.resolved.project_id,
            version = %prepared.resolved.version_id,
            files = prepared.index.server_files().len(),
            downloaded_bytes,
            reused,
            "modpack applied"
        );
        Ok(ModpackApplied {
            file_count: prepared.index.server_files().len(),
            downloaded_bytes,
            reused,
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
}

/// 已取回并解析的整合包
///
/// 持有暂存路径与清单，供 [`ModpackService::apply`] 布置。
#[derive(Debug)]
pub struct ModpackPrepared {
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModpackApplied {
    /// 服务端需要的文件数
    pub file_count: usize,
    /// 本次下载的字节数
    pub downloaded_bytes: u64,
    /// 已存在而跳过的文件数
    pub reused: usize,
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
