mod backup;
mod doctor;
mod fs_error;
mod java;
mod lifecycle;
mod platform_config;
mod platform_doctor;
mod props;
mod registry;
mod settings;
mod util;
mod workspace;

/// `workspace doctor` 的检查项标识清单（`--only` 校验与帮助文案共用）
pub const KNOWN_KINDS: &[&str] = CheckKind::KNOWN_KINDS;
pub use backup::{
    BackupError, BackupErrorKind, BackupService, CreateOptions, CreateOutcome,
    DEFAULT_FLUSH_WAIT_MS, DeleteOutcome, MAX_LABEL_LEN, PruneOutcome, RestoreOutcome,
    SnapshotPage, VerifyOutcome,
};
pub use doctor::{CheckKind, DoctorError, DoctorService, Finding, Report, Severity};
use domain::server::download::DownloadStatus;
use domain::server::error::InvalidTransition;
use domain::server::version::VersionId;
pub use fs_error::FsError;
pub use java::{
    InstallOutcome, InstallStage, JavaError, JavaService, RemoteJavaPackage, ScanOutcome,
    StageSink, UninstallOutcome,
};
pub use lifecycle::{
    BusyOp, DEFAULT_RAM_MIB, LifecycleError, LifecycleErrorKind, LogWatch, ServerLifecycleService,
    StartOptions, StatusReport,
};
pub use platform_config::{
    Config, ConfigKey, ConfigValueError, EffectiveValue, FEATURE_HEADER, JAVA_SOURCE_ALL,
    lexical_normalize,
};
pub use platform_doctor::{PlatformCheck, PlatformDoctor, PlatformFinding, PlatformReport};
/// Java 源聚合端口（定义在 `port`，此处转出便于既有调用方）
pub use port::JavaSourceRegistryPort;
use port::{
    ArtifactDownloader, CancellationToken, DownloadRequest, DownloadedArtifact, ProgressSink,
    ServerJarSource,
};
pub use props::{PropsError, PropsReport, ServerPropsService};
pub use registry::{
    InstallJarError, InstallJarErrorKind, InstalledServerJar, RegisteredServer, ServerRegistry,
    ServerRegistryError, ServerRegistryErrorKind,
};
pub use settings::{ServerSettingsService, SettingChange, SettingsError, SettingsErrorKind};
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;
pub use workspace::{
    InitOutcome, MARKER_FILE, NOISE_ENTRIES, RemoveOutcome, WorkspaceError, WorkspaceErrorKind,
    WorkspaceService, absolute_path,
};

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
    /// 该版本要求的 Java 大版本（版本详情 `javaVersion.majorVersion`）
    pub java_major: Option<domain::java::JavaMajor>,
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
        let java_major = resolved.java_major;

        // 上游未提供哈希 / 大小时不传校验条件：Fabric 等服务端 JAR 走 chunked
        // 且无 Content-Length，占位值参与比对必然失败，只能以「无校验」下载。
        let unverified = !checksum.is_verified();
        let request = DownloadRequest {
            location: resolved.location,
            destination,
            expected: (!unverified).then(|| checksum.clone()),
            expected_size: (!unverified).then_some(resolved.artifact.size),
        };
        let artifact = self.downloader.download(&request, progress, cancel).await?;
        status = status.transition(DownloadStatus::Verifying)?;
        status = status.transition(DownloadStatus::Ready)?;
        Ok(InstalledDownload {
            status,
            artifact,
            checksum,
            java_major,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::server::artifact::{Checksum, ChecksumAlgorithm, JarArtifact};
    use port::{DownloadError, DownloadedArtifact, ResolvedArtifact, SourceError};
    use std::error::Error;
    use std::sync::Mutex;

    struct FakeSource {
        resolved: ResolvedArtifact,
        not_found: bool,
    }

    #[derive(Clone)]
    struct FakeDownloader {
        downloaded: DownloadedArtifact,
        cancelled: bool,
        /// 记录收到的请求，供断言校验条件是否下发
        seen: Arc<Mutex<Option<DownloadRequest>>>,
    }

    impl FakeDownloader {
        fn new(downloaded: DownloadedArtifact, cancelled: bool) -> Self {
            Self {
                downloaded,
                cancelled,
                seen: Arc::new(Mutex::new(None)),
            }
        }
    }

    #[async_trait::async_trait]
    impl ServerJarSource for FakeSource {
        fn variant(&self) -> domain::server::version::ServerVariant {
            domain::server::version::ServerVariant::Vanilla
        }

        fn name(&self) -> &'static str {
            "fake"
        }

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
            request: &DownloadRequest,
            _progress: Option<&dyn ProgressSink>,
            _cancel: &CancellationToken,
        ) -> Result<DownloadedArtifact, DownloadError> {
            if let Ok(mut seen) = self.seen.lock() {
                *seen = Some(request.clone());
            }
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
            java_major: None,
        })
    }

    #[tokio::test]
    async fn happy_path_reaches_ready() -> Result<(), Box<dyn Error>> {
        let installer = ServerInstaller::new(
            Arc::new(FakeSource {
                resolved: resolved()?,
                not_found: false,
            }),
            Arc::new(FakeDownloader::new(
                DownloadedArtifact {
                    path: PathBuf::from("/tmp/server.jar"),
                    size: 12,
                },
                false,
            )),
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
            Arc::new(FakeDownloader::new(
                DownloadedArtifact {
                    path: PathBuf::from("/tmp/server.jar"),
                    size: 12,
                },
                true,
            )),
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
                    java_major: None,
                },
            }),
            Arc::new(FakeDownloader::new(
                DownloadedArtifact {
                    path: PathBuf::from("/tmp/server.jar"),
                    size: 12,
                },
                false,
            )),
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

    /// 有真实校验和时，校验条件必须下发给下载器
    #[tokio::test]
    async fn verified_artifact_passes_checksum_to_downloader() -> Result<(), Box<dyn Error>> {
        let downloader = Arc::new(FakeDownloader::new(
            DownloadedArtifact {
                path: PathBuf::from("/tmp/server.jar"),
                size: 12,
            },
            false,
        ));
        let installer = ServerInstaller::new(
            Arc::new(FakeSource {
                resolved: resolved()?,
                not_found: false,
            }),
            downloader.clone(),
        );
        installer
            .install(
                &VersionId::new("1.21.1"),
                PathBuf::from("/tmp/server.jar"),
                None,
                &CancellationToken::new(),
            )
            .await?;

        let seen = downloader.seen.lock().expect("lock").clone().expect("请求");
        assert!(seen.expected.is_some(), "应下发校验和");
        assert_eq!(seen.expected_size, Some(12));
        Ok(())
    }

    /// 上游未提供校验和（如 Fabric）时不下发任何校验条件：
    /// 否则占位值参与比对，下载必然以 `ChecksumMismatch` 失败
    #[tokio::test]
    async fn unverified_artifact_skips_checksum_verification() -> Result<(), Box<dyn Error>> {
        let downloader = Arc::new(FakeDownloader::new(
            DownloadedArtifact {
                path: PathBuf::from("/tmp/server.jar"),
                size: 293_000,
            },
            false,
        ));
        let installer = ServerInstaller::new(
            Arc::new(FakeSource {
                resolved: ResolvedArtifact {
                    artifact: JarArtifact {
                        version: VersionId::new("1.21.1"),
                        size: 0,
                        checksum: Checksum::unverified(),
                    },
                    location: "https://example.invalid/server.jar".parse()?,
                    java_major: None,
                },
                not_found: false,
            }),
            downloader.clone(),
        );

        let out = installer
            .install(
                &VersionId::new("1.21.1"),
                PathBuf::from("/tmp/server.jar"),
                None,
                &CancellationToken::new(),
            )
            .await?;

        let seen = downloader.seen.lock().expect("lock").clone().expect("请求");
        assert!(seen.expected.is_none(), "未校验时不得下发校验和");
        assert_eq!(seen.expected_size, None, "未校验时不得下发大小");
        // 落库的 checksum 仍是未校验占位值，真实大小以下载结果为准
        assert!(!out.checksum.is_verified());
        assert_eq!(out.artifact.size, 293_000);
        Ok(())
    }
}
