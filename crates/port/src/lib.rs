use domain::server::artifact::JarArtifact;
use domain::server::version::{ServerVersion, VersionId};
use thiserror::Error;
use url::Url;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SourceError {
    #[error("version not found: {0}")]
    NotFound(VersionId),
    // 传输层失败，连接、超时、状态码，通常可重试
    #[error("network error: {0}")]
    NetWork(#[source] Box<dyn std::error::Error + Send + Sync>),
    // 响应无法解析，通常不可重试
    #[error("failed to decode upstream response: {0}")]
    Decode(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl SourceError {
    #[must_use]
    pub fn retryable(&self) -> bool {
        matches!(self, Self::NetWork(_))
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedArtifact {
    pub artifact: JarArtifact,
    pub location: Url,
}

#[async_trait::async_trait]
pub trait ServerJarSource: Send + Sync {
    async fn versions(&self) -> Result<Vec<ServerVersion>, SourceError>;
    async fn artifact(&self, version: &VersionId) -> Result<ResolvedArtifact, SourceError>;
}
