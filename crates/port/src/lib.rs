use domain::java::{JavaMajor, JavaPackageType, JavaRuntime, JavaVendor};
use domain::server::artifact::{Checksum, JarArtifact};
use domain::server::meta::{InstalledJar, ServerMeta, ServerName};
use domain::server::settings::ServerSettings;
pub use domain::server::status::ServerStatus;
use domain::server::version::{ServerVariant, ServerVersion, VersionId};
use domain::server::world::{DataVersion, WorldInfo};
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
    /// 该来源服务的变体
    ///
    /// 能力查询的唯一口径：可创建 / 可安装的变体集合 = 已注册来源
    /// `variant()` 的去重结果。未注册的变体入口必须隐藏，显式指定时报错
    /// 并列出可用项，禁止各入口写死变体清单。
    fn variant(&self) -> ServerVariant;
    /// 列出可用版本
    ///
    /// #Errors
    ///
    /// 上游不可达、返回异常状态码或响应无法解析时返回 [`SourceError`]
    async fn versions(&self) -> Result<Vec<ServerVersion>, SourceError>;
    /// 解析某版本的服务器 JAR 下载信息
    ///
    /// 字面量 `latest` / `latest-snapshot` 是保留别名，由来源解析为其声明的
    /// 最新稳定版 / 最新快照。
    ///
    /// # Errors
    ///
    /// 版本不存在返回 [`SourceError::NotFound`]; 其余同上
    async fn artifact(&self, version: &VersionId) -> Result<ResolvedArtifact, SourceError>;
    /// 来源标识（如 `mojang`）；`server version` 的「来源」展示口径
    fn name(&self) -> &'static str;
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

/// 读取世界 / 核心元数据失败
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum InspectError {
    /// 文件无法读取
    #[error("failed to read {path}")]
    Io {
        /// 出错路径
        path: PathBuf,
        /// 底层 IO 错误
        #[source]
        source: std::io::Error,
    },
    /// 文件内容无法解析
    #[error("failed to parse {path}: {message}")]
    Parse {
        /// 出错路径
        path: PathBuf,
        /// 解析失败原因
        message: String,
    },
}

impl InspectError {
    /// 构造 IO 失败
    #[must_use]
    pub fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }

    /// 构造解析失败
    #[must_use]
    pub fn parse(path: &Path, message: impl Into<String>) -> Self {
        Self::Parse {
            path: path.to_path_buf(),
            message: message.into(),
        }
    }
}

/// 读取世界数据版本与核心数据版本
///
/// 安装核心前用它确认目标版本不会把既有世界降级；读取失败由调用方决定
/// 是否中止，本 trait 只报告「有没有 / 是多少」。
pub trait CoreInspector: Send + Sync {
    /// 读取服务器目录下既有世界的信息；没有世界时返回 `None`
    ///
    /// # Errors
    ///
    /// 世界存在但 `level.dat` 无法读取 / 解析时返回 [`InspectError`]
    fn world_info(&self, server_dir: &Path) -> Result<Option<WorldInfo>, InspectError>;

    /// 读取核心 `version.json` 的 `world_version`；核心没有该字段时返回 `None`
    ///
    /// # Errors
    ///
    /// 文件无法读取或归档 / JSON 无法解析时返回 [`InspectError`]
    fn jar_data_version(&self, jar: &Path) -> Result<Option<DataVersion>, InspectError>;
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

    /// 回写核心元数据（版本 + jar）并刷新 `updated_at`，返回最新记录
    ///
    /// 安装核心可能换版本，版本与 jar 必须在同一次写入里落库，
    /// 否则会出现「jar 已是新版本、记录仍是旧版本」的不一致。
    ///
    /// # Errors
    ///
    /// 不存在返回 [`RepositoryError::NotFound`]
    fn update_core(
        &self,
        name: &ServerName,
        version: &VersionId,
        jar: &InstalledJar,
        updated_at: i64,
    ) -> Result<ServerMeta, RepositoryError>;

