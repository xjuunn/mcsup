use crate::config::Config;
use crate::output::Printer;
use crate::progress::BarSink;
use anyhow::{Context, Result};
use domain::server::version::VersionId;
use file::FileDownloader;
use port::CancellationToken;
use provider::http::HttpByteSource;
use provider::mojang::MojangSource;
use service::ServerInstaller;
use std::path::Path;
use std::sync::Arc;
use tracing::info;

/// 装配 Mojang 来源 + HTTP 传输 + 文件下载器，解析并安装服务端核心
///
/// # Errors
///
/// 版本不存在（3）、来源或传输/校验失败（5）、其他安装失败（1）时返回错误
pub(crate) async fn run(
    version: &str,
    dir: Option<&Path>,
    _cfg: &Config,
    printer: Printer,
) -> Result<()> {
    let version_id = VersionId::new(version);
    let dir = match dir {
        Some(dir) => dir.to_path_buf(),
        None => std::env::current_dir().context("获取当前工作目录失败")?,
    };
    let destination = dir.join("server.jar");
    let source = Arc::new(MojangSource::new().context("初始化 Mojang 源")?);
    let bytes = Arc::new(HttpByteSource::new().context("初始化 HTTP 传输")?);
    let downloader = FileDownloader::new(bytes.clone()).with_range_source(bytes);
    let installer = ServerInstaller::new(source, Arc::new(downloader));

    let cancel = CancellationToken::new();
    let sink = BarSink::new(format!("解析并下载 {version_id}"), printer.is_human());
    info!(version = %version_id, dest = %&destination.display(), "开始安装");
    let outcome = installer
        .install(&version_id, destination.clone(), Some(&sink), &cancel)
        .await
        .context("安装服务端失败")?;
    sink.finish();
    printer.print_installed(&version_id, &outcome.artifact, &destination)
}
