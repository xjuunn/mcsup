use crate::bootstrap;
use crate::output::Printer;
use crate::progress::{InstallSession, TaskSpinner};
use crate::prompts;
use anyhow::{Context, Result};
use mcsup::domain::server::version::{ServerVariant, ServerVersion, VersionId, VersionKind};
use mcsup::port::ServerJarSource;
use mcsup::service::{ServerInstaller, ServerRegistry};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;

/// 按注册表定位服务器并安装 / 更换服务端核心
///
/// 目标目录 = 注册表里该服务器的目录（`{workspace}/servers/{name}/server.jar`），
/// 来源按服务端变体做能力匹配；当前工作目录不参与定位；安装完成回写 `meta.jar`。
///
/// 版本：`--version` 直用；缺省时交互下从版本列表选择，非交互保留
/// `latest`（由来源解析为最新稳定版）。
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
    let registry = bootstrap::open_registry()?;
    let srv = registry
        .get(name)
        .with_context(|| format!("服务器 {name} 不存在"))?;
    let source = bootstrap::source_for(srv.meta.variant)?;
    let version_id = match version {
        Some(raw) => VersionId::new(raw),
        None if printer.is_interactive() => {
            pick_version(source.as_ref(), srv.meta.variant, printer).await?
        }
        None => VersionId::new("latest"),
    };
    let (size, destination) = download_jar(&registry, name, &version_id, source, printer).await?;
    printer.print_installed(&version_id, size, &destination)
}

/// 下载并登记服务端核心；不打印结果行（由调用方输出，保证 JSON 单行契约）
///
/// # Errors
///
/// 服务器不存在、版本不存在、来源不可达或传输 / 校验失败时返回错误
pub(crate) async fn download_jar(
    registry: &ServerRegistry,
    name: &str,
    version_id: &VersionId,
    source: Arc<dyn ServerJarSource>,
    printer: Printer,
) -> Result<(u64, PathBuf)> {
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
            version_id,
            &installer,
            Some(&session),
            session.cancel(),
        )
        .await
        .with_context(|| format!("安装服务端 {name} 失败"))?;
    session.finish();
    Ok((outcome.jar.size, outcome.destination))
}

/// 版本选择器：spinner 拉取列表 → 可过滤选择
///
/// # Errors
///
/// 列表获取失败、来源无版本或读取选择失败时返回错误
pub(crate) async fn pick_version(
    source: &dyn ServerJarSource,
    variant: ServerVariant,
    printer: Printer,
) -> Result<VersionId> {
    let spinner = TaskSpinner::new(
        format!("获取 {} 版本列表", variant.as_str()),
        printer.is_human(),
    );
    let versions = source.versions().await.context("获取版本列表失败")?;
    spinner.finish();
    if versions.is_empty() {
        anyhow::bail!("{} 暂无可用版本", variant.as_str());
    }
    let labels: Vec<String> = versions.iter().map(version_label).collect();
    let picked = prompts::select_filter("选择版本", &labels)?;
    versions
        .into_iter()
        .zip(labels)
        .find(|(_, label)| *label == picked)
        .map(|(v, _)| v.id)
        .ok_or_else(|| anyhow::anyhow!("无效选择: {picked}"))
}

/// 版本候选的展示标签（含版本 id，映射回 id 时按标签全名匹配）
#[must_use]
pub(crate) fn version_label(version: &ServerVersion) -> String {
    match version.kind {
        VersionKind::Release => version.id.to_string(),
        VersionKind::Snapshot => format!("{}（快照）", version.id),
        _ => format!("{}（旧版）", version.id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(kind: VersionKind, id: &str) -> ServerVersion {
        ServerVersion {
            id: VersionId::new(id),
            kind,
            variant: ServerVariant::Vanilla,
        }
    }

    /// 标签必须包含 id 原文：`select_filter` 归一后按标签全名回映 id
    #[test]
    fn version_label_marks_kind_but_keeps_id_intact() {
        assert_eq!(
            version_label(&version(VersionKind::Release, "1.21.1")),
            "1.21.1"
        );
        assert_eq!(
            version_label(&version(VersionKind::Snapshot, "24w33a")),
            "24w33a（快照）"
        );
        assert_eq!(
            version_label(&version(VersionKind::Historical, "1.0")),
            "1.0（旧版）"
        );
    }
}
