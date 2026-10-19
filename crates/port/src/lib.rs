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

    /// 当前默认工作目录；尚无注册时为 `None`
    ///
    /// # Errors
    ///
    /// 底层失败返回 [`RepositoryError::Storage`]
    fn default(&self) -> Result<Option<Workspace>, RepositoryError>;

    /// 设为默认并清除其他默认标记（原子）
    ///
    /// # Errors
    ///
    /// 不存在返回 [`RepositoryError::NotFound`]
    fn set_default(&self, id: &WorkspaceId) -> Result<(), RepositoryError>;

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
