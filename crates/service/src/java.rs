use domain::java::{JavaMajor, JavaPackageType, JavaRuntime, JavaVendor};
use port::{
    ArtifactDownloader, CancellationToken, DownloadRequest, DownloadedArtifact,
    JavaRuntimeRepository, JavaScanner, ProgressSink, RepositoryError, ScannedJava, SourceError,
};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tracing::info;

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
    /// 本地 IO 失败
    /// 文件系统失败；原因走 source 链，避免 `{:#}` 渲染时重复
    #[error("io error on {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// 扫描失败
    #[error("scan failed: {0}")]
    Scan(String),
}

/// 扫描结果汇总
#[derive(Debug, Clone)]
pub struct ScanOutcome {
    /// 新注册条数
    pub registered: usize,
    /// 跳过（已存在且未变）条数
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
        }
    }
}

/// Java 生命周期编排：扫描、安装、卸载、列表
pub struct JavaService {
    repo: Arc<dyn JavaRuntimeRepository>,
    scanner: Arc<dyn JavaScanner>,
    registry: Arc<dyn JavaSourceRegistryPort>,
    downloader: Arc<dyn ArtifactDownloader>,
    /// mcsup 管理的 JVM 安装根目录 `{data_dir}/mcsup/java`
    java_root: PathBuf,
}

