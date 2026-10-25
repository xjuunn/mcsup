use crate::fs_error::FsError;
use domain::java::{JavaMajor, JavaPackageType, JavaRuntime, JavaVendor};
use port::{
    ArchiveExtractor, ArtifactDownloader, CancellationToken, DownloadRequest, DownloadedArtifact,
    JavaPackageRequest, JavaRuntimeRepository, JavaScanner, JavaSourceRegistryPort, ProgressSink,
    RepositoryError, ScannedJava, SourceError,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tracing::info;

/// 硬中断（断电 / kill）遗留临时条目的判定年龄
///
/// 正常失败路径就地清理；超过该年龄的 `.tmp-*` / `.bak-*` / `*.tmp` 视为
/// 上次进程没来得及清的残留。够老才删，避免误删并发安装的进行中产物。
const STALE_TEMP_AGE: Duration = Duration::from_secs(3600);

/// Java 操作失败
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum JavaError {
    /// 运行时未注册
    #[error("java runtime not found: {0}")]
    NotFound(String),
    /// 来源解析或上游访问失败
    #[error(transparent)]
    Source(#[from] SourceError),
    /// 下载或校验失败
    #[error(transparent)]
    Download(#[from] port::DownloadError),
    /// 注册表失败
    #[error(transparent)]
    Repo(#[from] RepositoryError),
    /// 文件系统失败（共用载荷见 [`FsError`]）
    #[error(transparent)]
    Fs(#[from] FsError),
    /// 扫描失败
    #[error("scan failed: {0}")]
    Scan(String),
}

impl JavaError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        FsError::io(path, source).into()
    }
}

/// 扫描结果汇总
#[derive(Debug, Clone)]
pub struct ScanOutcome {
    /// 新注册条数（注册表中此前不存在的路径）
    pub registered: usize,
    /// 跳过条数（已存在，仅刷新元数据；写入失败也计入并 warn）
    pub skipped: usize,
}

/// 安装结果
#[derive(Debug, Clone)]
pub struct InstallOutcome {
    pub runtime: JavaRuntime,
    pub artifact: DownloadedArtifact,
}

/// 卸载结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UninstallOutcome {
    /// mcsup 管理的运行时：文件与记录一并删除
    RemovedManaged { path: PathBuf },
    /// 系统运行时：仅移除记录，文件保留
    UnregisteredSystem { path: PathBuf },
}

/// 安装阶段回调：CLI 用它实时切换 spinner 文案
pub trait StageSink: Send + Sync {
    /// 进入新阶段
    fn on_stage(&self, stage: InstallStage);
}

/// 安装流程的阶段划分
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallStage {
    /// 探测并排序下载源
    Probing,
    /// 查询可用发行包
    Listing,
    /// 解析下载地址
    Resolving,
    /// 下载归档
    Downloading,
    /// 解压并注册
    Extracting,
    /// 下载安装器本体（`Forge` / `NeoForge` 等 install 方式的来源）
    DownloadingInstaller,
    /// 运行安装器产出服务端目录：耗时可长达数分钟，必须有回显
    Installing,
    /// 安装完成，登记服务端核心（写注册表）
    Finalizing,
}

impl InstallStage {
    /// 面向用户的简短文案
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::Probing => "探测下载源…",
            Self::Listing => "查询可用版本…",
            Self::Resolving => "解析下载地址…",
            Self::Downloading => "下载 Java…",
            Self::Extracting => "解压并注册…",
            Self::DownloadingInstaller => "下载安装器…",
            Self::Installing => "运行安装器（下载 libraries 可能需要几分钟）…",
            Self::Finalizing => "登记服务端核心…",
        }
    }
}

/// Java 生命周期编排：扫描、安装、卸载、列表
///
/// 各组件均为 `Arc`，克隆即共享同一套装配（组合根把同一实例交给
/// `java` 命令与 lifecycle 编排两个消费者）。
#[derive(Clone)]
pub struct JavaService {
    repo: Arc<dyn JavaRuntimeRepository>,
    scanner: Arc<dyn JavaScanner>,
    registry: Arc<dyn JavaSourceRegistryPort>,
    downloader: Arc<dyn ArtifactDownloader>,
    /// 归档解压：重活经 port 能力注入，本层只做编排（见 [`extract_and_swap`]）
    extractor: Arc<dyn ArchiveExtractor>,
    /// mcsup 管理的 JVM 安装根目录 `{data_dir}/mcsup/java`
    java_root: PathBuf,
}

impl std::fmt::Debug for JavaService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JavaService").finish_non_exhaustive()
    }
}

impl JavaService {
    /// 组合依赖
    #[must_use]
    pub fn new(
        repo: Arc<dyn JavaRuntimeRepository>,
        scanner: Arc<dyn JavaScanner>,
        registry: Arc<dyn JavaSourceRegistryPort>,
        downloader: Arc<dyn ArtifactDownloader>,
        extractor: Arc<dyn ArchiveExtractor>,
        java_root: PathBuf,
    ) -> Self {
        Self {
            repo,
            scanner,
            registry,
            downloader,
            extractor,
            java_root,
        }
    }

    /// 是否已扫描过本机
    ///
    /// # Errors
    ///
    /// 注册表读取失败时返回 [`JavaError::Repo`]
    pub fn has_scanned(&self) -> Result<bool, JavaError> {
        Ok(self.repo.scan_state()?.is_some())
    }

    /// 扫描本机 Java 并注册；`force` 时忽略已扫描标记
    ///
    /// # Errors
    ///
    /// 扫描器或注册表失败时返回 [`JavaError`]
    pub fn scan(
        &self,
        force: bool,
        on_found: &mut dyn FnMut(&ScannedJava),
    ) -> Result<ScanOutcome, JavaError> {
        if !force && self.has_scanned()? {
            info!("java already scanned, skip");
            return Ok(ScanOutcome {
                registered: 0,
                skipped: 0,
            });
        }
        let now = crate::util::unix_now();
        let mut registered = 0usize;
        let mut skipped = 0usize;
        // 去重是扫描器的契约（见 port::JavaScanner）：service 不再维护 seen，
        // 只管注册与回调
        self.scanner
            .scan(&mut |found| {
                on_found(&found);
                let existed = match self.repo.get_by_path(&found.path) {
                    Ok(_) => true,
                    Err(RepositoryError::NotFound(_)) => false,
                    Err(err) => {
                        tracing::warn!(path = %found.path.display(), error = %err, "get_by_path failed");
                        skipped += 1;
                        return;
                    }
                };
                let runtime = JavaRuntime {
                    id: uuid::Uuid::new_v4().to_string(),
                    path: found.path.clone(),
                    major: found.major,
                    version: found.version.clone(),
                    vendor: found.vendor,
                    managed: false,
                    created_at: now,
                    updated_at: now,
                };
                match self.repo.upsert(&runtime) {
                    // 已存在的路径只是刷新元数据，不算新注册
                    Ok(()) => {
                        if existed {
                            skipped += 1;
                        } else {
                            registered += 1;
                        }
                    }
                    Err(err) => {
                        tracing::warn!(path = %found.path.display(), error = %err, "upsert failed");
                        skipped += 1;
                    }
                }
            })
            .map_err(|err| JavaError::Scan(err.to_string()))?;
        self.repo.mark_scanned(now)?;
        info!(registered, skipped, "java scan finished");
        Ok(ScanOutcome {
            registered,
            skipped,
        })
    }

