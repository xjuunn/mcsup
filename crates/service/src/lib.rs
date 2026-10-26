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
    WorkspaceService, absolute_path, resolve_selector,
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
    /// 安装器执行失败（`Forge` / `NeoForge`）
    #[error(transparent)]
    Installer(#[from] port::InstallerError),
    /// 安装了新核心，但无法确定其启动方式或参数文件不全
    #[error("installed server layout is incomplete: {0}")]
    Layout(String),
    /// 该来源要先运行安装器，但本机没有可用的 Java 运行时
    #[error(
        "installing this server requires Java to run the installer, \
         but no Java runtime was found; install one with `mcsup java install`"
    )]
    NoJavaForInstaller,
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
    /// 下载物是否为安装器（决定下载后是否运行它、以及最终启动方式）
    pub install: port::InstallKind,
    /// 安装器产物的版本目录名（`Forge` 的完整 build 串）；非安装器来源恒为 `None`
    pub build: Option<String>,
}

/// 安装器执行所需的运行环境
///
/// 由调用方（持有 `JavaService` 的 service 层）解析好 `java_bin` 后传入；
/// `ServerInstaller` 自身不查 Java，保持「来源 + 下载 + 安装」的单一职责。
#[derive(Clone)]
pub struct InstallerContext {
    /// 运行安装器的 `java` 可执行文件
    pub java_bin: PathBuf,
    /// 安装器执行器
    pub runner: Arc<dyn port::InstallerRunner>,
    /// 安装器输出的实时观察者：联网拉 libraries 可达数分钟，逐行透传给用户
    pub output: Option<Arc<dyn port::InstallerOutputSink>>,
}

