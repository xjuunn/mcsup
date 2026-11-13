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
use crate::mod_inspect::ModBlocked;
use crate::modpack_manifest::ModpackManifestSnapshot;
use domain::mod_::{ModLoader, ServerTarget};
use domain::server::modpack::{ModpackFile, ModpackIndex};
use domain::server::version::ServerVariant;
use port::mod_::ModInspector;
use port::modpack::{
    ModpackExtractError, ModpackExtractor, ModpackSource, ModpackSourceRegistry,
    PackMemberVerdicts, ResolvedModpack,
};
use port::{ArtifactDownloader, CancellationToken, DownloadRequest, ProgressSink, SourceError};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use tracing::{info, warn};
use url::Url;

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
    /// 已取回的本地归档；给出时不再联网下载
    ///
    /// `server create` 分两步：先取回清单拿 MC 版本（要写注册表），落库后再
    /// 布置。默认做法是两步各取回一次，等于把 `.mrpack` 下两遍——国际链路上
    /// 一个 20 MB 的包就是几分钟。把第一步取到的归档路径带过来，第二步直接
    /// 复用本地文件，只多读一次 zip 索引（毫秒级）。
    ///
    /// 文件不存在时回退到正常下载：暂存目录可能已被清理，静默成功会让调用方
    /// 拿到一个不存在的归档。
    pub archive: Option<&'a Path>,
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
            archive: None,
        }
    }

    /// 复用已取回的本地归档
    #[must_use]
    pub fn archive(mut self, archive: &'a Path) -> Self {
        self.archive = Some(archive);
        self
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
    /// 落盘后的本地检测器；`None` 表示不做装完复核
    ///
    /// 有它才能在装完时按**文件本身**判一次：清单的 `env` 与上游的
    /// `server_side` 都是作者的声明，声明错的时候只有读 jar 才能发现。
    /// 见 [`Self::audit_mods`]。
    inspector: Option<Arc<dyn ModInspector>>,
    /// 批量下载的并发文件数（见 [`Self::with_file_concurrency`]）
    file_concurrency: usize,
    /// mod 文件的镜像前缀，空表示只用清单给的地址
    mirrors: Vec<Url>,
}

