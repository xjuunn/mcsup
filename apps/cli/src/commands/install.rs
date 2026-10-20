use crate::bootstrap;
use crate::output::Printer;
use crate::progress::InstallSession;
use anyhow::{Context, Result};
use mcsup::domain::server::version::VersionId;
use mcsup::provider::mojang::MojangSource;
use mcsup::service::ServerInstaller;
use std::sync::Arc;
use tracing::info;

/// 装配 Mojang 来源 + HTTP 传输 + 文件下载器，按注册表定位服务器目录后安装核心
///
/// 目标目录 = 注册表里该服务器的目录（`{workspace}/servers/{name}/server.jar`），
/// 当前工作目录不参与定位；安装完成回写 `meta.jar`。
///
/// # Errors
///
/// 服务器不存在（3）、版本不存在（3）、来源或传输/校验失败（5）、其他安装失败（1）时返回错误
pub(crate) async fn run(
    name: &str,
    version: Option<&str>,
    _cfg: &crate::config::Config,
    printer: Printer,
) -> Result<()> {
    let version = version.unwrap_or("latest");
    let version_id = VersionId::new(version);
    let registry = bootstrap::open_registry()?;
    let source = Arc::new(MojangSource::new().context("初始化 Mojang 源")?);
    let downloader = Arc::new(bootstrap::build_downloader()?);
    let installer = ServerInstaller::new(source, downloader);

    // 解析版本到首字节之间没有任何进度回调，先挂 spinner 保证整段有回显；
    // 首个进度到达后由 session 内的 BarSink 接手同一条线，切换成进度显示
    let message = format!("解析并下载 {version_id}");
    let session = InstallSession::new(message, printer.is_human());
    info!(version = %version_id, server = name, "开始安装");
    let outcome = registry
        .install_jar(
            name,
            &version_id,
            &installer,
            Some(&session),
            session.cancel(),
        )
        .await
        .with_context(|| format!("安装服务端 {name} 失败"))?;
    session.finish();
    printer.print_installed(&version_id, outcome.jar.size, &outcome.destination)
}