    /// 列出全部已注册运行时；未扫描过则先触发扫描（无回显）
    ///
    /// # Errors
    ///
    /// 扫描或注册表失败时返回 [`JavaError`]
    pub fn list(&self) -> Result<Vec<JavaRuntime>, JavaError> {
        self.list_or_scan(&mut || {}, &mut |_| {})
    }

    /// 列出全部已注册运行时；未扫描过则先扫描并逐个回调
    ///
    /// 「先扫描再列出」的判定收敛在 service，入口不再各自重写分支：
    /// `on_first_scan` 仅在首次扫描将要开始时调用一次（入口用于打印阶段文案），
    /// `on_found` 逐个上报扫描到的运行时。
    ///
    /// # Errors
    ///
    /// 扫描或注册表失败时返回 [`JavaError`]
    pub fn list_or_scan(
        &self,
        on_first_scan: &mut dyn FnMut(),
        on_found: &mut dyn FnMut(&ScannedJava),
    ) -> Result<Vec<JavaRuntime>, JavaError> {
        if !self.has_scanned()? {
            on_first_scan();
            self.scan(false, on_found)?;
        }
        Ok(self.repo.list()?)
    }

    /// 从远端安装到 `{java_root}/{id}` 并注册
    ///
    /// `stage` 用于在各阶段切换回显文案；`progress` 只覆盖下载字节进度
    ///
    /// # Errors
    ///
    /// 来源不可达、下载失败或注册失败时返回 [`JavaError`]
    pub async fn install(
        &self,
        major: JavaMajor,
        package_type: JavaPackageType,
        vendor: Option<JavaVendor>,
        stage: Option<&dyn StageSink>,
        progress: Option<&dyn ProgressSink>,
        cancel: &CancellationToken,
    ) -> Result<InstallOutcome, JavaError> {
        let notify = |s: InstallStage| {
            if let Some(sink) = stage {
                sink.on_stage(s);
            }
        };

        // 源探测与排序属于安装编排的一部分，换入口（CLI/API/MCP）共享同一阶段机
        notify(InstallStage::Probing);
        let registry = until_cancelled(self.registry.ranked(), cancel).await?;

        notify(InstallStage::Listing);
        let request = JavaPackageRequest::for_current_platform(major, package_type);
        let packages = until_cancelled(registry.list_packages(&request), cancel).await??;
        let package = packages
            .into_iter()
            .find(|pkg| vendor.is_none_or(|v| pkg.vendor == v))
            .ok_or_else(|| {
                JavaError::Source(SourceError::NotFound(
                    domain::server::version::VersionId::new(major.to_string()),
                ))
            })?;

        notify(InstallStage::Resolving);
        let resolved = until_cancelled(registry.resolve_binary(&package), cancel).await??;

        // 上次硬中断（断电 / kill -9）残留的临时文件在安装开始时统一回收
        sweep_stale_temp(&self.java_root);

        // 目录名用「厂商-大版本」，短且稳定：重装同一大版本就地替换，
        // 不随归档文件名（版本号、平台后缀）膨胀路径
        let dir_name = install_dir_name(&package);
        let install_dir = self.java_root.join(&dir_name);
        // 失败清理只删本次新建的目录：重装失败时旧运行时必须原样保留
        let dir_preexisting = install_dir.exists();
        std::fs::create_dir_all(&install_dir)
            .map_err(|source| JavaError::io(&install_dir, source))?;
        let filename = download_filename(&resolved.location);
        let destination = install_dir.join(&filename);

        // 取消落在解析完成与下载发起之间：下载器入口也会查，这里兜底让任何
        // ArtifactDownloader 实现（含不查取消的）都受控，且目录尚未被使用
        if cancel.is_cancelled() {
            cleanup_partial_install(&install_dir, &destination, dir_preexisting);
            return Err(JavaError::Download(port::DownloadError::Cancelled));
        }

        notify(InstallStage::Downloading);
        let request = DownloadRequest {
            location: resolved.location,
            destination: destination.clone(),
            expected: None,
            expected_size: resolved.expected_size,
        };
        let artifact = match self.downloader.download(&request, progress, cancel).await {
            Ok(artifact) => artifact,
            Err(err) => {
                // 下载器已清自己的 .tmp，这里回收目录本身；取消同走此路径
                cleanup_partial_install(&install_dir, &destination, dir_preexisting);
                return Err(err.into());
            }
        };

        // Ctrl-C 落在下载完成与解压之间：不启动耗时的解压，直接回收
        if cancel.is_cancelled() {
            cleanup_partial_install(&install_dir, &destination, dir_preexisting);
            return Err(JavaError::Download(port::DownloadError::Cancelled));
        }

        notify(InstallStage::Extracting);
        // 解压失败时归档（可能数百 MB）与半成品都在 install_dir 里，必须回收
        let home = match extract_and_swap(&destination, &install_dir, self.extractor.as_ref()) {
            Ok(home) => home,
            Err(err) => {
                cleanup_partial_install(&install_dir, &destination, dir_preexisting);
                return Err(err);
            }
        };

        // 解压不可打断（同步解包）：取消落在解压期间时，全新安装连完整产物一并
        // 回收，不留未注册的目录；重装场景旧目录已被替换，删除只会两头落空，
        // 照常注册（入口层见取消后静默退出，不打印成功）
        if cancel.is_cancelled() && !dir_preexisting {
            let _ = std::fs::remove_dir_all(&install_dir);
            return Err(JavaError::Download(port::DownloadError::Cancelled));
        }

        let now = crate::util::unix_now();
        let runtime = JavaRuntime {
            id: dir_name,
            path: home,
            major: package.major,
            version: package.version,
            vendor: package.vendor,
            managed: true,
            created_at: now,
            updated_at: now,
        };
        self.repo.upsert(&runtime)?;
        info!(major = %runtime.major, path = %runtime.path.display(), "java installed");
        Ok(InstallOutcome { runtime, artifact })
    }