/// 对 [`port::JavaReleaseSource`] 集合的封装（service 只依赖聚合接口）
#[async_trait::async_trait]
pub trait JavaSourceRegistryPort: Send + Sync {
    /// 源名列表（优先级）
    fn source_names(&self) -> Vec<&'static str>;
    /// 列出可用包
    async fn list_packages(
        &self,
        major: JavaMajor,
        package_type: JavaPackageType,
    ) -> Result<Vec<port::JavaPackage>, SourceError>;
    /// 解析下载地址
    async fn resolve_binary(
        &self,
        package: &port::JavaPackage,
    ) -> Result<port::ResolvedJava, SourceError>;
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
        java_root: PathBuf,
    ) -> Self {
        Self {
            repo,
            scanner,
            registry,
            downloader,
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
        let now = unix_now();
        let mut registered = 0usize;
        let mut skipped = 0usize;
        let mut seen = std::collections::HashSet::new();
        self.scanner
            .scan(&mut |found| {
                if !seen.insert(found.path.clone()) {
                    return;
                }
                on_found(&found);
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
                    Ok(()) => registered += 1,
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
        if !self.has_scanned()? {
            self.scan(false, &mut |_| {})?;
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

        notify(InstallStage::Listing);
        let packages = self.registry.list_packages(major, package_type).await?;
        let package = packages
            .into_iter()
            .find(|pkg| vendor.is_none_or(|v| pkg.vendor == v))
            .ok_or_else(|| {
                JavaError::Source(SourceError::NotFound(
                    domain::server::version::VersionId::new(major.to_string()),
                ))
            })?;

        notify(InstallStage::Resolving);
        let resolved = self.registry.resolve_binary(&package).await?;

        // id 可能是 URL（adoptium），净化为安全目录名
        let dir_name = sanitize_dir_name(&package.id, &package);
        let install_dir = self.java_root.join(&dir_name);
        std::fs::create_dir_all(&install_dir).map_err(|source| JavaError::Io {
            path: install_dir.clone(),
            source,
        })?;
        let filename = download_filename(&resolved.location);
        let destination = install_dir.join(&filename);

        notify(InstallStage::Downloading);
        let request = DownloadRequest {
            location: resolved.location,
            destination: destination.clone(),
            expected: None,
            expected_size: resolved.expected_size,
        };
        let artifact = self.downloader.download(&request, progress, cancel).await?;

        notify(InstallStage::Extracting);
        let home = extract_and_swap(&destination, &install_dir)?;

        let now = unix_now();
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

    /// 卸载：managed 删文件+记录，系统只删记录
    ///
    /// # Errors
    ///
    /// 未注册返回 [`JavaError::NotFound`]；IO 或注册表失败返回对应错误
    pub fn uninstall(&self, path: &Path) -> Result<UninstallOutcome, JavaError> {
        let runtime = self.repo.get_by_path(path).map_err(|err| match err {
            RepositoryError::NotFound(key) => JavaError::NotFound(key),
            other => JavaError::Repo(other),
        })?;
        self.repo.remove(path)?;
        if runtime.managed {
            if runtime.path.starts_with(&self.java_root) {
                let _ = std::fs::remove_dir_all(&runtime.path);
            }
            info!(path = %runtime.path.display(), "managed java removed");
            Ok(UninstallOutcome::RemovedManaged { path: runtime.path })
        } else {
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

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

fn download_filename(location: &url::Url) -> String {
    location
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .filter(|s| !s.is_empty())
        .map_or_else(|| "java-archive".into(), str::to_owned)
}

/// 把 package id 净化为安全目录名；URL 只取末段文件名，剥掉归档扩展名
fn sanitize_dir_name(raw: &str, package: &port::JavaPackage) -> String {
    let base = raw
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("java");
    // 去掉 query / fragment 残留
    let base = base.split(['?', '#']).next().unwrap_or(base);
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let stem = strip_archive_ext(&cleaned);
    if stem.is_empty() || stem == "." || stem == ".." {
        format!("{}-{}", package.vendor.as_str(), package.major)
    } else {
        stem
    }
}

/// 剥掉常见归档扩展名，得到干净的目录名
fn strip_archive_ext(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    for suffix in [".tar.gz", ".tgz", ".zip", ".dmg", ".msi", ".pkg", ".exe"] {
        if lower.ends_with(suffix) {
            return name[..name.len() - suffix.len()].to_owned();
        }
    }
    name.to_owned()
}

/// 在解压目录中定位 JVM home：优先 Contents/Home，否则找含 bin/java 的一层子目录
///
/// 判定走 [`domain::java::is_jvm_home`]：Windows 上要找 `bin\java.exe`，
/// 只认 `bin/java` 会把安装目录根当成 home。
fn locate_java_home(root: &Path) -> Option<PathBuf> {
    let contents_home = root.join("Contents").join("Home");
    if domain::java::is_jvm_home(&contents_home) {
        return Some(contents_home);
    }
    if domain::java::is_jvm_home(root) {
        return Some(root.to_path_buf());
    }
    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if domain::java::is_jvm_home(&path) {
            return Some(path);
        }
        let nested_home = path.join("Contents").join("Home");
        if domain::java::is_jvm_home(&nested_home) {
            return Some(nested_home);
        }
    }
    None
}

/// 校验解压产物里有当前平台可用的 JVM home
///
/// 下错平台 / 架构的归档（例如在 Windows 上装到 macOS 包）解压不会报错，
/// 但产物里没有 `bin/java.exe`，跑不起来。必须在这里失败，而不是注册成运行时。
///
/// # Errors
///
/// 找不到 JVM home 时返回 [`JavaError::Io`]，消息点明可能是平台不符
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

/// 解压归档到临时目录，成功后原子替换目标目录
///
/// 避免重复安装时 `tar`/`unzip` 对已存在文件逐个询问覆盖。
/// 解压失败时保留原目录，清理临时目录。
/// 非压缩格式（dmg/msi）不解压，直接返回目标目录（归档已在其中）。
fn extract_and_swap(archive: &Path, into: &Path) -> Result<PathBuf, JavaError> {
    // 非 tar.gz / zip：不走临时目录（归档文件本身在 into 里，不能删）
    if !is_gzip(archive) && !has_ext(archive, "zip") {
        return Ok(into.to_path_buf());
    }

    let parent = into.parent().unwrap_or(into);
    let name = into.file_name().unwrap_or_default().to_string_lossy();
    let tmp = parent.join(format!(".tmp-{name}"));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|source| JavaError::Io {
        path: tmp.clone(),
        source,
    })?;

    let extract_result = extract_archive(archive, &tmp);
    if let Err(err) = extract_result {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(err);
    }

    // 解压成功但没有可用 home：多半是下错平台 / 架构的包（如 Windows 上装到 macOS 包），
    // 这种产物跑不起来，必须在替换与注册前拦下
    if let Err(err) = require_jvm_home(archive, &tmp) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(err);
    }

    // 归档已在 tmp 中解压完毕，可安全删除旧目录并替换
    if into.exists() {
        std::fs::remove_dir_all(into).map_err(|source| JavaError::Io {
            path: into.to_path_buf(),
            source,
        })?;
    }
    std::fs::rename(&tmp, into).map_err(|source| JavaError::Io {
        path: into.to_path_buf(),
        source,
    })?;
    Ok(locate_java_home(into).unwrap_or_else(|| into.to_path_buf()))
}

/// 解压归档到目录；不支持的格式原样保留并返回归档所在目录
fn extract_archive(archive: &Path, into: &Path) -> Result<PathBuf, JavaError> {
    if is_gzip(archive) {
        untar_gz(archive, into)?;
    } else if has_ext(archive, "zip") {
        unzip(archive, into)?;
    } else {
        // dmg / msi 等安装包：不自动解压，路径指向归档目录
        return Ok(into.to_path_buf());
    }
    Ok(into.to_path_buf())
}

fn has_ext(path: &Path, expected: &str) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case(expected))
}

#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn is_gzip(path: &Path) -> bool {
    let Some(name) = path.file_name() else {
        return false;
    };
    let name = name.to_string_lossy().to_ascii_lowercase();
    name.ends_with(".tar.gz") || name.ends_with(".tgz")
}

/// `.tar.gz` 解压：系统自带 `tar`（macOS / Linux 常驻，Windows 10 1803+ 内置）
fn untar_gz(archive: &Path, into: &Path) -> Result<(), JavaError> {
    let extractor = Extractor::new(
        OsStr::new("tar"),
        &[
            OsStr::new("-xzf"),
            archive.as_os_str(),
            OsStr::new("-C"),
            into.as_os_str(),
        ],
    );
    match run_extractor(&extractor) {
        Ok(()) => Ok(()),
        Err(ExtractAttempt::Missing) => Err(java_io_err(
            archive,
            "tar not found; install tar to extract .tar.gz".to_owned(),
        )),
        Err(ExtractAttempt::Failed(message)) => Err(java_io_err(archive, message)),
    }
}

/// `.zip` 解压：按平台给出候选工具依次尝试
///
/// 各平台自带的解压工具不同（Windows 无 `unzip`，Linux 无 `ditto`），
/// 只调用其中一个必然在某个平台报 `program not found`。
fn unzip(archive: &Path, into: &Path) -> Result<(), JavaError> {
    let candidates = zip_extractors(archive, into);
    let mut failure: Option<String> = None;
    for extractor in &candidates {
        match run_extractor(extractor) {
            Ok(()) => return Ok(()),
            // 本机没有这个程序：换下一个候选
            Err(ExtractAttempt::Missing) => {}
            Err(ExtractAttempt::Failed(message)) => {
                if failure.is_none() {
                    // 保留首选工具的失败原因，后续候选只用于覆盖「工具缺失」
                    failure = Some(message);
                }
            }
        }
    }
    let source = failure.unwrap_or_else(|| {
        let tried = candidates
            .iter()
            .map(|extractor| extractor.program.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(", ");
        format!("no zip extractor available (tried: {tried})")
    });
    Err(java_io_err(archive, source))
}

/// zip 的候选解压命令，按平台排列本机最可能可用的工具
fn zip_extractors(archive: &Path, into: &Path) -> Vec<Extractor> {
    let archive = archive.as_os_str();
    let into = into.as_os_str();

    #[cfg(target_os = "windows")]
    {
        vec![
            // Windows 10 1803+ 自带 bsdtar，直接解 zip
            Extractor::new(
                OsStr::new("tar"),
                &[OsStr::new("-xf"), archive, OsStr::new("-C"), into],
            ),
            // 老系统回退 PowerShell 的 Expand-Archive；路径走环境变量，避开引号转义
            Extractor {
                program: OsString::from("powershell"),
                args: vec![
                    OsString::from("-NoProfile"),
                    OsString::from("-NonInteractive"),
                    OsString::from("-Command"),
                    OsString::from(POWERSHELL_EXPAND_ARCHIVE),
                ],
                envs: vec![
                    (OsString::from("MCSUP_ZIP_SRC"), archive.to_os_string()),
                    (OsString::from("MCSUP_ZIP_DST"), into.to_os_string()),
                ],
            },
        ]
    }

    #[cfg(target_os = "macos")]
    {
        vec![
            Extractor::new(
                OsStr::new("unzip"),
                &[OsStr::new("-q"), archive, OsStr::new("-d"), into],
            ),
            // macOS 系统自带，能处理带资源分叉的 zip
            Extractor::new(
                OsStr::new("ditto"),
                &[OsStr::new("-x"), OsStr::new("-k"), archive, into],
            ),
            Extractor::new(
                OsStr::new("tar"),
                &[OsStr::new("-xf"), archive, OsStr::new("-C"), into],
            ),
        ]
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        vec![
            Extractor::new(
                OsStr::new("unzip"),
                &[OsStr::new("-q"), archive, OsStr::new("-d"), into],
            ),
            Extractor::new(
                OsStr::new("tar"),
                &[OsStr::new("-xf"), archive, OsStr::new("-C"), into],
            ),
        ]
    }
}

/// `PowerShell` 解压脚本：路径经环境变量传入，不进命令行
///
/// 只有 Windows 的候选解压链用它，其他目标下不跟着引用方一起裁掉就是死代码
#[cfg(target_os = "windows")]
const POWERSHELL_EXPAND_ARCHIVE: &str =
    "Expand-Archive -LiteralPath $env:MCSUP_ZIP_SRC -DestinationPath $env:MCSUP_ZIP_DST -Force";

/// 外部解压命令
struct Extractor {
    /// 程序名
    program: OsString,
    /// 参数
    args: Vec<OsString>,
    /// 环境变量
    envs: Vec<(OsString, OsString)>,
}

impl Extractor {
    /// 组装无环境变量的命令
    fn new(program: &OsStr, args: &[&OsStr]) -> Self {
        Self {
            program: program.to_os_string(),
            args: args.iter().map(|arg| (*arg).to_os_string()).collect(),
            envs: Vec::new(),
        }
    }
}

/// [`run_extractor`] 的一次尝试结果
#[derive(Debug)]
enum ExtractAttempt {
    /// 本机没有该程序，可换下一个候选
    Missing,
    /// 程序存在但执行失败
    Failed(String),
}

/// 执行解压命令；程序不存在与执行失败分开上报，前者驱动候选回退
fn run_extractor(extractor: &Extractor) -> Result<(), ExtractAttempt> {
    let mut command = std::process::Command::new(&extractor.program);
    command.args(&extractor.args);
    for (key, value) in &extractor.envs {
        command.env(key, value);
    }
    let status = command.status().map_err(|source| match source.kind() {
        std::io::ErrorKind::NotFound => ExtractAttempt::Missing,
        _ => ExtractAttempt::Failed(format!("{}: {source}", extractor.program.to_string_lossy())),
    })?;
    if !status.success() {
        return Err(ExtractAttempt::Failed(format!(
            "{} exited with {status}",
            extractor.program.to_string_lossy()
        )));
    }
    Ok(())
}

/// 构造以归档路径为上下文的 IO 错误
fn java_io_err(path: &Path, source: String) -> JavaError {
    JavaError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::other(source),
    }
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

        async fn list_packages(
            &self,
            major: JavaMajor,
            package_type: JavaPackageType,
        ) -> Result<Vec<port::JavaPackage>, SourceError> {
            Ok(vec![port::JavaPackage {
                id: "pkg1".into(),
                vendor: JavaVendor::Temurin,
                major,
                version: format!("{major}.0.5"),
                package_type,
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
            std::fs::write(&request.destination, b"stub").map_err(port::DownloadError::Io)?;
            Ok(DownloadedArtifact {
                path: request.destination.clone(),
                size: 4,
            })
        }
    }

    fn svc(tmp: &Path) -> (JavaService, Arc<MemRepo>) {
        let repo = Arc::new(MemRepo::new());
        let scanner = Arc::new(FakeScanner {
            homes: vec![tmp.join("jdk21")],
        });
        let registry = Arc::new(FakeRegistry);
        let downloader = Arc::new(NoopDownloader);
        let java_root = tmp.join("java");
        std::fs::create_dir_all(&java_root).expect("mkdir");
        (
            JavaService::new(repo.clone(), scanner, registry, downloader, java_root),
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
        assert_eq!(forced.registered, 1);
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
        assert_eq!(managed[0].id, "pkg1");
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

    #[test]
    fn zip_extractors_start_with_platform_tool() {
        let candidates = zip_extractors(Path::new("a.zip"), Path::new("out"));
        assert!(!candidates.is_empty());
        #[cfg(target_os = "windows")]
        assert_eq!(candidates[0].program, OsString::from("tar"));
        #[cfg(not(target_os = "windows"))]
        assert_eq!(candidates[0].program, OsString::from("unzip"));
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
        assert!(matches!(err, JavaError::Io { .. }));
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

    #[test]
    fn missing_program_is_reported_for_fallback() {
        let extractor = Extractor::new(OsStr::new("mcsup-no-such-extractor"), &[]);
        assert!(matches!(
            run_extractor(&extractor),
            Err(ExtractAttempt::Missing)
        ));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn failing_program_is_reported_as_failure() {
        let extractor =
            Extractor::new(OsStr::new("cmd"), &[OsStr::new("/C"), OsStr::new("exit 3")]);
        assert!(matches!(
            run_extractor(&extractor),
            Err(ExtractAttempt::Failed(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn failing_program_is_reported_as_failure() {
        let extractor = Extractor::new(OsStr::new("sh"), &[OsStr::new("-c"), OsStr::new("exit 3")]);
        assert!(matches!(
            run_extractor(&extractor),
            Err(ExtractAttempt::Failed(_))
        ));
    }
}
