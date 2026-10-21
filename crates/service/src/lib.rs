mod fs_error;
mod java;
mod registry;
mod util;
mod workspace;

pub use fs_error::FsError;
pub use java::{
    InstallOutcome, InstallStage, JavaError, JavaService, RemoteJavaPackage, ScanOutcome,
    StageSink, UninstallOutcome,
};
/// Java 源聚合端口（定义在 `port`，此处转出便于既有调用方）
pub use port::JavaSourceRegistryPort;
pub use registry::{
    InstallJarError, InstalledServerJar, RegisteredServer, ServerRegistry, ServerRegistryError,
};
pub use workspace::{
    DoctorFinding, DoctorLevel, InitOutcome, WorkspaceError, WorkspaceService, absolute_path,
};

use domain::server::download::DownloadStatus;
use domain::server::error::InvalidTransition;
use domain::server::version::VersionId;
use port::{
    ArtifactDownloader, CancellationToken, DownloadRequest, DownloadedArtifact, ProgressSink,
    ServerJarSource,
};
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;

/// 安装任务失败
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum InstallError {
    /// 来源解析或上游访问失败
    #[error(transparent)]
    Source(#[from] port::SourceError),
    /// 下载或校验失败
    #[error(transparent)]
    Download(#[from] port::DownloadError),
    /// 状态机流转出错
    #[error(transparent)]
    Status(#[from] InvalidTransition),
}

/// 安装成功的结果
#[derive(Debug, Clone)]
pub struct InstalledDownload {
    /// 最终状态
    pub status: DownloadStatus,
    /// 最终信息
    pub artifact: DownloadedArtifact,
    /// 来源声明的校验和（下载时已按它校验）
    pub checksum: domain::server::artifact::Checksum,
}

/// 服务端安装编排
pub struct ServerInstaller {
    source: Arc<dyn ServerJarSource>,
    downloader: Arc<dyn ArtifactDownloader>,
}

impl std::fmt::Debug for ServerInstaller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerInstaller").finish_non_exhaustive()
    }
}

impl ServerInstaller {
    /// 组合来源与下载器
    #[must_use]
    pub fn new(source: Arc<dyn ServerJarSource>, downloader: Arc<dyn ArtifactDownloader>) -> Self {
        Self { source, downloader }
    }

    /// 解析选中版本并下载
    ///
    /// # Errors
    ///
    /// 版本不存在、来源不可达、下载/校验失败或任务被取消时返回 [`InstallError`]
    pub async fn install(
        &self,
        version: &VersionId,
        destination: PathBuf,
        progress: Option<&dyn ProgressSink>,
        cancel: &CancellationToken,
    ) -> Result<InstalledDownload, InstallError> {
        let mut status = DownloadStatus::Pending.transition(DownloadStatus::Downloading)?;
        let resolved = self.source.artifact(version).await?;
        let checksum = resolved.artifact.checksum.clone();

        let request = DownloadRequest {
            location: resolved.location,
            destination,
            expected: Some(resolved.artifact.checksum),
            expected_size: Some(resolved.artifact.size),
        };
        let artifact = self.downloader.download(&request, progress, cancel).await?;
        status = status.transition(DownloadStatus::Verifying)?;
        status = status.transition(DownloadStatus::Ready)?;
        Ok(InstalledDownload {
            status,
            artifact,
            checksum,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::server::artifact::{Checksum, ChecksumAlgorithm, JarArtifact};
    use port::{DownloadError, DownloadedArtifact, ResolvedArtifact, SourceError};
    use std::error::Error;

    struct FakeSource {
        resolved: ResolvedArtifact,
        not_found: bool,
    }

    struct FakeDownloader {
        downloaded: DownloadedArtifact,
        cancelled: bool,
    }

    #[async_trait::async_trait]
    impl ServerJarSource for FakeSource {
        async fn versions(
            &self,
        ) -> Result<Vec<domain::server::version::ServerVersion>, SourceError> {
            Ok(Vec::new())
        }

        async fn artifact(&self, version: &VersionId) -> Result<ResolvedArtifact, SourceError> {
            if self.not_found {
                Err(SourceError::NotFound(version.clone()))
            } else {
                Ok(self.resolved.clone())
            }
        }
    }

    #[async_trait::async_trait]
    impl ArtifactDownloader for FakeDownloader {
        async fn download(
            &self,
            _request: &DownloadRequest,
            _progress: Option<&dyn ProgressSink>,
            _cancel: &CancellationToken,
        ) -> Result<DownloadedArtifact, DownloadError> {
            if self.cancelled {
                Err(DownloadError::Cancelled)
            } else {
                Ok(self.downloaded.clone())
            }
        }
    }

    fn resolved() -> Result<ResolvedArtifact, url::ParseError> {
        Ok(ResolvedArtifact {
            artifact: JarArtifact {
                version: VersionId::new("1.21.1"),
                size: 12,
                checksum: Checksum::new(ChecksumAlgorithm::Sha1, "0".repeat(40)),
            },
            location: "https://example.invalid/server.jar".parse()?,
        })
    }

    #[tokio::test]
    async fn happy_path_reaches_ready() -> Result<(), Box<dyn Error>> {
        let installer = ServerInstaller::new(
            Arc::new(FakeSource {
                resolved: resolved()?,
                not_found: false,
            }),
            Arc::new(FakeDownloader {
                downloaded: DownloadedArtifact {
                    path: PathBuf::from("/tmp/server.jar"),
                    size: 12,
                },
                cancelled: false,
            }),
        );

        let out = installer
            .install(
                &VersionId::new("1.21.1"),
                PathBuf::from("/tmp/server.jar"),
                None,
                &CancellationToken::new(),
            )
            .await?;

        assert_eq!(out.status, DownloadStatus::Ready);
        assert_eq!(out.artifact.path, PathBuf::from("/tmp/server.jar"));
        Ok(())
    }

    #[tokio::test]
    async fn propagates_download_cancellation() -> Result<(), Box<dyn Error>> {
        let installer = ServerInstaller::new(
            Arc::new(FakeSource {
                resolved: resolved()?,
                not_found: false,
            }),
            Arc::new(FakeDownloader {
                downloaded: DownloadedArtifact {
                    path: PathBuf::from("/tmp/server.jar"),
                    size: 12,
                },
                cancelled: true,
            }),
        );

        let result = installer
            .install(
                &VersionId::new("1.21.1"),
                PathBuf::from("/tmp/server.jar"),
                None,
                &CancellationToken::new(),
            )
            .await;

        assert!(matches!(
            result,
            Err(InstallError::Download(DownloadError::Cancelled))
        ));
        Ok(())
    }

    #[tokio::test]
    async fn propagates_not_found() -> Result<(), Box<dyn Error>> {
        let installer = ServerInstaller::new(
            Arc::new(FakeSource {
                not_found: true,
                resolved: ResolvedArtifact {
                    artifact: JarArtifact {
                        version: VersionId::new("placeholder"),
                        size: 0,
                        checksum: Checksum::new(ChecksumAlgorithm::Sha1, "0".repeat(40)),
                    },
                    location: "https://example.invalid/placeholder.jar".parse()?,
                },
            }),
            Arc::new(FakeDownloader {
                downloaded: DownloadedArtifact {
                    path: PathBuf::from("/tmp/server.jar"),
                    size: 12,
                },
                cancelled: false,
            }),
        );

        let result = installer
            .install(
                &VersionId::new("1.21.1"),
                PathBuf::from("/tmp/server.jar"),
                None,
                &CancellationToken::new(),
            )
            .await;

        assert!(matches!(
            result,
            Err(InstallError::Source(SourceError::NotFound(_)))
        ));
        Ok(())
    }
}