    /// 回写服务属性并刷新 `updated_at`，返回最新记录
    ///
    /// # Errors
    ///
    /// 不存在返回 [`RepositoryError::NotFound`]
    fn update_settings(
        &self,
        name: &ServerName,
        settings: &ServerSettings,
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

/// supervisor 启动请求（全部路径已由调用方解析）
#[derive(Debug, Clone)]
pub struct SupervisorStartRequest {
    /// 服务器目录：运行期文件（`run/` / `console.log`）都落在其下
    pub server_dir: PathBuf,
    /// `java` 可执行文件
    pub java_bin: PathBuf,
    /// 服务端核心 JAR 绝对路径
    pub jar_path: PathBuf,
    /// 堆内存上限；`None` 用 JVM 默认
    pub ram_mib: Option<u32>,
    /// 用于再拉起 supervisor 的可执行文件（`mcsup` 本体）
    pub supervisor_exe: PathBuf,
}

/// 一次运行的实时视图
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorStatus {
    /// 领域状态机口径
    pub status: ServerStatus,
    /// 服务端进程 PID（`Starting` 起即存在）
    pub pid: Option<u32>,
    /// 从拉起到现在的运行时长
    pub uptime: Option<std::time::Duration>,
    /// 退出码（进程已退出时）
    pub exit_code: Option<i32>,
}

/// supervisor 拉起结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorStartOutcome {
    /// supervisor 进程 PID
    pub supervisor_pid: u32,
}

/// 停机规格
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopSpec {
    /// 优雅停机宽限时长
    pub grace: std::time::Duration,
    /// 宽限到期后是否强杀
    pub force: bool,
}

impl StopSpec {
    /// 默认宽限 30 秒，不强杀
    #[must_use]
    pub fn default_grace() -> Self {
        Self {
            grace: std::time::Duration::from_secs(30),
            force: false,
        }
    }
}

/// 停机结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopOutcome {
    /// 宽限内正常退出
    Stopped {
        /// 请求停机到退出的耗时
        elapsed: std::time::Duration,
    },
    /// 本来就没在跑（幂等）
    AlreadyStopped,
    /// 宽限到期仍在运行（未加 `force` 时不会强杀）
    TimedOut {
        /// 已等待的时长
        waited: std::time::Duration,
    },
}

/// supervisor 交互失败
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SupervisorError {
    /// 服务器已在运行（状态冲突）
    #[error("server is already running")]
    AlreadyRunning,
    /// 拉起 supervisor 进程失败
    #[error("failed to spawn supervisor: {0}")]
    Spawn(#[source] std::io::Error),
    /// 连接 / 读写失败（调用方预期 supervisor 存在的场景）
    #[error("supervisor unreachable: {0}")]
    Unreachable(String),
    /// 协议帧错误或版本不匹配
    #[error("supervisor protocol error: {0}")]
    Protocol(String),
    /// 远端（supervisor 侧）返回的业务错误
    #[error("{kind}: {message}")]
    Remote { kind: String, message: String },
    /// 本地 IO 失败
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// 服务器 supervisor：拉起 / 停机 / 查询 / 控制台注入
///
/// 回答「这个服务器现在跑没跑、怎么让它停下来」。Local 由 `runtime` 实现
/// （本地进程 + IPC），Remote 将来经 transport 代理同一 trait——业务判定
/// 仍在 `domain` / `service`，实现层只回答「在哪里执行」。
pub trait SupervisorPort: Send + Sync {
    /// 拉起 supervisor（已运行时拒绝重复启动）
    ///
    /// # Errors
    ///
    /// 已在运行返回 [`SupervisorError::AlreadyRunning`]；拉起或握手失败返回
    /// 其余错误
    fn start(
        &self,
        request: SupervisorStartRequest,
    ) -> Result<SupervisorStartOutcome, SupervisorError>;

    /// 请求停机：先优雅停机，宽限到期且 `force` 时强杀
    ///
    /// # Errors
    ///
    /// supervisor 无响应或协议失败时返回错误
    fn stop(&self, server_dir: &Path, spec: StopSpec) -> Result<StopOutcome, SupervisorError>;

    /// 实时状态；`Ok(None)` 表示没有 supervisor（未运行）
    ///
    /// # Errors
    ///
    /// 协议失败时返回错误（连接失败按「不在」处理）
    fn status(&self, server_dir: &Path) -> Result<Option<SupervisorStatus>, SupervisorError>;

    /// 向服务端 stdin 注入一行
    ///
    /// # Errors
    ///
    /// 未运行返回 [`SupervisorError::Unreachable`]；其余同上
    fn send_line(&self, server_dir: &Path, line: &str) -> Result<(), SupervisorError>;

    /// 通知空载（进程已退出）的 supervisor 尽快退出
    ///
    /// # Errors
    ///
    /// 协议失败时返回错误
    fn shutdown(&self, server_dir: &Path) -> Result<(), SupervisorError>;
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