    /// 卸载：managed 删整个安装目录+记录，系统只删记录
    ///
    /// managed 的删除目标是 home 在 `java_root` 下的直接子目录
    /// （`managed_install_dir`）——只删 home 会把外层包装目录留成空壳。
    /// 删完再自底向上清掉 `java_root` 下的空目录链，旧版嵌套布局
    /// （`{java_root}/{dir}/{dir}`）残留的空壳一并扫净。
    /// 先删文件再注销：删除失败时记录还在，退出占用后重试即可。
    ///
    /// # Errors
    ///
    /// 未注册返回 [`JavaError::NotFound`]；IO 或注册表失败返回对应错误
    pub fn uninstall(&self, path: &Path) -> Result<UninstallOutcome, JavaError> {
        let runtime = self.repo.get_by_path(path).map_err(|err| match err {
            RepositoryError::NotFound(key) => JavaError::NotFound(key),
            other => JavaError::Repo(other),
        })?;
        if runtime.managed {
            if let Some(install_dir) = managed_install_dir(&self.java_root, &runtime.path)
                && install_dir.exists()
            {
                std::fs::remove_dir_all(&install_dir)
                    .map_err(|source| JavaError::io(&install_dir, source))?;
                prune_empty_parents(&install_dir, &self.java_root);
            }
            self.repo.remove(path)?;
            info!(path = %runtime.path.display(), "managed java removed");
            Ok(UninstallOutcome::RemovedManaged { path: runtime.path })
        } else {
            self.repo.remove(path)?;
            info!(path = %runtime.path.display(), "system java unregistered");
            Ok(UninstallOutcome::UnregisteredSystem { path: runtime.path })
        }
    }

    /// 按大版本查找已注册运行时（`server set java-version` 用）
    ///
    /// # Errors
    ///
    /// 注册表失败时返回 [`JavaError::Repo`]
    pub fn find_by_major(&self, major: JavaMajor) -> Result<Vec<JavaRuntime>, JavaError> {
        Ok(self
            .repo
            .list()?
            .into_iter()
            .filter(|rt| rt.major == major)
            .collect())
    }

    /// 已启用的下载源名
    #[must_use]
    pub fn source_names(&self) -> Vec<&'static str> {
        self.registry.source_names()
    }
}

fn download_filename(location: &url::Url) -> String {
    location
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .filter(|s| !s.is_empty())
        .map_or_else(|| "java-archive".into(), str::to_owned)
}

/// 自底向上删掉 `root` 以下的空目录链（不含 `root` 本身）
///
/// 卸载残留的空壳目录（如旧版嵌套布局的外层）逐层清掉；任一层非空或
/// 删除失败即停——那层属于别的运行时或被占用，不能动。
fn prune_empty_parents(start: &Path, root: &Path) {
    let mut cursor = start.parent().map(Path::to_path_buf);
    while let Some(dir) = cursor {
        if dir == root || !dir.starts_with(root) {
            break;
        }
        if std::fs::remove_dir(&dir).is_err() {
            break;
        }
        cursor = dir.parent().map(Path::to_path_buf);
    }
}

/// managed 运行时的安装根：`{java_root}/{dir}`（install 落盘的那一层）
///
/// JVM home 可能嵌套在归档顶层目录里（`{dir}/{dir}/Contents/Home`），卸载必须
/// 删到 `java_root` 的直接子目录，只删 home 会留下 `Contents` 空壳。
/// home 不在 `java_root` 下、或就是 `java_root` 本身时返回 `None`（不删盘）。
fn managed_install_dir(java_root: &Path, home: &Path) -> Option<PathBuf> {
    let relative = home.strip_prefix(java_root).ok()?;
    let first = relative.components().next()?;
    Some(java_root.join(first.as_os_str()))
}

/// 安装失败后的目录回收
///
/// 归档文件（可能数百 MB）总是删掉；`install_dir` 是本次新建的空壳时整个删除，
/// 重装场景（目录本就存在）只删归档，旧运行时原样保留。
fn cleanup_partial_install(install_dir: &Path, archive: &Path, dir_preexisting: bool) {
    let _ = std::fs::remove_file(archive);
    if !dir_preexisting {
        let _ = std::fs::remove_dir_all(install_dir);
    }
}

/// 取消可打断的阶段等待（轮询令牌，粒度 [`CANCEL_POLL`]）
///
/// [`CancellationToken`] 是原子标志、没有唤醒机制；探测排序 / 查询版本 / 解析
/// 下载地址各自都要跑完一轮网络往返，期间取消标志无人检查——Ctrl-C 后观感是
/// 「按了没反应，过了几秒才停」。把阶段 future 与取消轮询竞争，取消在轮询粒度
/// 内立即生效；进行中的请求随 future 丢弃（子任务即使在后台跑完也无人消费）。
async fn until_cancelled<T>(
    future: impl Future<Output = T>,
    cancel: &CancellationToken,
) -> Result<T, JavaError> {
    tokio::select! {
        biased;
        () = async {
            while !cancel.is_cancelled() {
                tokio::time::sleep(CANCEL_POLL).await;
            }
        } => Err(JavaError::Download(port::DownloadError::Cancelled)),
        value = future => Ok(value),
    }
}

/// 阶段取消的轮询粒度
const CANCEL_POLL: Duration = Duration::from_millis(25);

/// 回收硬中断（断电 / kill -9）残留的临时条目
///
/// 进程被强杀时析构与错误路径都来不及跑，`.tmp-*` 解压目录、`.bak-*` 备份
/// 目录、`*.tmp` 下载半成品会永久留在 `java_root`。下次安装开始时按年龄
/// 清理：够老（[`STALE_TEMP_AGE`]）才删，不碰并发安装的进行中产物。
fn sweep_stale_temp(java_root: &Path) {
    let Ok(entries) = std::fs::read_dir(java_root) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let is_temp = name.starts_with(".tmp-")
            || name.starts_with(".bak-")
            || (name.ends_with(".tmp") && name.starts_with('.'));
        if !is_temp {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > STALE_TEMP_AGE);
        if !stale {
            continue;
        }
        let path = entry.path();
        let removed = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match removed {
            Ok(()) => info!(path = %path.display(), "removed stale temp from previous run"),
            Err(err) => tracing::warn!(path = %path.display(), %err, "failed to remove stale temp"),
        }
    }
}