impl std::fmt::Debug for InstallerContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstallerContext")
            .field("java_bin", &self.java_bin)
            .finish_non_exhaustive()
    }
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
    /// 直接可运行的服务端（原版 / Fabric / Paper）下到 `destination` 即结束；
    /// 安装器（Forge / NeoForge）先下到 `destination`，再由 `installer`
    /// 就地安装成服务端目录。`installer` 仅在需要时读取：来源声明为
    /// [`port::InstallKind::Direct`] 时传 `None` 即可。
    ///
    /// `stages` 用于回显安装器阶段：下载完成到安装完成之间可能长达数分钟
    /// （安装器要联网拉 libraries），没有阶段回调时用户只看到冻结的进度条。
    ///
    /// # Errors
    ///
    /// 版本不存在、来源不可达、下载/校验失败、安装器执行失败或任务被取消时
    /// 返回 [`InstallError`]
    pub async fn install(
        &self,
        version: &VersionId,
        destination: PathBuf,
        installer: Option<&InstallerContext>,
        progress: Option<&dyn ProgressSink>,
        stages: Option<&dyn StageSink>,
        cancel: &CancellationToken,
    ) -> Result<InstalledDownload, InstallError> {
        let mut status = DownloadStatus::Pending.transition(DownloadStatus::Downloading)?;
        let resolved = self.source.artifact(version).await?;
        let checksum = resolved.artifact.checksum.clone();
        let java_major = resolved.java_major;
        let install = resolved.install;
        // 安装器产出的启动参数文件在 `libraries/.../{build}/` 下，目录名是平台
        // 自己的版本串（Forge 为 `26.3-66.1.0`），与 `VersionId`（`26.3`）不同；
        // 由来源在解析时给出，这里只负责往下传。
        let build = resolved.installer_args_dir;
        let notify = |stage: InstallStage| {
            if let Some(sink) = stages {
                sink.on_stage(stage);
            }
        };

        // 上游未提供哈希 / 大小时不传校验条件：Fabric 等服务端 JAR 走 chunked
        // 且无 Content-Length，占位值参与比对必然失败，只能以「无校验」下载。
        // 大小 `0` 表示来源未声明（如 Forge 只给了 SHA1 侧车）：0 字节不可能是
        // 真实文件，按「未知」处理，不当作期望值参与比对。
        let unverified = !checksum.is_verified();
        let known_size = resolved.artifact.size > 0;
        let request = DownloadRequest {
            location: resolved.location,
            destination: destination.clone(),
            expected: (!unverified).then(|| checksum.clone()),
            expected_size: (!unverified && known_size).then_some(resolved.artifact.size),
        };
        notify(if install == port::InstallKind::Installer {
            InstallStage::DownloadingInstaller
        } else {
            InstallStage::Downloading
        });
        let artifact = self.downloader.download(&request, progress, cancel).await?;
        status = status.transition(DownloadStatus::Verifying)?;

        // 安装器：下载物是安装器本身，就地运行它产出服务端目录
        if install == port::InstallKind::Installer {
            if cancel.is_cancelled() {
                return Err(port::DownloadError::Cancelled.into());
            }
            // 没拿到上下文几乎总是「本机没有可用 Java」——这句话要说人话，
            // 用户看到的是「为什么装不成」，而不是内部装配缺了什么
            let ctx = installer.ok_or(InstallError::NoJavaForInstaller)?;
            let dir = destination.parent().ok_or_else(|| {
                InstallError::Layout(format!(
                    "安装器下载路径没有父目录：{}",
                    destination.display()
                ))
            })?;
            // 从这里到返回之间是纯阻塞执行（几分钟量级），安装器的输出会逐行
            // 透传给用户，让他们看到正在联机下载 libraries 而不是卡死
            notify(InstallStage::Installing);
            ctx.runner.run_installer(
                &ctx.java_bin,
                &destination,
                dir,
                ctx.output.as_deref(),
                cancel,
            )?;
            notify(InstallStage::Finalizing);
        }

        status = status.transition(DownloadStatus::Ready)?;
        Ok(InstalledDownload {
            status,
            artifact,
            checksum,
            java_major,
            install,
            build,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::server::artifact::{Checksum, ChecksumAlgorithm, JarArtifact};
    use port::{DownloadError, DownloadedArtifact, ResolvedArtifact, SourceError};
    use std::error::Error;
    use std::path::Path;
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
            install: port::InstallKind::Direct,
            installer_args_dir: None,
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
                None,
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
                None,
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
                    install: port::InstallKind::Direct,
                    installer_args_dir: None,
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
                None,
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
                None,
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
                    install: port::InstallKind::Direct,
                    installer_args_dir: None,
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
                None,
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

    // ---- 安装器路径：extras 必须写进服务器目录 ----

    /// 假安装器执行器：模拟安装器把参数文件写进目录，再记录被调用次数
    struct FakeRunner {
        calls: Arc<Mutex<usize>>,
    }

    impl port::InstallerRunner for FakeRunner {
        fn run_installer(
            &self,
            _java_bin: &Path,
            _installer: &Path,
            dir: &Path,
            _output: Option<&dyn port::InstallerOutputSink>,
            _cancel: &CancellationToken,
        ) -> Result<(), port::InstallerError> {
            *self.calls.lock().expect("lock") += 1;
            std::fs::write(dir.join("user_jvm_args.txt"), b"# args").expect("write");
            Ok(())
        }
    }

    /// 安装器跑完后，安装器产出的参数文件目录名随结果一起带出
    ///
    /// 该目录名由来源给出（`Forge` 为 `26.3-66.1.0`），与 `VersionId`
    /// （`26.3`）不同；service 据此拼参数文件路径。
    #[tokio::test]
    async fn installer_args_dir_is_returned() -> Result<(), Box<dyn Error>> {
        let dir = tempfile::tempdir()?;
        let mut artifact = resolved()?;
        artifact.install = port::InstallKind::Installer;
        artifact.installer_args_dir = Some("26.3-66.1.0".to_owned());
        let calls = Arc::new(Mutex::new(0));
        let installer = ServerInstaller::new(
            Arc::new(FakeSource {
                resolved: artifact,
                not_found: false,
            }),
            Arc::new(FakeDownloader::new(
                DownloadedArtifact {
                    path: dir.path().join(".forge-installer.jar"),
                    size: 12,
                },
                false,
            )),
        );
        let ctx = InstallerContext {
            java_bin: PathBuf::from("/jdk/bin/java"),
            runner: Arc::new(FakeRunner {
                calls: Arc::clone(&calls),
            }),
            output: None,
        };

        let out = installer
            .install(
                &VersionId::new("26.3"),
                dir.path().join(".forge-installer.jar"),
                Some(&ctx),
                None,
                None,
                &CancellationToken::new(),
            )
            .await?;

        assert_eq!(*calls.lock().expect("lock"), 1);
        assert_eq!(out.build.as_deref(), Some("26.3-66.1.0"));
        Ok(())
    }
}
