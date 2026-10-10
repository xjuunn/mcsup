use domain::server::artifact::JarArtifact;
use domain::server::version::{ServerVersion, VersionId};
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
    NetWork(#[source] Box<dyn std::error::Error + Send + Sync>),
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
            Self::NetWork(_) => true,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_classification_is_by_error_kind() {
        let net: Box<dyn std::error::Error + Send + Sync> = Box::new(std::io::Error::other("boom"));
        assert!(SourceError::NetWork(net).retryable());
        assert!(SourceError::Status { status: 503 }.retryable());
        assert!(SourceError::Status { status: 429 }.retryable());
        assert!(!SourceError::Status { status: 404 }.retryable());
        assert!(!SourceError::NotFound(VersionId::new("1.21.1")).retryable());
    }
}
