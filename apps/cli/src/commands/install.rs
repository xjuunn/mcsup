use crate::config::Config;
use crate::output::Printer;
use crate::progress::{BarSink, TaskSpinner};
use anyhow::{Context, Result};
use mcsup::domain::server::version::VersionId;
use mcsup::file::FileDownloader;
use mcsup::port::CancellationToken;
use mcsup::provider::http::HttpByteSource;
use mcsup::provider::mojang::MojangSource;
use mcsup::service::ServerInstaller;
use std::sync::Arc;
use tracing::info;

/// 装配 Mojang 来源 + HTTP 传输 + 文件下载器，解析并安装服务端核心
///
/// # Errors
///
/// 版本不存在（3）、来源或传输/校验失败（5）、其他安装失败（1）时返回错误
pub(crate) async fn run(
    name: &str,
    version: Option<&str>,
    _cfg: &Config,
    printer: Printer,
) -> Result<()> {
    let version = version.unwrap_or("latest");
    let version_id = VersionId::new(version);
    let dir = std::env::current_dir().context("获取当前工作目录失败")?;
    let _ = name;
    let destination = dir.join("server.jar");
    let source = Arc::new(MojangSource::new().context("初始化 Mojang 源")?);
    let bytes = Arc::new(HttpByteSource::new().context("初始化 HTTP 传输")?);
    let downloader = FileDownloader::new(bytes.clone()).with_range_source(bytes);
    let installer = ServerInstaller::new(source, Arc::new(downloader));

    let cancel = CancellationToken::new();
    // 解析版本到首字节之间没有任何进度回调，先挂 spinner 保证整段有回显；
    // 首个进度到达后由 BarSink 接手同一条线，切换成进度显示
    let message = format!("解析并下载 {version_id}");
    let spinner = TaskSpinner::new(message.clone(), printer.is_human());
    let sink = BarSink::new(spinner.handle(), message);
    info!(version = %version_id, dest = %&destination.display(), "开始安装");
    let outcome = installer
        .install(&version_id, destination.clone(), Some(&sink), &cancel)
        .await
        .context("安装服务端失败")?;
    spinner.finish();
    printer.print_installed(&version_id, &outcome.artifact, &destination)
}