impl std::fmt::Debug for ModpackService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModpackService")
            .field("sources", &self.sources.names())
            .field("file_concurrency", &self.file_concurrency)
            .field("mirrors", &self.mirrors.len())
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
            inspector: None,
            file_concurrency: FILE_CONCURRENCY,
            mirrors: Vec::new(),
        }
    }

    /// 指定落盘后的本地检测器
    ///
    /// 装上它之后，[`Self::apply`] 会在下载完成后按**文件本身**再判一次，
    /// 把服务端加载器明确不接受的 jar 移进回收站。整合包的 `env` 字段与上游
    /// 的 `server_side` 都是作者声明，声明与实际不符时只有读 jar 才能发现；
    /// 不装则只按声明装，行为与旧版本一致。
    #[must_use]
    pub fn with_inspector(mut self, inspector: Arc<dyn ModInspector>) -> Self {
        self.inspector = Some(inspector);
        self
    }

    /// 指定批量下载的并发文件数
    ///
    /// `0` 视为「用实现内置缺省」，与 [`ArtifactDownloader::download_many`] 的
    /// 契约一致；配置层已经在键表校验里挡掉了 0，这里只是不让下游拿到一个
    /// 会让整批退化成串行的值。
    #[must_use]
    pub fn with_file_concurrency(mut self, concurrency: usize) -> Self {
        if concurrency > 0 {
            self.file_concurrency = concurrency;
        }
        self
    }

    /// 指定 mod 文件的镜像前缀
    ///
    /// 前缀只替换地址的 scheme 与 host，路径原样保留：镜像站按上游同样的
    /// 路径规则回源（`data/<项目>/versions/<版本>/<文件名>`），存整条地址就得
    /// 为一个整合包写几百行配置，上游一换文件名全作废。
    #[must_use]
    pub fn with_mirrors(mut self, mirrors: Vec<Url>) -> Self {
        self.mirrors = mirrors;
        self
    }

    /// 为一个上游地址生成镜像候选
    ///
    /// 只对清单实际给出的地址做前缀替换；镜像前缀本身不带路径部分以外的
    /// 语义，因此拼接是安全的。校验和与大小对全部候选成立，镜像给坏字节只会
    /// 换来一次 `ChecksumMismatch`，不会污染落盘文件。
    fn mirrors_for(&self, location: &Url) -> Vec<Url> {
        if self.mirrors.is_empty() {
            return Vec::new();
        }
        let path = location.path().trim_start_matches('/');
        self.mirrors
            .iter()
            .filter_map(|base| {
                let mut base = base.clone();
                // 前缀是**目录**：`/prefix` 与 `/prefix/` 必须等价。少了这一补，
                // `join` 会把最后一段当文件名替换掉，`https://m.example/prefix`
                // 会拼成 `https://m.example/data/...`，静默指到镜像站的根。
                if !base.path().ends_with('/') {
                    base.set_path(&format!("{}/", base.path()));
                }
                base.join(path).ok()
            })
            .filter(|candidate| candidate != location)
            .collect()
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
        // 已取回的归档直接复用：`server create` 在选包阶段已经把 `.mrpack`
        // 下到本地，再下一遍在国际链路上是分钟级浪费。文件不在时才回退到
        // 下载——暂存目录可能已被清理，静默成功会让 `apply` 拿到不存在的归档。
        let archive_path = match options.archive {
            Some(path) if path.is_file() => path.to_path_buf(),
            _ => {
                let staging = options.server_dir.join("modpack-staging");
                tokio::fs::create_dir_all(&staging)
                    .await
                    .map_err(|err| ModpackError::io(&staging, err))?;
                let archive_path = staging.join(&resolved.file.filename);
                self.download_file(&resolved.file, &archive_path, options.progress, cancel)
                    .await?;
                archive_path
            }
        };

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
        //
        //    但不能只看「上游说它仅客户端」：库类项目（`satin` / `melody`
        //    一类提供 API 的）常被标成 `unsupported`，实际是别的 mod 起不来的
        //    硬前提。判定因而收敛到 `PackMemberVerdicts::skips`——仅客户端
        //    **且** 无成员依赖才跳过。
        let source = self.select_source(Some(prepared.source))?;
        let mod_hashes: Vec<String> = prepared
            .index
            .downloadable_server_files()
            .into_iter()
            .filter(|file| file.path.starts_with("mods/"))
            .filter(|file| file.is_plugin_jar())
            .filter_map(|file| file.hashes.sha1.clone())
            .collect();
        // 纯配置 / 资源包整合包没有可反查的 jar，直接省掉一次上游请求
        let verdicts = if mod_hashes.is_empty() {
            PackMemberVerdicts::default()
        } else {
            source.pack_member_verdicts(&mod_hashes).await?
        };

        // 3. 清单文件：只装服务端需要的（server: unsupported 已由领域层过滤），
        //    并行下载，进度收敛为整包口径（见 [`Self::download_files`]）
        let (jobs, skipped_client_only) =
            self.classify_files(prepared, server_dir, &verdicts).await?;
        let mut applied = self
            .download_files(prepared, source.as_ref(), &jobs, progress, cancel)
            .await?;

        // 4. 落盘后按文件本身复核一次
        //
        //    前三步信的都是「别人怎么说」：清单的 env、上游的 server_side。
        //    整合包作者把仅客户端模组标成 `server: required` 是常见情况
        //    （实测 Sodium / ETF / malilib 一类都在清单里声明 required），
        //    上游标注也可能迟到或写错；依赖齐不齐、加载器对不对得上，更是
        //    只有把整批文件摆在一起才判得出来。这一步与 `server start` 的
        //    预检共用同一个扫描，因此这里报空就等于启动不会被 mod 拦住。
        applied.blocked = self.audit_mods(prepared, server_dir).await;
        applied.file_count = prepared
            .index
            .server_files()
            .len()
            .saturating_sub(skipped_client_only.len());
        applied.skipped_client_only = skipped_client_only.len();
        applied.skipped_client_only_files = skipped_client_only;

        // 5. 把清单留一份：项目 / 版本 ID 都在下载地址里，装完就有了权威数据，
        //    后续补依赖与整合包更新按 ID 直接定位，不必拿 modid 去搜索
        let snapshot = ModpackManifestSnapshot::from_index(
            prepared.source,
            &prepared.resolved.project_id,
            &prepared.resolved.version_id,
            &prepared.resolved.version_number,
            &prepared.index,
            crate::atomic::unix_now(),
        );
        snapshot
            .save(server_dir)
            .await
            .map_err(|err| ModpackError::io(&ModpackManifestSnapshot::path_in(server_dir), err))?;
        Ok(applied)
    }

    /// 装完的本地复核：列出会让服务端起不来的条目
    ///
    /// 前面几步信的都是「别人怎么说」——清单的 `env`、上游项目的
    /// `server_side`。声明与实际不符是常态：作者把仅客户端模组标成
    /// `server: required`、上游 `server_side` 写错、整合包里混进别的加载器
    /// 的 jar、依赖没有一并打包。这些在装完的那一刻全都查得出来，等到
    /// `server start` 再拦，用户已经以为装好了。
    ///
    /// 扫描范围是整台服务端的 `mods/` 与 `plugins/`，不是本次下载的那批：
    /// 依赖闭包要拿全量文件才算得准，`overrides/` 铺进来的 jar 同样是加载器
    /// 会读的。判定本身复用 [`ModInspectService::blockers_in_dir`]，与
    /// `server start` 的预检是同一份代码——两处各判一套迟早分叉成「装完说
    /// 没事、启动却被拦下」。
    ///
    /// 这里**不动任何文件**：怎么处置由用户决定，入口负责问（见
    /// `mcsup server mod` 的停用流程）。工具单方面移走文件，用户就只能从
    /// 结果反推发生了什么。
    ///
    /// 没有装检测器时直接返回空：不做判定比按猜测处置安全。
    async fn audit_mods(&self, prepared: &ModpackPrepared, server_dir: &Path) -> Vec<ModBlocked> {
        let Some(inspector) = self.inspector.clone() else {
            return Vec::new();
        };
        let target = ServerTarget {
            loader: prepared
                .index
                .loader()
                .and_then(|(name, _version)| ModLoader::parse(name)),
            mc_version: prepared.index.dependencies.minecraft().map(str::to_owned),
            java_major: None,
            bridged_loaders: Vec::new(),
        };
        let dir = server_dir.to_path_buf();
        // 读 jar 是同步 IO：放进阻塞线程池，别占住异步 executor
        let scanned = tokio::task::spawn_blocking(move || {
            crate::mod_inspect::ModInspectService::local(inspector).blockers_in_dir(&dir, &target)
        })
        .await;
        match scanned {
            Ok(Ok(blocked)) => {
                if !blocked.is_empty() {
                    info!(
                        blocked = blocked.len(),
                        "installed pack has mods that would stop the server"
                    );
                }
                blocked
            }
            Ok(Err(err)) => {
                // 扫不动不是致命错误：文件已经装好，启动预检会再拦一次
                warn!(error = %err, "post-install mod audit failed; skipping");
                Vec::new()
            }
            Err(err) => {
                warn!(error = %err, "post-install mod audit did not finish");
                Vec::new()
            }
        }
    }

    /// 把清单文件分类：要下载的、仅客户端剔除的
    ///
    /// 仅客户端文件顺手清掉旧落盘——重装时留着只会让服务端起不来。
    /// 返回被剔除文件的清单路径，供入口回显「跳过的是哪几个」，而不是只报数量。
    async fn classify_files<'a>(
        &self,
        prepared: &'a ModpackPrepared,
        server_dir: &Path,
        verdicts: &PackMemberVerdicts,
    ) -> Result<(Vec<(&'a ModpackFile, PathBuf)>, Vec<String>), ModpackError> {
        let mut skipped_client_only: Vec<String> = Vec::new();
        let mut jobs: Vec<(&'a ModpackFile, PathBuf)> = Vec::new();
        for file in prepared.index.downloadable_server_files() {
            let target = safe_target(server_dir, &file.path)?;
            if file
                .hashes
                .sha1
                .as_deref()
                .is_some_and(|hash| verdicts.skips(hash))
            {
                // 上一次安装可能已经把它落盘；重装时顺手清掉，避免旧文件
                // 继续让服务端起不来
                if tokio::fs::try_exists(&target).await.unwrap_or(false) {
                    tokio::fs::remove_file(&target)
                        .await
                        .map_err(|err| ModpackError::io(&target, err))?;
                }
                skipped_client_only.push(file.path.clone());
                continue;
            }
            jobs.push((file, target));
        }
        Ok((jobs, skipped_client_only))
    }

    /// 并行下载清单文件，进度收敛为整包口径
    ///
    /// 下载本身交给 [`ArtifactDownloader::download_many`]：并发度、整包进度聚合、
    /// 瞬态重试都是下载器的职责（见 file crate 的实现），service 只做业务编排——
    /// 判定复用、决定每个文件的目标路径，以及哈希过期后的回源修正。
    ///
    /// 两段式：先按清单哈希整批并发下载（绝大多数文件这一轮就完成），只有报
    /// `ChecksumMismatch` 的少数文件再走 [`Self::fetch_one_file`] 的权威哈希
    /// 回源路径。这样快车道不被慢逻辑拖累，业务语义（清单哈希过期可修正）不变。
    ///
    /// 第三段兜的是**瞬时传输失败**：批量阶段某个文件连接被重置（跨境 CDN 在
    /// 大量并发握手时很常见），不代表这个文件下不到，更不代表整包报废。这些
    /// 文件在批量结束后以并发 1 逐个重下——并发正是失败的原因，降并发才是对治。
    /// 重下仍失败才报错；绝不静默跳过，缺文件的整合包启动即崩。
    ///
    /// 进度口径收敛为整包：清单里每个文件都带 size，总字节可知；已复用
    /// （reused）的文件按声明 size 直接计入已完成，余下的由下载器累加。
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

        // 大文件优先（LPT 调度）：按体积降序，让大文件先启动、占满批量并发和
        // 最多的墙钟时间，小文件补尾部。否则按清单原始顺序下载时，大文件若
        // 排在末尾会拖着尾段、在途连接数一路衰减，观感就是「越下越慢」；这个
        // 顺序同时最小化整批的完成时间（最长处理时间优先）。
        to_download.sort_unstable_by_key(|(file, _)| std::cmp::Reverse(file.size));

        // 复用部分的字节先计入：进度条的起点就是「跳过的那一大半」
        let settled = ReusedProgress::new(reused_bytes, total_bytes, progress);

        // 第一段：按清单哈希整批并发下载
        let mut requests = Vec::with_capacity(to_download.len());
        let mut index: Vec<usize> = Vec::with_capacity(to_download.len());
        for (position, (file, target)) in to_download.iter().enumerate() {
            let url = first_https(&file.downloads)?;
            let mirrors = self.mirrors_for(&url);
            requests.push(DownloadRequest {
                location: url,
                destination: target.clone(),
                expected: file.hashes.sha1_checksum(),
                expected_size: (file.size > 0).then_some(file.size),
                mirrors,
            });
            index.push(position);
        }
        let outcomes = self
            .downloader
            .download_many(
                &requests,
                self.file_concurrency,
                Some(&settled.sink()),
                cancel,
            )
            .await;

        let mut downloaded_bytes = 0_u64;
        // 清单哈希过期、但上游权威哈希与下载字节一致的文件路径
        let mut stale_hashes: Vec<String> = Vec::new();
        // (位置, 清单声明的哈希, 实际下载字节的哈希)
        let mut needs_authority: Vec<(usize, String, String)> = Vec::new();
        // 批量阶段传输失败、留到第三段低并发重下的文件位置
        let mut needs_retry: Vec<usize> = Vec::new();
        for (position, outcome) in index.iter().copied().zip(outcomes) {
            match outcome {
                Ok(downloaded) => {
                    downloaded_bytes = downloaded_bytes.saturating_add(downloaded.size);
                }
                Err(port::DownloadError::ChecksumMismatch { expected, actual }) => {
                    needs_authority.push((position, expected, actual));
                }
                // 取消 / 校验失败 / 本地 IO 重试也不会变好，按原错误立即返回
                Err(err) if !err.retryable() => return Err(err.into()),
                Err(err) => {
                    warn!(
                        path = %to_download[position].0.path,
                        error = %err,
                        "modpack file failed in the concurrent phase; queued for serial retry"
                    );
                    needs_retry.push(position);
                }
            }
        }

        // 第二段：只有清单哈希过期的文件才回源确认 + 按权威哈希重下。
        // 带着批量下载已取得的 expected/actual 进入，不重复注定失败的尝试。
        //
        // 这一段的下载不再上报进度：这些文件的字节在批量阶段已按清单大小计入，
        // 重下再按单文件从 0 报会把整包位置拽回去（观感是数字倒退、速度归零）。
        // 发生概率低、单次通常只涉及一两个文件，进度条停在原处比倒退诚实。
        let (authority_bytes, stale_paths) = self
            .retry_authority(source, &to_download, &needs_authority, cancel)
            .await?;
        downloaded_bytes = downloaded_bytes.saturating_add(authority_bytes);
        stale_hashes.extend(stale_paths);

        // 第三段：批量阶段的瞬时失败按并发 1 重下。
        let retried_bytes = self
            .retry_serially(&to_download, &needs_retry, cancel)
            .await?;
        downloaded_bytes = downloaded_bytes.saturating_add(retried_bytes);

        // 收尾把进度落到整包总量。批量阶段最后一次上报可能停在最后一个
        // 文件最后一个 chunk 之前，而此后的校验 / 重命名、以及 `apply` 里的
        // 模组判定都不再产生进度事件 —— 不补这一下，回显会长期停在
        // `934.90 MB/934.91 MB`，与随后的阶段切换之间留下一段假死。
        // 到这里所有清单文件都已落盘（重下失败会提前返回 Err），补满不虚报。
        if let Some(sink) = progress {
            sink.on_progress(total_bytes, Some(total_bytes));
        }

        info!(
            project = %prepared.resolved.project_id,
            version = %prepared.resolved.version_id,
            files = prepared.index.server_files().len(),
            downloaded_bytes,
            reused,
            stale_hashes = stale_hashes.len(),
            retried = needs_retry.len(),
            "modpack files downloaded"
        );
        Ok(ModpackApplied {
            file_count: 0,
            downloaded_bytes,
            reused,
            skipped_client_only: 0,
            skipped_client_only_files: Vec::new(),
            skipped_bytes: 0,
            stale_hashes,
            // 下载阶段还不知道文件本身是什么，装完判定在 `apply` 里补
            blocked: Vec::new(),
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
        let mirrors = self.mirrors_for(&file.location);
        let request = DownloadRequest {
            location: file.location.clone(),
            destination: destination.to_path_buf(),
            expected: file.checksum.clone(),
            expected_size: (file.size > 0).then_some(file.size),
            mirrors,
        };
        self.downloader.download(&request, progress, cancel).await?;
        Ok(())
    }

    /// 清单哈希已过期：回源确认并改用权威哈希重下
    ///
    /// 单独成函数是为了让批量下载的第二阶段复用它：批量下载已经用清单哈希试过
    /// 一次并拿到 `ChecksumMismatch`，此时**不该**再重复一遍注定失败的尝试——
    /// 直接带着已知的 `expected`/`actual` 进入回源确认。
    ///
    /// 判定标准：上游权威哈希必须与刚下载的字节一致，才认为「文件本身没坏、
    /// 只是清单过期了」；不一致说明下载到的字节确实不对，原样抛出 mismatch。
    #[allow(clippy::too_many_arguments)]
    async fn accept_authoritative_hash(
        &self,
        source: &dyn ModpackSource,
        file: &ModpackFile,
        url: Url,
        target: &Path,
        expected: String,
        actual: String,
        cancel: &CancellationToken,
    ) -> Result<FetchedFile, ModpackError> {
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
        let mirrors = self.mirrors_for(&url);
        let retry = DownloadRequest {
            location: url,
            destination: target.to_path_buf(),
            expected: Some(domain::server::artifact::Checksum::new(
                domain::server::artifact::ChecksumAlgorithm::Sha1,
                authoritative,
            )),
            expected_size: (file.size > 0).then_some(file.size),
            mirrors,
        };
        let downloaded = self.downloader.download(&retry, None, cancel).await?;
        Ok(FetchedFile {
            bytes: downloaded.size,
            stale: true,
        })
    }

    /// 只有清单哈希过期的文件才回源确认 + 按权威哈希重下。
    ///
    /// 这一段的下载不再上报进度：这些文件的字节在批量阶段已按清单大小计入，重下
    /// 再按单文件从 0 报会把整包位置拽回去。发生概率低、单次通常只涉及一两个
    /// 文件，进度条停在原处比倒退诚实。返回实际写入的字节数与清单哈希过期的路径。
    async fn retry_authority(
        &self,
        source: &dyn ModpackSource,
        to_download: &[(&ModpackFile, PathBuf)],
        needs_authority: &[(usize, String, String)],
        cancel: &CancellationToken,
    ) -> Result<(u64, Vec<String>), ModpackError> {
        let mut bytes = 0_u64;
        let mut stale_paths = Vec::new();
        for (position, expected, actual) in needs_authority {
            if cancel.is_cancelled() {
                return Err(ModpackError::Download(port::DownloadError::Cancelled));
            }
            let (file, target) = &to_download[*position];
            let url = first_https(&file.downloads)?;
            let fetched = self
                .accept_authoritative_hash(
                    source,
                    file,
                    url,
                    target,
                    expected.clone(),
                    actual.clone(),
                    cancel,
                )
                .await?;
            bytes = bytes.saturating_add(fetched.bytes);
            if fetched.stale {
                stale_paths.push(file.path.clone());
            }
        }
        Ok((bytes, stale_paths))
    }

    /// 批量阶段的瞬时失败按并发 1 重下，返回实际写入的字节数。
    ///
    /// 这些文件已在上层的单文件重试与候选源回退中失败过，继续用同样的并发只会
    /// 重复同一个结果；把同时在途的连接降到 1 才能避开 CDN 对握手风暴的限流。
    /// 落到这里的只有 `retryable()` 的失败，重下仍失败就是真实故障，原样返回
    /// ——不跳过任何一个清单文件。
    async fn retry_serially(
        &self,
        to_download: &[(&ModpackFile, PathBuf)],
        needs_retry: &[usize],
        cancel: &CancellationToken,
    ) -> Result<u64, ModpackError> {
        let mut bytes = 0_u64;
        for position in needs_retry {
            if cancel.is_cancelled() {
                return Err(ModpackError::Download(port::DownloadError::Cancelled));
            }
            let (file, target) = &to_download[*position];
            let url = first_https(&file.downloads)?;
            let mirrors = self.mirrors_for(&url);
            let request = DownloadRequest {
                location: url,
                destination: target.clone(),
                expected: file.hashes.sha1_checksum(),
                expected_size: (file.size > 0).then_some(file.size),
                mirrors,
            };
            // `download` 自带单文件瞬态重试与候选源回退，这里只需压低并发：
            // 逐个调用保证同一时刻只有一个文件在途。
            let downloaded = self.downloader.download(&request, None, cancel).await?;
            bytes = bytes.saturating_add(downloaded.size);
            info!(
                path = %file.path,
                "modpack file recovered by serial retry"
            );
        }
        Ok(bytes)
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
/// 清单里小文件占多数，低于分段阈值的文件只能单流，它们的吞吐由这个并发数
/// 决定；单连接约 45 KiB/s 的限速下，几十路并发是必要而非浪费。24 路在实测里
/// 与更高取值吞吐持平（详见 `docs/concurrency-performance.md`），再高只放大对
/// CDN 的连接压力。此处是 service 侧缺省，实际值由平台配置
/// （`download-concurrency`）经 [`Self::with_file_concurrency`] 覆盖。
const FILE_CONCURRENCY: usize = 24;

/// 复用文件的进度起点
///
/// 重装整合包时大部分文件已存在且校验一致，它们不会经过下载器，下载器也就
/// 不知道这部分字节。把复用字节作为**偏移量**叠加到下载器的整批进度上，
/// 进度条才从「实际已完成」的位置开始走，而不是从 0 重新爬。
///
/// 偏移只在 service 侧持有：下载器仍然只报告它自己传了多少，语义不被污染；
/// 上层拿到的仍是整包绝对值。
struct ReusedProgress<'a> {
    /// 复用文件贡献的字节
    offset: u64,
    /// 整包总字节
    total: u64,
    /// 上层 sink
    upstream: Option<&'a dyn ProgressSink>,
}

impl<'a> ReusedProgress<'a> {
    fn new(offset: u64, total: u64, upstream: Option<&'a dyn ProgressSink>) -> Self {
        Self {
            offset,
            total,
            upstream,
        }
    }

    /// 叠加了复用偏移的 sink
    fn sink(&self) -> OffsetSink<'_> {
        OffsetSink {
            offset: self.offset,
            total: self.total,
            upstream: self.upstream,
        }
    }
}

/// 把下载器的整批进度叠加复用偏移后上报
struct OffsetSink<'a> {
    offset: u64,
    total: u64,
    upstream: Option<&'a dyn ProgressSink>,
}

impl ProgressSink for OffsetSink<'_> {
    fn on_progress(&self, transferred: u64, _total: Option<u64>) {
        if let Some(sink) = self.upstream {
            // 偏移量与传来的增量相加即为整包已完成字节；总长用清单字节总和。
            // 以总长封顶：清单里个别文件未声明大小时（不参与批量 total 的拼接），
            // 它们的字节仍会累加进来，封顶保证进度条不显示超过总量的数字。
            let done = self.offset.saturating_add(transferred).min(self.total);
            sink.on_progress(done, Some(self.total));
        }
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
    /// 被跳过文件的清单路径（如 `mods/sodium.jar`），与 [`Self::skipped_client_only`]
    /// 数量一一对应，供入口回显列出「跳过的是哪几个」
    pub skipped_client_only_files: Vec<String>,
    /// 因服务端不读取而未下载的字节数（资源包 / 光影 / `*.disabled`）
    ///
    /// 与 `downloaded_bytes` 是两个口径：这些字节既没下也没装，省下来的就是
    /// 它。回显里单列，用户才知道「为什么这次快了几个数量级」。
    pub skipped_bytes: u64,
    /// 清单哈希已过期、但按上游权威哈希接受的文件路径
    pub stale_hashes: Vec<String>,
    /// 装完复核出的、会让服务端起不来的条目
    ///
    /// 与 `skipped_client_only` 是两个口径：那一个按**声明**跳过、从未下载；
    /// 这一个已经下了盘，读 jar 才发现装载不了或依赖不齐。判定口径与
    /// `server start` 的预检完全一致（同一个
    /// [`crate::ModInspectService::blockers_in_dir`]），因此这里为空就等于
    /// 「启动不会被 mod 拦住」。
    ///
    /// 安装本身不动这些文件：怎么处置是用户的决定，入口负责问。
    pub blocked: Vec<ModBlocked>,
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
fn first_https(downloads: &[String]) -> Result<Url, ModpackError> {
    let raw = downloads.first().ok_or_else(|| {
        ModpackError::Source(SourceError::Decode(Box::new(std::io::Error::other(
            "modpack file has no download url",
        ))))
    })?;
    let url = raw
        .parse::<Url>()
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

    /// 只为拿到一个能调私有方法的实例：镜像与并发断言不碰网络与磁盘
    struct NoopDownloader;

    #[async_trait::async_trait]
    impl ArtifactDownloader for NoopDownloader {
        async fn download(
            &self,
            _request: &DownloadRequest,
            _progress: Option<&dyn ProgressSink>,
            _cancel: &CancellationToken,
        ) -> Result<port::DownloadedArtifact, port::DownloadError> {
            Err(port::DownloadError::Cancelled)
        }

        async fn download_many(
            &self,
            _requests: &[DownloadRequest],
            _concurrency: usize,
            _progress: Option<&dyn ProgressSink>,
            _cancel: &CancellationToken,
        ) -> Vec<Result<port::DownloadedArtifact, port::DownloadError>> {
            Vec::new()
        }
    }

    struct NoopExtractor;

    impl ModpackExtractor for NoopExtractor {
        fn read_index(&self, _archive: &Path) -> Result<ModpackIndex, ModpackExtractError> {
            Err(ModpackExtractError::Extract("unused".to_owned()))
        }

        fn extract_overrides(
            &self,
            _archive: &Path,
            _dest: &Path,
        ) -> Result<(), ModpackExtractError> {
            Err(ModpackExtractError::Extract("unused".to_owned()))
        }
    }

    fn service_for_defaults() -> ModpackService {
        ModpackService::new(
            Arc::new(ModpackSourceRegistry::new(Vec::new())),
            Arc::new(NoopExtractor),
            Arc::new(NoopDownloader),
        )
    }

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

    /// 镜像只换 scheme + host，路径原样接上：镜像站按上游同一套路径回源
    #[test]
    fn mirrors_replace_origin_and_keep_path() {
        let service = service_for_defaults().with_mirrors(vec![
            Url::parse("https://a.example").expect("url"),
            Url::parse("https://b.example/prefix").expect("url"),
        ]);
        let upstream =
            Url::parse("https://cdn.modrinth.com/data/AANobbMI/versions/x/s.jar").expect("url");
        let got: Vec<String> = service
            .mirrors_for(&upstream)
            .into_iter()
            .map(|url| url.to_string())
            .collect();
        assert_eq!(
            got,
            vec![
                "https://a.example/data/AANobbMI/versions/x/s.jar",
                "https://b.example/prefix/data/AANobbMI/versions/x/s.jar",
            ]
        );
    }

    /// 没配镜像时不给候选：多一次失败尝试只会让报错变慢
    #[test]
    fn mirrors_empty_when_unconfigured() {
        let service = service_for_defaults();
        let upstream = Url::parse("https://cdn/mod/x.jar").expect("url");
        assert!(service.mirrors_for(&upstream).is_empty());
    }

    /// 并发为 0 不能把整批退化成串行
    #[test]
    fn zero_concurrency_keeps_default() {
        let service = service_for_defaults().with_file_concurrency(0);
        assert_eq!(service.file_concurrency, FILE_CONCURRENCY);
    }

    /// 配了并发就按配的来
    #[test]
    fn concurrency_is_applied() {
        let service = service_for_defaults().with_file_concurrency(16);
        assert_eq!(service.file_concurrency, 16);
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
        let file = ModpackFile::new(
            "mods/a.jar".to_owned(),
            vec!["http://cdn/a.jar".to_owned()],
            1,
            ModpackHashes::default(),
            None,
        );
        assert!(first_https(&file.downloads).is_err());
    }
}
