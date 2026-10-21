use domain::java::{JavaMajor, JavaPackageType, JavaRuntime, JavaVendor};
use domain::server::artifact::{Checksum, JarArtifact};
use domain::server::meta::{InstalledJar, ServerMeta, ServerName};
use domain::server::version::{ServerVersion, VersionId};
use domain::workspace::{Workspace, WorkspaceId, WorkspaceName};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use thiserror::Error;
use url::Url;

/// 获取服务端资源时的错误
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SourceError {
    /// 目标版本不存在
    #[error("version not found: {0}")]
    NotFound(VersionId),
    /// 上游返回非成功状态码
    #[error("unexpected HTTP status: {status}")]
    Status { status: u16 },
    /// 传输层失败，连接、超时、状态码，通常可重试
    #[error("network error: {0}")]
    Network(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// 响应无法解析，通常不可重试
    #[error("failed to decode upstream response: {0}")]
    Decode(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// 初始化失败
    #[error("failed to initialize source: {0}")]
    Init(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl SourceError {
    #[must_use]
    pub fn retryable(&self) -> bool {
        match self {
            Self::Status { status } => *status == 429 || *status >= 500,
            Self::Network(_) => true,
            Self::NotFound(_) | Self::Decode(_) | Self::Init(_) => false,
        }
    }
}

/// 待下载的内容和URL
#[derive(Debug, Clone)]
pub struct ResolvedArtifact {
    pub artifact: JarArtifact,
    pub location: Url,
}

/// 服务端 JAR 来源
#[async_trait::async_trait]
pub trait ServerJarSource: Send + Sync {
    /// 列出可用版本
    ///
    /// #Errors
    ///
    /// 上游不可达、返回异常状态码或响应无法解析时返回 [`SourceError`]
    async fn versions(&self) -> Result<Vec<ServerVersion>, SourceError>;
    /// 解析某版本的服务器 JAR 下载信息
    ///
    /// # Errors
    ///
    /// 版本不存在返回 [`SourceError::NotFound`]; 其余同上
    async fn artifact(&self, version: &VersionId) -> Result<ResolvedArtifact, SourceError>;
}

/// 一次下载请求
#[derive(Debug, Clone)]
pub struct DownloadRequest {
    /// 字节来源地址
    pub location: Url,
    /// 最终保存路径
    pub destination: PathBuf,
    /// 期望的校验和；为 `None` 时跳过校验
    pub expected: Option<Checksum>,
    /// 期望的字节数; 为 `None` 时跳过大小检验
    pub expected_size: Option<u64>,
}

/// 下载进度观察者
pub trait ProgressSink: Send + Sync {
    /// 已传输字节与总量；总量未知时为 `None`
    fn on_progress(&self, transferred: u64, total: Option<u64>);
}

/// 协作式取消令牌
#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    flag: Arc<AtomicBool>,
}

impl CancellationToken {
    /// 新建未取消的令牌
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 请求取消
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    /// 是否已请求取消
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

/// 下载成功的结果
#[derive(Debug, Clone)]
pub struct DownloadedArtifact {
    /// 路径
    pub path: PathBuf,
    /// 实际写入的字节数
    pub size: u64,
}

/// 下载过程中的错误
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DownloadError {
    /// 来源传输失败
    #[error(transparent)]
    Source(#[from] SourceError),
    /// 本地 IO 失败
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// 校验和不匹配
    #[error("checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch { expected: String, actual: String },
    /// 字节数与期望不符
    #[error("size mismatch: expected {expected} bytes, got {actual}")]
    SizeMismatch { expected: u64, actual: u64 },
    /// 不支持的检验算法
    #[error("unsupported checksum algorithm")]
    UnsupportedAlgorithm,
    /// 调用方法请求取消
    #[error("download cancelled")]
    Cancelled,
    /// 超时
    #[error("download timed out")]
    Timeout,
}

impl DownloadError {
    /// 是否可重试
    #[must_use]
    pub fn retryable(&self) -> bool {
        match self {
            Self::Source(err) => err.retryable(),
            Self::Io(_) | Self::Timeout => true,
            Self::ChecksumMismatch { .. }
            | Self::SizeMismatch { .. }
            | Self::UnsupportedAlgorithm
            | Self::Cancelled => false,
        }
    }
}

/// 按 URL 打开一个可增量读取的响应体
#[async_trait::async_trait]
pub trait ByteSource: Send + Sync {
    /// 打开响应体
    ///
    /// # Errors
    ///
    /// 上游不可达或返回异常状态码时返回 [`SourceError`]
    async fn open(&self, location: &Url) -> Result<Box<dyn ByteReader>, SourceError>;
}

/// 可增量读取的响应体
#[async_trait::async_trait]
pub trait ByteReader: Send {
    /// 读取下一块数据，读到结尾时返回 `Ok(None)`
    ///
    /// # Errors
    ///
    /// 读取过程中传输中断时返回 [`SourceError`]
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, SourceError>;
}

/// 远端对象的元信息
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteStat {
    /// 对象总字节数
    pub size: u64,
    /// 是否声明支持按字节范围读取
    pub supports_range: bool,
}

/// 可按字节范围读取的来源
#[async_trait::async_trait]
pub trait RangeSource: Send + Sync {
    /// 读取远端元信息
    ///
    /// # Errors
    ///
    /// 上游不可达、返回异常状态码或缺少必要的响应头时返回 [`SourceError`]
    async fn stat(&self, location: &Url) -> Result<RemoteStat, SourceError>;

    /// 打开闭区间 `start..=end` 的响应体
    ///
    /// 实现要求上游以部分响应返回该区间；上游忽略范围时按 [`SourceError::Status`] 报告，由调用方判定回退
    ///
    /// # Errors
    ///
    /// 上游不可达、未按范围返回或状态码异常时返回 [`SourceError`]
    async fn open_range(
        &self,
        location: &Url,
        range: std::ops::RangeInclusive<u64>,
    ) -> Result<Box<dyn ByteReader>, SourceError>;
}

/// 下载器
#[async_trait::async_trait]
pub trait ArtifactDownloader: Send + Sync {
    /// 执行一次下载
    ///
    /// # Errors
    ///
    /// 传输、IO、校验失败或取消/超时时返回 [`DownloadError`]
    async fn download(
        &self,
        request: &DownloadRequest,
        progress: Option<&dyn ProgressSink>,
        cancel: &CancellationToken,
    ) -> Result<DownloadedArtifact, DownloadError>;
}

/// 归档解压失败（工具缺失 / 执行失败），消息自带工具与路径上下文
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{0}")]
pub struct ExtractError(pub String);

/// 归档解压能力
///
/// 解压是重活（外部工具链，将来的远端执行同样在此之后），不在 service 内
/// 做；service 只保留编排：临时目录 → 解压 → 校验 → 原子换入。
pub trait ArchiveExtractor: Send + Sync {
    /// 归档是否属于可解压格式
    ///
    /// dmg / msi 等安装包不解压，调用方按目录内容继续处理。
    fn is_extractable(&self, archive: &Path) -> bool;

    /// 把归档内容解到 `dest`；调用方负责临时目录与原子换入
    ///
    /// # Errors
    ///
    /// 工具缺失或解压失败时返回 [`ExtractError`]
    fn extract(&self, archive: &Path, dest: &Path) -> Result<(), ExtractError>;
}

/// 元数据存储错误
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RepositoryError {
    /// 同名服务或工作目录已存在
    #[error("already exists: {0}")]
    AlreadyExists(String),
    /// 服务或工作目录不存在
    #[error("not found: {0}")]
    NotFound(String),
    /// 记录内容损坏，无法还原为领域对象
    #[error("corrupt repository record: {0}")]
    Corrupt(String),
    /// 底层存储失败
    #[error("storage error: {0}")]
    Storage(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// 服务元数据存储
pub trait ServerRepository: Send + Sync {
    /// 写入新服务
    ///
    /// # Errors
    ///
    /// 名称冲突或同工作目录下 `rel_dir` 冲突返回 [`RepositoryError::AlreadyExists`]；
    /// 底层失败返回 [`RepositoryError::Storage`]
    fn create(&self, meta: &ServerMeta) -> Result<(), RepositoryError>;

    /// 按名称读取
    ///
    /// # Errors
    ///
    /// 不存在返回 [`RepositoryError::NotFound`]
    fn get(&self, name: &ServerName) -> Result<ServerMeta, RepositoryError>;

    /// 列出全部服务，按名称升序
    ///
    /// # Errors
    ///
    /// 底层失败返回 [`RepositoryError::Storage`]
    fn list(&self) -> Result<Vec<ServerMeta>, RepositoryError>;

    /// 回写核心元数据并刷新 `updated_at`，返回最新记录
    ///
    /// # Errors
    ///
    /// 不存在返回 [`RepositoryError::NotFound`]
    fn update_jar(
        &self,
        name: &ServerName,
        jar: &InstalledJar,
        updated_at: i64,
    ) -> Result<ServerMeta, RepositoryError>;

    /// 按名称注销单个服务器登记（不删磁盘）
    ///
    /// # Errors
    ///
    /// 不存在返回 [`RepositoryError::NotFound`]
    fn remove(&self, name: &ServerName) -> Result<(), RepositoryError>;

    /// 按工作目录删除全部服务器登记（注销工作目录时用）
    ///
    /// # Errors
    ///
    /// 底层失败返回 [`RepositoryError::Storage`]
    fn remove_by_workspace(&self, workspace_id: &WorkspaceId) -> Result<(), RepositoryError>;

    /// 按名称改挂到另一工作目录并更新 `rel_dir`（移动单服）
    ///
    /// # Errors
    ///
    /// 不存在返回 [`RepositoryError::NotFound`]，
    /// 目标冲突返回 [`RepositoryError::AlreadyExists`]
    fn relink(
        &self,
        name: &ServerName,
        workspace_id: &WorkspaceId,
        rel_dir: &Path,
        updated_at: i64,
    ) -> Result<ServerMeta, RepositoryError>;
}

/// 工作目录注册表存储
pub trait WorkspaceRepository: Send + Sync {
    /// 写入新工作目录
    ///
    /// # Errors
    ///
    /// 名称或路径冲突返回 [`RepositoryError::AlreadyExists`]；
    /// 底层失败返回 [`RepositoryError::Storage`]
    fn create(&self, ws: &Workspace) -> Result<(), RepositoryError>;

    /// 按身份读取
    ///
    /// # Errors
    ///
    /// 不存在返回 [`RepositoryError::NotFound`]
    fn get(&self, id: &WorkspaceId) -> Result<Workspace, RepositoryError>;

    /// 按名称读取
    ///
    /// # Errors
    ///
    /// 不存在返回 [`RepositoryError::NotFound`]
    fn get_by_name(&self, name: &WorkspaceName) -> Result<Workspace, RepositoryError>;

    /// 全部工作目录，按名称升序
    ///
    /// # Errors
    ///
    /// 底层失败返回 [`RepositoryError::Storage`]
    fn list(&self) -> Result<Vec<Workspace>, RepositoryError>;

    /// 当前工作区；尚无注册时为 `None`
    ///
    /// # Errors
    ///
    /// 底层失败返回 [`RepositoryError::Storage`]
    fn current(&self) -> Result<Option<Workspace>, RepositoryError>;

    /// 设为当前并清除其他当前标记（原子）
    ///
    /// # Errors
    ///
    /// 不存在返回 [`RepositoryError::NotFound`]
    fn set_current(&self, id: &WorkspaceId) -> Result<(), RepositoryError>;

    /// 注销（不删磁盘）
    ///
    /// # Errors
    ///
    /// 不存在返回 [`RepositoryError::NotFound`]
    fn remove(&self, id: &WorkspaceId) -> Result<(), RepositoryError>;

    /// 移动后改写路径并刷新 `updated_at`
    ///
    /// # Errors
    ///
    /// 不存在返回 [`RepositoryError::NotFound`]
    fn relink_path(
        &self,
        id: &WorkspaceId,
        path: &Path,
        updated_at: i64,
    ) -> Result<(), RepositoryError>;
}

/// Java 发行包查询条件
#[derive(Debug, Clone)]
pub struct JavaPackageRequest {
    /// 目标大版本
    pub major: JavaMajor,
    /// 包类型
    pub package_type: JavaPackageType,
    /// 目标操作系统标识（如 `mac`、`linux`、`windows`）
    pub os: String,
    /// 目标架构标识（如 `aarch64`、`x64`）
    pub arch: String,
}

impl JavaPackageRequest {
    /// 当前平台（OS / 架构）的查询条件
    #[must_use]
    pub fn for_current_platform(major: JavaMajor, package_type: JavaPackageType) -> Self {
        Self {
            major,
            package_type,
            os: current_os().to_owned(),
            arch: current_arch().to_owned(),
        }
    }
}

/// 当前平台的 API 操作系统标识
const fn current_os() -> &'static str {
    if cfg!(target_os = "macos") {
        "mac"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    }
}

/// 当前平台的 API 架构标识
const fn current_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x64"
    }
}

/// 远端一个可下载的 Java 发行包
#[derive(Debug, Clone)]
pub struct JavaPackage {
    /// 源内唯一标识
    pub id: String,
    pub vendor: JavaVendor,
    pub major: JavaMajor,
    /// 完整版本串
    pub version: String,
    pub package_type: JavaPackageType,
}

/// 解析后的可下载二进制
#[derive(Debug, Clone)]
pub struct ResolvedJava {
    pub package: JavaPackage,
    pub location: Url,
    /// 期望字节数；`None` 跳过校验
    pub expected_size: Option<u64>,
}

/// Java 发行包来源
#[async_trait::async_trait]
pub trait JavaReleaseSource: Send + Sync {
    /// 来源名（用于日志与错误聚合）
    fn name(&self) -> &'static str;

    /// 列出符合条件的发行包，按版本降序
    ///
    /// # Errors
    ///
    /// 上游不可达、状态码异常或响应无法解析时返回 [`SourceError`]
    async fn list_packages(
        &self,
        request: &JavaPackageRequest,
    ) -> Result<Vec<JavaPackage>, SourceError>;

    /// 解析某包的下载地址
    ///
    /// # Errors
    ///
    /// 包不存在返回 [`SourceError::NotFound`]；其余同上
    async fn resolve_binary(&self, package: &JavaPackage) -> Result<ResolvedJava, SourceError>;
}

/// Java 发行包聚合注册表：多源按优先级故障切换（service 只依赖这个聚合端口）
#[async_trait::async_trait]
pub trait JavaSourceRegistryPort: Send + Sync {
    /// 源名列表（当前优先级）
    fn source_names(&self) -> Vec<&'static str>;

    /// 并发探测各源元数据端点，返回按延迟升序重排后的注册表
    ///
    /// 探测失败的源被剔除；幂等，未探测到任何源时返回空注册表
    async fn ranked(&self) -> Arc<dyn JavaSourceRegistryPort>;

    /// 按优先级列出可用包；单源失败切换下一个，全部失败聚合错误
    ///
    /// # Errors
    ///
    /// 所有源均失败时返回最后一个 [`SourceError`]
    async fn list_packages(
        &self,
        request: &JavaPackageRequest,
    ) -> Result<Vec<JavaPackage>, SourceError>;

    /// 解析下载地址；单源失败切换下一个
    ///
    /// # Errors
    ///
    /// 所有源均失败时返回最后一个 [`SourceError`]
    async fn resolve_binary(&self, package: &JavaPackage) -> Result<ResolvedJava, SourceError>;
}

/// 本机 Java 扫描结果
#[derive(Debug, Clone)]
pub struct ScannedJava {
    pub path: PathBuf,
    pub major: JavaMajor,
    pub version: String,
    pub vendor: JavaVendor,
}

/// 本机 Java 扫描器
pub trait JavaScanner: Send + Sync {
    /// 扫描本机已安装的 Java，每个候选项通过 `on_found` 回调逐个上报
    ///
    /// 契约：同一 JVM home 只上报一次（unix 上实现须先把候选路径归一到真实
    /// 路径再去重）；service 侧不再重复去重。
    ///
    /// 语义是 best-effort：单项候选探测失败只跳过该候选（`java -version`
    /// 退出非零的损坏安装不登记），不中断整体扫描，因此正常情况下总是 `Ok(())`。
    ///
    /// # Errors
    ///
    /// 实现自身的致命错误（平台命令整体不可用等）
    fn scan(
        &self,
        on_found: &mut dyn FnMut(ScannedJava),
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

/// Java 运行时注册表
pub trait JavaRuntimeRepository: Send + Sync {
    /// 写入或覆盖一条运行时记录（按 path 去重）
    ///
    /// # Errors
    ///
    /// 底层失败返回 [`RepositoryError::Storage`]
    fn upsert(&self, runtime: &JavaRuntime) -> Result<(), RepositoryError>;

    /// 全部运行时，按 major 降序、path 升序
    ///
    /// # Errors
    ///
    /// 底层失败返回 [`RepositoryError::Storage`]
    fn list(&self) -> Result<Vec<JavaRuntime>, RepositoryError>;

    /// 按路径读取
    ///
    /// # Errors
    ///
    /// 不存在返回 [`RepositoryError::NotFound`]
    fn get_by_path(&self, path: &Path) -> Result<JavaRuntime, RepositoryError>;

    /// 删除一条记录
    ///
    /// # Errors
    ///
    /// 不存在返回 [`RepositoryError::NotFound`]
    fn remove(&self, path: &Path) -> Result<(), RepositoryError>;

    /// 扫描状态：已扫描过则返回 `Some(unix秒)`
    ///
    /// # Errors
    ///
    /// 底层失败返回 [`RepositoryError::Storage`]
    fn scan_state(&self) -> Result<Option<i64>, RepositoryError>;

    /// 标记已扫描（幂等覆盖时间戳）
    ///
    /// # Errors
    ///
    /// 底层失败返回 [`RepositoryError::Storage`]
    fn mark_scanned(&self, at: i64) -> Result<(), RepositoryError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_classification_is_by_error_kind() {
        let net: Box<dyn std::error::Error + Send + Sync> = Box::new(std::io::Error::other("boom"));
        assert!(SourceError::Network(net).retryable());
        assert!(SourceError::Status { status: 503 }.retryable());
        assert!(SourceError::Status { status: 429 }.retryable());
        assert!(!SourceError::Status { status: 404 }.retryable());
        assert!(!SourceError::NotFound(VersionId::new("1.21.1")).retryable());
    }
}
