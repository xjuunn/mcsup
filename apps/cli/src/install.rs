use std::path::PathBuf;
use std::sync::Arc;

use domain::server::version::VersionId;
use file::FileDownloader;
use port::CancellationToken;
use provider::http::HttpByteSource;
use provider::mojang::MojangSource;
use service::{InstallError, ServerInstaller};

/// 各层实现的一次性装配，供入口复用
pub(crate) struct InstallRunner {
    installer: ServerInstaller,
}

impl InstallRunner {
    /// 装配 Mojang 来源、HTTP 字节来源与文件下载器
    ///
    /// # Errors
    ///
    /// HTTP 客户端构建失败时返回 [`InstallError::Source`]
    pub(crate) fn assemble() -> Result<Self, InstallError> {
        let source = Arc::new(MojangSource::new()?);
        let bytes = Arc::new(HttpByteSource::new()?);
        let downloader = Arc::new(FileDownloader::new(bytes));
        Ok(Self {
            installer: ServerInstaller::new(source, downloader),
        })
    }

    /// 安装 `version` 到 `dir/server.jar`
    ///
    /// # Errors
    ///
    /// 版本未找到、来源不可达或下载/校验失败时返回 [`InstallError`]
    pub(crate) async fn install(
        &self,
        version: &VersionId,
        dir: PathBuf,
    ) -> Result<PathBuf, InstallError> {
        let destination = dir.join("server.jar");
        let downloaded = self
            .installer
            .install(version, destination, &CancellationToken::new())
            .await?;
        Ok(downloaded.artifact.path)
    }
}

impl std::fmt::Debug for InstallRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstallRunner").finish_non_exhaustive()
    }
}