/// 安装目录名：`{vendor}-{major}`（如 `zulu-27`、`temurin-21-jre`）
///
/// 短且稳定：重装同一大版本就地替换，路径不随归档文件名（版本号、平台后缀）
/// 膨胀，卸载时 `managed_install_dir` 直接删到这一层，不留包装目录空壳。
fn install_dir_name(package: &port::JavaPackage) -> String {
    let base = format!("{}-{}", package.vendor.as_str(), package.major);
    match package.package_type {
        JavaPackageType::Jdk => base,
        JavaPackageType::Jre => format!("{base}-jre"),
    }
}

/// 在解压目录中定位 JVM home：根目录一对候选（本身 / bundle）优先，
/// 再对一层子目录依次做同样的探测
///
/// 候选与顺序统一在 [`domain::java::find_jvm_home`]：必须与扫描走同一份，
/// 否则同一目录会被两处登记成不同身份。判定形状（`bin/java(.exe)`）见
/// [`domain::java::java_bin_under`]。
fn locate_java_home(root: &Path) -> Option<PathBuf> {
    if let Some(home) = domain::java::find_jvm_home(root, Path::is_file) {
        return Some(home);
    }
    let entries = std::fs::read_dir(root).ok()?;
    entries
        .flatten()
        .find_map(|entry| domain::java::find_jvm_home(&entry.path(), Path::is_file))
}

/// 校验解压产物里有当前平台可用的 JVM home
///
/// 下错平台 / 架构的归档（例如在 Windows 上装到 macOS 包）解压不会报错，
/// 但产物里没有 `bin/java.exe`，跑不起来。必须在这里失败，而不是注册成运行时。
///
/// # Errors
///
/// 找不到 JVM home 时返回 [`JavaError::Fs`]，消息点明可能是平台不符
fn require_jvm_home(archive: &Path, dir: &Path) -> Result<PathBuf, JavaError> {
    match locate_java_home(dir) {
        Some(home) => Ok(home),
        None => Err(java_io_err(
            archive,
            format!(
                "解压后未找到可用的 JVM home（期望 bin/{}），归档可能属于其他平台或架构",
                domain::java::java_exe_name()
            ),
        )),
    }
}

/// 解压归档到临时目录，成功后把 JVM home 提升到目标目录根并原子替换
///
/// 归档通常自带一层顶层目录（`zulu27…/bin/java.exe`），直接整树入位会得到
/// `{java_root}/{dir}/{dir}/…` 的双层嵌套。因此解压到临时目录后先定位 JVM
/// home，再把 home 本身改名到目标目录——安装路径只有一层，无包装目录空壳。
///
/// 流程：解压到唯一临时目录 → 校验并定位 JVM home → 旧目录改名为备份 →
/// home 改名入位（清理临时目录与备份）；任一步失败都回滚备份，原目录不丢。
/// 非压缩格式（dmg/msi）不解压，但仍须在其中找到 JVM home 才放行。
///
/// # Errors
///
/// 解压失败、产物无 JVM home、改名失败时返回 [`JavaError::Fs`]（或解压器错误）
fn extract_and_swap(
    archive: &Path,
    into: &Path,
    extractor: &dyn ArchiveExtractor,
) -> Result<PathBuf, JavaError> {
    // 非压缩归档（dmg / msi 等安装包）：不走临时目录（归档文件本身在 into 里，
    // 不能删），但同样必须验出 JVM home——安装包目录注册成运行时是跑不起来的
    if !extractor.is_extractable(archive) {
        return require_jvm_home(archive, into);
    }

    let parent = into.parent().unwrap_or(into);
    let name = into.file_name().unwrap_or_default().to_string_lossy();
    // 后缀带纳秒时间戳：并发安装同一包时两个临时目录不会互删
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let tmp = parent.join(format!(".tmp-{name}-{stamp}"));
    std::fs::create_dir_all(&tmp).map_err(|source| JavaError::io(&tmp, source))?;

    let extract_result = extractor
        .extract(archive, &tmp)
        .map_err(|err| java_io_err(archive, err.to_string()));
    if let Err(err) = extract_result {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(err);
    }

    // 解压成功但没有可用 home：多半是下错平台 / 架构的包（如 Windows 上装到 macOS 包），
    // 这种产物跑不起来，必须在替换与注册前拦下
    let home = match require_jvm_home(archive, &tmp) {
        Ok(home) => home,
        Err(err) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(err);
        }
    };

    // 归档已在 tmp 中解压完毕：旧目录先改名备份，改名失败可原样回滚
    let bak = parent.join(format!(".bak-{name}-{stamp}"));
    let had_old = into.exists();
    if had_old && let Err(source) = std::fs::rename(into, &bak) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(JavaError::io(into, source));
    }

    // home 提升到目标目录根；home 就是 tmp 根时整树直接入位
    let promote = if home == tmp {
        std::fs::rename(&tmp, into)
    } else {
        match std::fs::rename(&home, into) {
            Ok(()) => {
                // 包装目录残渣（归档顶层目录的其余内容）一并清掉
                let _ = std::fs::remove_dir_all(&tmp);
                Ok(())
            }
            Err(source) => Err(source),
        }
    };
    if let Err(source) = promote {
        // 回滚：新目录没入位，把旧目录放回去
        if had_old {
            let _ = std::fs::rename(&bak, into);
        }
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(JavaError::io(into, source));
    }
    if had_old {
        let _ = std::fs::remove_dir_all(&bak);
    }
    Ok(into.to_path_buf())
}

/// 构造以归档路径为上下文的 IO 错误
fn java_io_err(path: &Path, source: String) -> JavaError {
    JavaError::io(path, std::io::Error::other(source))
}

/// 对外重导出，便于 CLI 构造聚合注册表
pub use port::JavaPackage as RemoteJavaPackage;

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use port::RepositoryError;

    struct MemRepo {
        runtimes: std::sync::Mutex<Vec<JavaRuntime>>,
        scanned: std::sync::Mutex<Option<i64>>,
    }

    impl MemRepo {
        fn new() -> Self {
            Self {
                runtimes: std::sync::Mutex::new(Vec::new()),
                scanned: std::sync::Mutex::new(None),
            }
        }
    }

    impl JavaRuntimeRepository for MemRepo {
        fn upsert(&self, runtime: &JavaRuntime) -> Result<(), RepositoryError> {
            let mut list = self.runtimes.lock().expect("lock");
            list.retain(|r| r.path != runtime.path);
            list.push(runtime.clone());
            Ok(())
        }

        fn list(&self) -> Result<Vec<JavaRuntime>, RepositoryError> {
            Ok(self.runtimes.lock().expect("lock").clone())
        }

        fn get_by_path(&self, path: &Path) -> Result<JavaRuntime, RepositoryError> {
            self.runtimes
                .lock()
                .expect("lock")
                .iter()
                .find(|r| r.path == path)
                .cloned()
                .ok_or_else(|| RepositoryError::NotFound(path.display().to_string()))
        }

        fn remove(&self, path: &Path) -> Result<(), RepositoryError> {
            let mut list = self.runtimes.lock().expect("lock");
            let before = list.len();
            list.retain(|r| r.path != path);
            if list.len() == before {
                return Err(RepositoryError::NotFound(path.display().to_string()));
            }
            Ok(())
        }

        fn scan_state(&self) -> Result<Option<i64>, RepositoryError> {
            Ok(*self.scanned.lock().expect("lock"))
        }

        fn mark_scanned(&self, at: i64) -> Result<(), RepositoryError> {
            *self.scanned.lock().expect("lock") = Some(at);
            Ok(())
        }
    }

    struct FakeScanner {
        homes: Vec<PathBuf>,
    }

    impl JavaScanner for FakeScanner {
        fn scan(
            &self,
            on_found: &mut dyn FnMut(ScannedJava),
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            for home in &self.homes {
                on_found(ScannedJava {
                    path: home.clone(),
                    major: JavaMajor::parse(21).expect("21"),
                    version: "21.0.5".into(),
                    vendor: JavaVendor::Temurin,
                });
            }
            Ok(())
        }
    }

    struct FakeRegistry;

    #[async_trait]
    impl JavaSourceRegistryPort for FakeRegistry {
        fn source_names(&self) -> Vec<&'static str> {
            vec!["fake"]
        }

        async fn ranked(&self) -> Arc<dyn JavaSourceRegistryPort> {
            Arc::new(FakeRegistry)
        }

        async fn list_packages(
            &self,
            request: &JavaPackageRequest,
        ) -> Result<Vec<port::JavaPackage>, SourceError> {
            let major = request.major;
            Ok(vec![port::JavaPackage {
                id: "pkg1".into(),
                vendor: JavaVendor::Temurin,
                major,
                version: format!("{major}.0.5"),
                package_type: request.package_type,
            }])
        }

        async fn resolve_binary(
            &self,
            package: &port::JavaPackage,
        ) -> Result<port::ResolvedJava, SourceError> {
            Ok(port::ResolvedJava {
                package: package.clone(),
                location: url::Url::parse("https://example.com/java-bin").expect("url"),
                expected_size: None,
            })
        }
    }

    struct NoopDownloader;

    #[async_trait]
    impl ArtifactDownloader for NoopDownloader {
        async fn download(
            &self,
            request: &DownloadRequest,
            _progress: Option<&dyn ProgressSink>,
            _cancel: &CancellationToken,
        ) -> Result<DownloadedArtifact, port::DownloadError> {
            // 模拟已展开的安装目录：归档本身 + 可运行的 bin/java
            let dir = request
                .destination
                .parent()
                .ok_or_else(|| port::DownloadError::Io(std::io::Error::other("no parent")))?;
            let bin = dir.join("bin");
            std::fs::create_dir_all(&bin).map_err(port::DownloadError::Io)?;
            std::fs::write(bin.join(domain::java::java_exe_name()), b"stub")
                .map_err(port::DownloadError::Io)?;
            std::fs::write(&request.destination, b"stub").map_err(port::DownloadError::Io)?;
            Ok(DownloadedArtifact {
                path: request.destination.clone(),
                size: 4,
            })
        }
    }

    /// 下载一开工就失败：错误、超时、Ctrl-C 取消都收敛到这条返回路径
    struct FailingDownloader;

    #[async_trait]
    impl ArtifactDownloader for FailingDownloader {
        async fn download(
            &self,
            _request: &DownloadRequest,
            _progress: Option<&dyn ProgressSink>,
            _cancel: &CancellationToken,
        ) -> Result<DownloadedArtifact, port::DownloadError> {
            Err(port::DownloadError::Cancelled)
        }
    }

    /// 只落归档、不产出可运行的 bin/java：解压校验必然失败
    struct ArchiveOnlyDownloader;

    #[async_trait]
    impl ArtifactDownloader for ArchiveOnlyDownloader {
        async fn download(
            &self,
            request: &DownloadRequest,
            _progress: Option<&dyn ProgressSink>,
            _cancel: &CancellationToken,
        ) -> Result<DownloadedArtifact, port::DownloadError> {
            std::fs::write(&request.destination, b"stub").map_err(port::DownloadError::Io)?;
            Ok(DownloadedArtifact {
                path: request.destination.clone(),
                size: 4,
            })
        }
    }

    /// 归档格式判定与真实工具链一致；测试下载不产生真归档，extract 不会被触发
    struct FakeExtractor;

    impl ArchiveExtractor for FakeExtractor {
        #[allow(clippy::case_sensitive_file_extension_comparisons)]
        fn is_extractable(&self, archive: &Path) -> bool {
            let Some(name) = archive.file_name() else {
                return false;
            };
            // 与 file::extract::is_gzip 同规则：先小写再比后缀
            let name = name.to_string_lossy().to_ascii_lowercase();
            name.ends_with(".zip") || name.ends_with(".tar.gz") || name.ends_with(".tgz")
        }

        fn extract(&self, _archive: &Path, dest: &Path) -> Result<(), port::ExtractError> {
            std::fs::create_dir_all(dest).map_err(|err| port::ExtractError(err.to_string()))?;
            Ok(())
        }
    }

    /// 模拟真实归档：解压出的顶层目录里套着 JVM home（`zulu27/bin/java.exe`）
    struct NestedExtractor;

    impl ArchiveExtractor for NestedExtractor {
        fn is_extractable(&self, _archive: &Path) -> bool {
            true
        }

        fn extract(&self, _archive: &Path, dest: &Path) -> Result<(), port::ExtractError> {
            let home = dest.join("zulu27.28.101").join("bin");
            std::fs::create_dir_all(&home).map_err(|err| port::ExtractError(err.to_string()))?;
            std::fs::write(home.join(domain::java::java_exe_name()), b"stub")
                .map_err(|err| port::ExtractError(err.to_string()))?;
            Ok(())
        }
    }

    /// 归档自带顶层目录时，home 必须提升到安装目录根：最终路径是
    /// `{java_root}/{dir}/bin/java.exe`，不留 `{dir}/{dir}` 嵌套与包装目录空壳
    #[test]
    fn extract_and_swap_promotes_nested_home() {
        let tmp = tempfile::tempdir().expect("tmp");
        let into = tmp.path().join("zulu-27");
        std::fs::create_dir_all(&into).expect("mkdir into");
        let archive = into.join("zulu.zip");
        std::fs::write(&archive, b"stub").expect("archive");

        let home = extract_and_swap(&archive, &into, &NestedExtractor).expect("swap");

        assert_eq!(home, into);
        assert!(
            into.join("bin")
                .join(domain::java::java_exe_name())
                .is_file(),
            "home must sit at install root"
        );
        // 包装目录与临时目录都不残留
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("read_dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".tmp-") || name == "zulu27.28.101")
            .collect();
        assert!(leftovers.is_empty(), "leftovers: {leftovers:?}");
    }

    fn svc(tmp: &Path) -> (JavaService, Arc<MemRepo>) {
        svc_with(tmp, Arc::new(NoopDownloader))
    }

    /// 用指定下载器装配服务：失败清理类用例需要下载器行为可控
    fn svc_with(
        tmp: &Path,
        downloader: Arc<dyn ArtifactDownloader>,
    ) -> (JavaService, Arc<MemRepo>) {
        svc_parts(tmp, downloader, Arc::new(FakeRegistry))
    }

    /// 指定下载器与聚合注册表：取消类用例需要注册表挂起 / 在阶段中触发取消
    fn svc_parts(
        tmp: &Path,
        downloader: Arc<dyn ArtifactDownloader>,
        registry: Arc<dyn JavaSourceRegistryPort>,
    ) -> (JavaService, Arc<MemRepo>) {
        let repo = Arc::new(MemRepo::new());
        let scanner = Arc::new(FakeScanner {
            homes: vec![tmp.join("jdk21")],
        });
        let extractor = Arc::new(FakeExtractor);
        let java_root = tmp.join("java");
        std::fs::create_dir_all(&java_root).expect("mkdir");
        (
            JavaService::new(
                repo.clone(),
                scanner,
                registry,
                downloader,
                extractor,
                java_root,
            ),
            repo,
        )
    }

    #[test]
    fn scan_registers_once() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (service, _repo) = svc(tmp.path());
        let mut count = 0;
        let out = service.scan(false, &mut |_| count += 1).expect("scan");
        assert_eq!(out.registered, 1);
        assert_eq!(count, 1);
        let again = service.scan(false, &mut |_| {}).expect("scan again");
        assert_eq!(again.registered, 0);
    }

    #[test]
    fn scan_force_reruns() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (service, _repo) = svc(tmp.path());
        service.scan(false, &mut |_| {}).expect("first");
        let forced = service.scan(true, &mut |_| {}).expect("force");
        // 已注册路径再次扫描只刷新元数据，不再计为新注册
        assert_eq!(forced.registered, 0);
        assert_eq!(forced.skipped, 1);
    }

    #[test]
    fn scan_counts_new_vs_existing() {
        let tmp = tempfile::tempdir().expect("tmp");
        let repo = Arc::new(MemRepo::new());
        let scanner = Arc::new(FakeScanner {
            homes: vec![tmp.path().join("jdk21"), tmp.path().join("jdk17")],
        });
        let service = JavaService::new(
            repo,
            scanner,
            Arc::new(FakeRegistry),
            Arc::new(NoopDownloader),
            Arc::new(FakeExtractor),
            tmp.path().join("java"),
        );
        let first = service.scan(true, &mut |_| {}).expect("first");
        assert_eq!(first.registered, 2);
        assert_eq!(first.skipped, 0);
        let second = service.scan(true, &mut |_| {}).expect("second");
        assert_eq!(second.registered, 0);
        assert_eq!(second.skipped, 2);
    }

    #[test]
    fn list_triggers_scan_once() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (service, _repo) = svc(tmp.path());
        let list = service.list().expect("list");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].major.as_i64(), 21);
    }

    #[test]
    fn uninstall_system_only_removes_record() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (service, _repo) = svc(tmp.path());
        service.scan(false, &mut |_| {}).expect("scan");
        let path = tmp.path().join("jdk21");
        std::fs::create_dir_all(&path).expect("mkdir home");
        let outcome = service.uninstall(&path).expect("uninstall");
        assert_eq!(
            outcome,
            UninstallOutcome::UnregisteredSystem { path: path.clone() }
        );
        assert!(path.exists(), "system files must remain");
        assert!(service.list().expect("list").is_empty());
    }

    #[test]
    fn uninstall_unknown_is_not_found() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (service, _repo) = svc(tmp.path());
        let err = service.uninstall(Path::new("/ghost")).expect_err("missing");
        assert!(matches!(err, JavaError::NotFound(_)));
    }

    /// managed 的 home 嵌套在归档顶层目录里：必须删到 `java_root` 的直接子目录，
    /// 只删 home 会留下 `Contents` 空壳（`Info.plist` / `_CodeSignature` 等）
    #[test]
    fn uninstall_managed_removes_whole_install_dir() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (service, repo) = svc(tmp.path());
        let java_root = tmp.path().join("java");
        let install_dir = java_root.join("zulu27");
        let home = install_dir.join("zulu27").join("Contents").join("Home");
        std::fs::create_dir_all(home.join("bin")).expect("mkdir");
        std::fs::write(
            home.join("bin").join(domain::java::java_exe_name()),
            b"stub",
        )
        .expect("stub");
        repo.upsert(&JavaRuntime {
            id: "zulu27".into(),
            path: home.clone(),
            major: JavaMajor::parse(27).expect("27"),
            version: "27.0.0".into(),
            vendor: JavaVendor::Zulu,
            managed: true,
            created_at: 1,
            updated_at: 1,
        })
        .expect("upsert");

        let outcome = service.uninstall(&home).expect("uninstall");

        assert_eq!(outcome, UninstallOutcome::RemovedManaged { path: home });
        assert!(!install_dir.exists(), "whole install dir must be gone");
        assert!(repo.list().expect("list").is_empty(), "record removed");
    }

    /// 先删文件再注销：安装目录删除失败时记录必须保留，退出占用后重试即可
    #[test]
    fn uninstall_managed_failure_keeps_record() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (service, repo) = svc(tmp.path());
        let java_root = tmp.path().join("java");
        std::fs::create_dir_all(&java_root).expect("mkdir");
        // 安装目录换成同名文件：remove_dir_all 必失败
        let install_dir = java_root.join("zulu27");
        std::fs::write(&install_dir, "occupied").expect("file");
        let home = install_dir.join("zulu27").join("Contents").join("Home");
        repo.upsert(&JavaRuntime {
            id: "zulu27".into(),
            path: home.clone(),
            major: JavaMajor::parse(27).expect("27"),
            version: "27.0.0".into(),
            vendor: JavaVendor::Zulu,
            managed: true,
            created_at: 1,
            updated_at: 1,
        })
        .expect("upsert");

        let err = service.uninstall(&home).expect_err("must fail");
        assert!(matches!(err, JavaError::Fs(FsError::Io { .. })));
        // 记录还在，解除占用后重试即可自愈
        assert!(repo.get_by_path(&home).is_ok(), "record kept for retry");
    }

    #[tokio::test]
    async fn install_registers_managed() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (service, _repo) = svc(tmp.path());
        let cancel = CancellationToken::new();
        let outcome = service
            .install(
                JavaMajor::parse(21).expect("21"),
                JavaPackageType::Jdk,
                None,
                None,
                None,
                &cancel,
            )
            .await
            .expect("install");
        assert!(outcome.runtime.managed);
        assert!(outcome.runtime.path.starts_with(tmp.path().join("java")));
        assert_eq!(outcome.artifact.size, 4);
        let managed: Vec<_> = service
            .list()
            .expect("list")
            .into_iter()
            .filter(|rt| rt.managed)
            .collect();
        assert_eq!(managed.len(), 1);
        assert_eq!(managed[0].id, "temurin-21");
    }

    /// `java_root` 下的全部残留名（失败清理类断言用）
    fn java_root_entries(tmp: &Path) -> Vec<String> {
        std::fs::read_dir(tmp.join("java"))
            .expect("read java_root")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect()
    }

    /// 下载失败 / Ctrl-C 取消：本次新建的安装目录整体回收，不留空壳
    #[tokio::test]
    async fn install_download_failure_removes_new_dir() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (service, _repo) = svc_with(tmp.path(), Arc::new(FailingDownloader));
        let cancel = CancellationToken::new();

        let err = service
            .install(
                JavaMajor::parse(21).expect("21"),
                JavaPackageType::Jdk,
                None,
                None,
                None,
                &cancel,
            )
            .await
            .expect_err("must fail");

        assert!(matches!(
            err,
            JavaError::Download(port::DownloadError::Cancelled)
        ));
        let leftovers = java_root_entries(tmp.path());
        assert!(leftovers.is_empty(), "leftovers: {leftovers:?}");
    }

    /// 解压校验失败：已下载的归档（可能数百 MB）连同新建目录一起回收
    #[tokio::test]
    async fn install_extract_failure_removes_archive_and_new_dir() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (service, _repo) = svc_with(tmp.path(), Arc::new(ArchiveOnlyDownloader));
        let cancel = CancellationToken::new();

        let err = service
            .install(
                JavaMajor::parse(21).expect("21"),
                JavaPackageType::Jdk,
                None,
                None,
                None,
                &cancel,
            )
            .await
            .expect_err("must fail");

        assert!(matches!(err, JavaError::Fs(FsError::Io { .. })));
        let leftovers = java_root_entries(tmp.path());
        assert!(leftovers.is_empty(), "leftovers: {leftovers:?}");
    }

    /// 重装失败：目录本就存在（旧运行时在里面），只回收本次归档，目录原样保留
    #[tokio::test]
    async fn install_extract_failure_keeps_preexisting_dir() {
        let tmp = tempfile::tempdir().expect("tmp");
        let java_root = tmp.path().join("java");
        let install_dir = java_root.join("temurin-21");
        let sentinel = install_dir.join("keep.txt");
        std::fs::create_dir_all(&install_dir).expect("mkdir");
        std::fs::write(&sentinel, b"old install").expect("sentinel");

        let (service, _repo) = svc_with(tmp.path(), Arc::new(ArchiveOnlyDownloader));
        let cancel = CancellationToken::new();
        let err = service
            .install(
                JavaMajor::parse(21).expect("21"),
                JavaPackageType::Jdk,
                None,
                None,
                None,
                &cancel,
            )
            .await
            .expect_err("must fail");

        assert!(matches!(err, JavaError::Fs(FsError::Io { .. })));
        assert!(
            sentinel.exists(),
            "existing install must survive a failed reinstall"
        );
        assert!(
            !install_dir.join("java-bin").exists(),
            "partial archive must be cleaned"
        );
    }

    /// 聚合注册表永远不返回（网络挂起）：取消必须在轮询粒度内生效，
    /// 而不是等网络 future 返回——回归时表现为 5s 超时而非正常返回
    #[tokio::test]
    async fn install_aborts_promptly_while_registry_hangs() {
        struct HangingRegistry;

        #[async_trait]
        impl JavaSourceRegistryPort for HangingRegistry {
            fn source_names(&self) -> Vec<&'static str> {
                vec!["hang"]
            }

            async fn ranked(&self) -> Arc<dyn JavaSourceRegistryPort> {
                Arc::new(HangingRegistry)
            }

            async fn list_packages(
                &self,
                _request: &JavaPackageRequest,
            ) -> Result<Vec<port::JavaPackage>, SourceError> {
                std::future::pending().await
            }

            async fn resolve_binary(
                &self,
                _package: &port::JavaPackage,
            ) -> Result<port::ResolvedJava, SourceError> {
                std::future::pending().await
            }
        }

        let tmp = tempfile::tempdir().expect("tmp");
        let (service, _repo) = svc_parts(
            tmp.path(),
            Arc::new(NoopDownloader),
            Arc::new(HangingRegistry),
        );
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            token.cancel();
        });

        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            service.install(
                JavaMajor::parse(21).expect("21"),
                JavaPackageType::Jdk,
                None,
                None,
                None,
                &cancel,
            ),
        )
        .await;
        let err = outcome
            .expect("cancel must abort the hanging registry, not wait for it")
            .expect_err("must be cancelled");
        assert!(matches!(
            err,
            JavaError::Download(port::DownloadError::Cancelled)
        ));
    }

    /// 在解析阶段触发取消：不发起下载，新建目录整体回收
    #[tokio::test]
    async fn install_cancels_between_resolve_and_download() {
        struct CancelOnResolve {
            cancel: CancellationToken,
        }

        #[async_trait]
        impl JavaSourceRegistryPort for CancelOnResolve {
            fn source_names(&self) -> Vec<&'static str> {
                vec!["fake"]
            }

            async fn ranked(&self) -> Arc<dyn JavaSourceRegistryPort> {
                Arc::new(CancelOnResolve {
                    cancel: self.cancel.clone(),
                })
            }

            async fn list_packages(
                &self,
                request: &JavaPackageRequest,
            ) -> Result<Vec<port::JavaPackage>, SourceError> {
                Ok(vec![port::JavaPackage {
                    id: "pkg1".into(),
                    vendor: JavaVendor::Temurin,
                    major: request.major,
                    version: format!("{}.0.5", request.major),
                    package_type: request.package_type,
                }])
            }

            async fn resolve_binary(
                &self,
                package: &port::JavaPackage,
            ) -> Result<port::ResolvedJava, SourceError> {
                self.cancel.cancel();
                Ok(port::ResolvedJava {
                    package: package.clone(),
                    location: url::Url::parse("https://example.com/java-bin").expect("url"),
                    expected_size: None,
                })
            }
        }

        let tmp = tempfile::tempdir().expect("tmp");
        let cancel = CancellationToken::new();
        let (service, _repo) = svc_parts(
            tmp.path(),
            Arc::new(NoopDownloader),
            Arc::new(CancelOnResolve {
                cancel: cancel.clone(),
            }),
        );

        let err = service
            .install(
                JavaMajor::parse(21).expect("21"),
                JavaPackageType::Jdk,
                None,
                None,
                None,
                &cancel,
            )
            .await
            .expect_err("must be cancelled");
        assert!(matches!(
            err,
            JavaError::Download(port::DownloadError::Cancelled)
        ));
        let leftovers = java_root_entries(tmp.path());
        assert!(leftovers.is_empty(), "leftovers: {leftovers:?}");
    }

    /// 取消落在下载完成与解压之间：不启动解压，归档与新建目录一并回收
    #[tokio::test]
    async fn install_cancels_before_extract() {
        struct CancelOnDownload {
            cancel: CancellationToken,
        }

        #[async_trait]
        impl ArtifactDownloader for CancelOnDownload {
            async fn download(
                &self,
                request: &DownloadRequest,
                _progress: Option<&dyn ProgressSink>,
                _cancel: &CancellationToken,
            ) -> Result<DownloadedArtifact, port::DownloadError> {
                std::fs::write(&request.destination, b"stub").map_err(port::DownloadError::Io)?;
                self.cancel.cancel();
                Ok(DownloadedArtifact {
                    path: request.destination.clone(),
                    size: 4,
                })
            }
        }

        let tmp = tempfile::tempdir().expect("tmp");
        let cancel = CancellationToken::new();
        let (service, _repo) = svc_parts(
            tmp.path(),
            Arc::new(CancelOnDownload {
                cancel: cancel.clone(),
            }),
            Arc::new(FakeRegistry),
        );

        let err = service
            .install(
                JavaMajor::parse(21).expect("21"),
                JavaPackageType::Jdk,
                None,
                None,
                None,
                &cancel,
            )
            .await
            .expect_err("must be cancelled");
        assert!(matches!(
            err,
            JavaError::Download(port::DownloadError::Cancelled)
        ));
        let leftovers = java_root_entries(tmp.path());
        assert!(leftovers.is_empty(), "leftovers: {leftovers:?}");
    }

    /// 强杀（断电 / kill -9）残留按年龄回收：够老才删，新鲜条目与正常安装不碰
    #[test]
    fn sweep_stale_temp_removes_only_stale() {
        let tmp = tempfile::tempdir().expect("tmp");
        let java_root = tmp.path().join("java");
        std::fs::create_dir_all(&java_root).expect("mkdir");

        let stale = java_root.join(".partial.zip.123.1.tmp");
        std::fs::write(&stale, b"half").expect("stale file");
        let handle = std::fs::File::options()
            .write(true)
            .open(&stale)
            .expect("open");
        handle
            .set_modified(SystemTime::now() - STALE_TEMP_AGE - Duration::from_secs(60))
            .expect("backdate");

        let fresh = java_root.join(".fresh.zip.124.2.tmp");
        std::fs::write(&fresh, b"half").expect("fresh file");
        let live_tmp = java_root.join(".tmp-zulu-27-20260101");
        std::fs::create_dir_all(&live_tmp).expect("tmp dir");
        let install = java_root.join("zulu-27");
        std::fs::create_dir_all(&install).expect("install dir");

        sweep_stale_temp(&java_root);

        assert!(!stale.exists(), "stale download temp must be removed");
        assert!(fresh.exists(), "fresh temp must survive");
        assert!(live_tmp.exists(), "fresh extraction temp must survive");
        assert!(install.exists(), "managed install must survive");
    }

    /// 目录名用「厂商-大版本」：短、稳定，JRE 加后缀避免与 JDK 同址
    #[test]
    fn install_dir_name_is_short_and_stable() {
        let jdk = port::JavaPackage {
            id: "https://example.com/zulu27.28.101-ca-jdk27.0.0-win_x64.zip".into(),
            vendor: JavaVendor::Zulu,
            major: JavaMajor::parse(27).expect("27"),
            version: "27.0.0".into(),
            package_type: JavaPackageType::Jdk,
        };
        assert_eq!(install_dir_name(&jdk), "zulu-27");
        let jre = port::JavaPackage {
            package_type: JavaPackageType::Jre,
            ..jdk
        };
        assert_eq!(install_dir_name(&jre), "zulu-27-jre");
    }

    #[test]
    fn locate_java_home_finds_nested_home() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("jdk-21");
        std::fs::create_dir_all(home.join("bin")).expect("bin");
        std::fs::write(
            home.join("bin").join(domain::java::java_exe_name()),
            b"stub",
        )
        .expect("java stub");
        assert_eq!(locate_java_home(tmp.path()), Some(home));
    }

    #[test]
    fn locate_java_home_finds_root_home() {
        let tmp = tempfile::tempdir().expect("tmp");
        std::fs::create_dir_all(tmp.path().join("bin")).expect("bin");
        std::fs::write(
            tmp.path().join("bin").join(domain::java::java_exe_name()),
            b"stub",
        )
        .expect("java stub");
        assert_eq!(locate_java_home(tmp.path()), Some(tmp.path().to_path_buf()));
    }

    /// 下错平台的包：目录里只有与当前平台不符的 java 文件名，视为不可用
    #[test]
    fn require_jvm_home_rejects_foreign_platform_tree() {
        let dir = tempfile::tempdir().expect("tmp");
        let bin = dir.path().join("jdk-27").join("bin");
        std::fs::create_dir_all(&bin).expect("bin");
        let foreign = if cfg!(target_os = "windows") {
            "java"
        } else {
            "java.exe"
        };
        std::fs::write(bin.join(foreign), b"stub").expect("stub");

        let archive = dir.path().join("foreign.zip");
        let err = require_jvm_home(&archive, dir.path()).expect_err("must fail");
        assert!(matches!(err, JavaError::Fs(FsError::Io { .. })));
    }

    #[test]
    fn require_jvm_home_accepts_current_platform_tree() {
        let dir = tempfile::tempdir().expect("tmp");
        let bin = dir.path().join("jdk-27").join("bin");
        std::fs::create_dir_all(&bin).expect("bin");
        std::fs::write(bin.join(domain::java::java_exe_name()), b"stub").expect("stub");

        let archive = dir.path().join("ok.zip");
        let home = require_jvm_home(&archive, dir.path()).expect("home");
        assert_eq!(home, dir.path().join("jdk-27"));
    }
}
