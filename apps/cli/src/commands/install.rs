use crate::bootstrap;
use crate::output::Printer;
use crate::progress::{InstallSession, TaskSpinner};
use crate::prompts;
use anyhow::{Context, Result};
use mcsup::domain::server::version::{ServerVariant, ServerVersion, VersionId, VersionKind};
use mcsup::file::FileCoreInspector;
use mcsup::port::ServerJarSource;
use mcsup::port::WorkspaceRepository as _;
use mcsup::service::ServerInstaller;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;

/// 一次安装的结果：本地与远端同一形状，供入口统一打印
struct Installed {
    /// 实际安装的版本
    version: VersionId,
    /// 核心 / 安装器产物的字节数
    size: u64,
    /// 落点（本地路径，或对端机器上的路径）
    destination: PathBuf,
}

/// 按注册表定位服务器并安装 / 更换服务端核心
///
/// 目标目录 = 注册表里该服务器的目录（`{workspace}/servers/{name}/server.jar`），
/// 来源按服务端变体做能力匹配；当前工作目录不参与定位；安装完成回写 `meta.jar`。
///
/// 版本：`--version` 直用；核心尚未装成时沿用注册表里已记录的版本（下载失败后
/// 重试不再重选）；其余交互下从版本列表选择，非交互保留 `latest`
/// （由来源解析为最新稳定版）。
///
/// 落点（本地 / 某台远端）只决定「在哪里执行」；进度回显、阶段文案与结果打印
/// 在两侧完全一致——远端把同一组 `TaskEvent` 经协议帧回传，本地入口用同一个
/// `InstallSession` 渲染。
///
/// # Errors
///
/// 服务器不存在（3）、版本不存在（3）、来源或传输/校验失败（5）、其他安装失败（1）时返回错误
pub(crate) async fn run(
    name: &str,
    version: Option<&str>,
    _cfg: &crate::config::ConfigFile,
    printer: Printer,
) -> Result<()> {
    // 远端服务器：核心下载、校验、（安装器类变体的）安装器执行全部在对端
    if let Some(installed) = install_remote(name, version, printer)? {
        return printer.print_installed(&installed.version, installed.size, &installed.destination);
    }
    let registry = bootstrap::open_registry()?;
    let srv = registry.get(name).with_context(|| {
        crate::i18n::pick(
            printer.lang(),
            format!("服务器 {name} 不存在"),
            format!("server {name} does not exist"),
        )
    })?;
    let source = bootstrap::source_for(&srv.meta.variant, printer.lang())?;
    let version_id = match version {
        Some(raw) => VersionId::new(normalize_version_alias(raw).to_owned()),
        None => match default_version_source(srv.meta.jar.is_some(), printer.is_interactive()) {
            VersionSource::Recorded => srv.meta.version.clone(),
            VersionSource::Prompt => {
                pick_version(source.as_ref(), &srv.meta.variant, printer).await?
            }
            VersionSource::Latest => VersionId::new("latest"),
        },
    };
    let (size, destination) =
        download_jar(name, &version_id, source, srv.meta.variant, printer).await?;
    printer.print_installed(&version_id, size, &destination)
}

/// 远端安装：目标落在远端时整段转发到对端
///
/// 返回 `Ok(Some(..))` 表示已由远端处理；`Ok(None)` 表示本地条目，调用方继续走
/// 本地路径。未限定来源时与其它命令同一口径：本地优先，其次唯一同名远端（多台
/// 重名报歧义）。失败时不重试——下载 / 校验失败是对端的事实，重试语义由用户
/// 再次执行决定。
///
/// 进度：与本地同一个 [`InstallSession`]，对端经 `Response::Task` 帧回传阶段、
/// 字节进度与安装器输出，本地只负责画。
///
/// # Errors
///
/// 服务器不存在（3）、远端拒绝或传输失败（5）、未授权（6）时返回错误
fn install_remote(
    name: &str,
    version: Option<&str>,
    printer: Printer,
) -> Result<Option<Installed>> {
    let lang = printer.lang();
    let Some(target) = super::origin::remote_target(name, lang)? else {
        return Ok(None);
    };
    // 未给版本时沿用在册版本：让重试失败的核心安装不会悄悄换版本
    let version = version.unwrap_or_else(|| target.meta.version.as_str());
    let message = crate::i18n::pick(
        lang,
        format!(
            "在 {} 安装 {} {version}",
            target.member.name, target.meta.name
        ),
        format!(
            "installing {} {version} on {}",
            target.meta.name, target.member.name
        ),
    );
    let session = InstallSession::new(message, printer.is_human());
    let result = target
        .member
        .servers
        .install_with(target.meta.name.as_str(), version, &session);
    session.finish();
    let meta = result.with_context(|| {
        crate::i18n::pick(
            lang,
            format!(
                "在 {} 安装 {} 的核心失败",
                target.member.name, target.meta.name
            ),
            format!(
                "failed to install the core of {} on {}",
                target.meta.name, target.member.name
            ),
        )
    })?;
    let Some(jar) = meta.jar.as_ref() else {
        return Err(anyhow::anyhow!(
            "{}",
            crate::i18n::pick(
                lang,
                "对端安装完成但注册记录缺少核心元数据",
                "the peer finished the install but the record has no core metadata"
            )
        ));
    };
    let destination = server_dir_of(&target.member, &meta)?;
    Ok(Some(Installed {
        version: meta.version,
        size: jar.size,
        destination,
    }))
}

/// 远端服务器的目录展示口径：`{workspace path}/servers/{rel_dir}` 在对端的
/// 绝对路径，由镜像侧记录拼出；拼不出时退回限定名
fn server_dir_of(
    member: &mcsup::backend::remote::RemoteMember,
    meta: &mcsup::domain::server::meta::ServerMeta,
) -> Result<PathBuf> {
    let workspaces = member.workspaces.list()?;
    let ws = workspaces
        .iter()
        .find(|ws| ws.id.as_str() == meta.workspace_id.as_str());
    Ok(ws.map_or_else(
        || PathBuf::from(meta.rel_dir.to_string_lossy().into_owned()),
        |ws| ws.path.join("servers").join(meta.rel_dir.clone()),
    ))
}

/// 版本别名归一：`lts` 与 `latest` 同义
///
/// 别名只在入口归一到来源已有的保留名，来源侧仍只认 `latest` /
/// `latest-snapshot` 两个保留名（`port::ServerJarSource::artifact` 的契约）。
#[must_use]
pub(crate) fn normalize_version_alias(raw: &str) -> &str {
    if raw.eq_ignore_ascii_case("lts") {
        "latest"
    } else {
        raw
    }
}

/// 未显式传 `--version` 时版本的来源
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VersionSource {
    /// 沿用注册表记录的版本：核心还没装成（下载失败 / 被清理）时重试不重选
    Recorded,
    /// 交互选择
    Prompt,
    /// 非交互缺省 `latest`
    Latest,
}

/// 缺省版本的来源判定：核心未装成 > 交互 > 非交互 `latest`
fn default_version_source(has_jar: bool, interactive: bool) -> VersionSource {
    if !has_jar {
        VersionSource::Recorded
    } else if interactive {
        VersionSource::Prompt
    } else {
        VersionSource::Latest
    }
}

/// 下载并登记服务端核心；不打印结果行（由调用方输出，保证 JSON 单行契约）
///
/// 经 lifecycle 编排：运行中换核心被拒绝（Linux 上 rename 会成功但 JVM
/// 仍用旧 inode，用户以为升级了实际没有），判定收敛在 service。
///
/// `Forge` / `NeoForge` 的下载物是安装器，需要先跑 `--installServer`：调用方在此
/// 按与服务端启动同一套口径挑好 java，连同 [`LocalInstallerRunner`] 组成
/// [`InstallerContext`] 交给 service。
///
/// 只有安装器类来源（[`ServerVariant::Forge`] / [`ServerVariant::NeoForge`]）
/// 才要求 Java：直接可运行的来源（原版 / Fabric / Paper）不读上下文，因此
/// 缺 Java 不影响它们——判定按 `variant` 收敛在这里，不散到每个来源。
///
/// # Errors
///
/// 服务器不存在、版本不存在、来源不可达、传输 / 校验失败、安装器执行失败、
/// 安装器类来源但本机没有可用 Java，或运行中拒绝时返回错误
pub(crate) async fn download_jar(
    name: &str,
    version_id: &VersionId,
    source: Arc<dyn ServerJarSource>,
    variant: ServerVariant,
    printer: Printer,
) -> Result<(u64, PathBuf)> {
    let downloader = Arc::new(bootstrap::build_downloader()?);
    let installer = ServerInstaller::new(source, downloader);
    let inspector = FileCoreInspector::new();
    let lifecycle = super::lifecycle::lifecycle_service()?;

    // 解析版本到首字节之间没有任何进度回调，先挂 spinner 保证整段有回显；
    // 首个进度到达后由 session 内的 BarSink 接手同一条线，切换成进度显示。
    // 下载结束后还有运行安装器（几分钟）与登记核心：安装器的输出经
    // `installer_output` 逐行滚动显示，进度条不会停在 100% 冻住。
    let message = crate::i18n::pick(
        printer.lang(),
        format!("解析并下载 {version_id}"),
        format!("resolving and downloading {version_id}"),
    );
    let session = InstallSession::new(message, printer.is_human());

    // 安装器类来源才需要 java。这里**不能**把失败吞成 `None`：service 会把
    // 「没给上下文」报成笼统的 `NoJavaForInstaller`，用户永远看不到真正原因
    // 是「本机没装 Java」还是「装的 Java 与大版本要求不符」。
    let installer_ctx = if variant.needs_installer() {
        // 先把名字取出来：闭包会在 `variant` 之后还要读它，直接捕获会与
        // `lifecycle` 的借用冲突
        let variant_name = variant.as_str().to_owned();
        let java_bin = lifecycle
            .installer_java_bin(name, &mut || {}, &mut |_| {})
            .with_context(|| {
                crate::i18n::pick(
                    printer.lang(),
                    format!("{variant_name} 需要先运行安装器，但找不到可用的 Java 运行时"),
                    format!(
                        "{variant_name} needs to run its installer first, but no usable Java runtime was found"
                    ),
                )
            })?;
        Some(mcsup::service::InstallerContext {
            java_bin,
            runner: Arc::new(mcsup::runtime::LocalInstallerRunner::new()),
        })
    } else {
        None
    };

    info!(version = %version_id, server = name, "开始安装");
    let outcome = lifecycle
        .install_jar_checked(
            name,
            version_id,
            &installer,
            &inspector,
            installer_ctx.as_ref(),
            Some(&session),
            session.cancel(),
        )
        .await
        .with_context(|| {
            crate::i18n::pick(
                printer.lang(),
                format!("安装服务端 {name} 失败"),
                format!("failed to install the server core for {name}"),
            )
        })?;
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
    variant: &ServerVariant,
    printer: Printer,
) -> Result<VersionId> {
    let spinner = TaskSpinner::new(
        crate::i18n::pick(
            printer.lang(),
            format!("获取 {} 版本列表", variant.as_str()),
            format!("fetching the {} version list", variant.as_str()),
        ),
        printer.is_human(),
    );
    let versions = source.versions().await.context(crate::i18n::pick(
        printer.lang(),
        "获取版本列表失败",
        "failed to fetch the version list",
    ))?;
    spinner.finish();
    select_version(versions, variant.as_str(), printer)
}

/// 从已取回的版本列表里交互选择（本地与远端共用）
///
/// 远端向导已经从对端拿到了版本列表，只需要「选择」这一步；抽出来让两端
/// 排序、标签与过滤开关完全一致，避免远端另写一套选择逻辑。
pub(crate) fn select_version(
    mut versions: Vec<ServerVersion>,
    label: &str,
    printer: Printer,
) -> Result<VersionId> {
    if versions.is_empty() {
        anyhow::bail!(crate::i18n::pick(
            printer.lang(),
            format!("{label} 暂无可用版本"),
            format!("no versions available for {label}")
        ));
    }
    // 顺序由领域层统一：各来源给出的顺序不一致（Forge 的 BTreeMap 是串字典序，
    // `1.10` 会落在 `1.1` 之后），入口不再各自排序
    ServerVersion::sort_display_order(&mut versions);
    let choices = version_choices(printer.lang(), &versions);
    let picked = prompts::select_filter_scoped(
        printer.lang(),
        &crate::i18n::pick(printer.lang(), "选择版本", "select a version"),
        &choices,
        prompts::ScopeHint {
            show_all: crate::i18n::pick(printer.lang(), "显示所有版本", "show all versions").leak(),
            show_default: crate::i18n::pick(printer.lang(), "只显示正式版", "releases only").leak(),
        },
    )?;
    versions
        .into_iter()
        .zip(choices)
        .find(|(_, choice)| choice.label == picked)
        .map(|(v, _)| v.id)
        .ok_or_else(|| {
            anyhow::anyhow!(crate::i18n::pick(
                printer.lang(),
                format!("无效选择: {picked}"),
                format!("invalid choice: {picked}"),
            ))
        })
}

/// 版本候选：默认只列正式版，快照 / 旧版标为扩展候选（由 Ctrl+A 切换显示）
fn version_choices(
    lang: crate::locale::Lang,
    versions: &[ServerVersion],
) -> Vec<prompts::ScopedChoice> {
    versions
        .iter()
        .map(|version| {
            let extra = !matches!(version.kind, VersionKind::Release);
            prompts::ScopedChoice::new(version_label(lang, version), extra)
        })
        .collect()
}

/// 版本候选的展示标签（含版本 id，映射回 id 时按标签全名匹配）
#[must_use]
pub(crate) fn version_label(lang: crate::locale::Lang, version: &ServerVersion) -> String {
    match version.kind {
        VersionKind::Release => version.id.to_string(),
        VersionKind::Snapshot => crate::i18n::pick(
            lang,
            format!("{}（快照）", version.id),
            format!("{} (snapshot)", version.id),
        ),
        // `VersionKind` 是 `#[non_exhaustive]`：`Historical` 与将来新增的种类
        // 都按旧版展示，不 panic
        _ => crate::i18n::pick(
            lang,
            format!("{}（旧版）", version.id),
            format!("{} (legacy)", version.id),
        ),
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
            java_major: None,
        }
    }

    /// 标签必须包含 id 原文：`select_filter` 归一后按标签全名回映 id
    #[test]
    fn version_label_marks_kind_but_keeps_id_intact() {
        assert_eq!(
            version_label(
                crate::locale::Lang::Zh,
                &version(VersionKind::Release, "1.21.1")
            ),
            "1.21.1"
        );
        assert_eq!(
            version_label(
                crate::locale::Lang::Zh,
                &version(VersionKind::Snapshot, "24w33a")
            ),
            "24w33a（快照）"
        );
        assert_eq!(
            version_label(
                crate::locale::Lang::Zh,
                &version(VersionKind::Historical, "1.0")
            ),
            "1.0（旧版）"
        );
    }

    /// 正式版为默认候选，快照 / 旧版标为扩展候选（由 Ctrl+A 切换显示）
    #[test]
    fn version_choices_marks_non_release_as_extra() {
        let versions = vec![
            version(VersionKind::Release, "1.21.1"),
            version(VersionKind::Snapshot, "24w33a"),
            version(VersionKind::Historical, "1.0"),
        ];
        let choices = version_choices(crate::locale::Lang::Zh, &versions);
        assert_eq!(choices.len(), 3);
        assert_eq!(choices[0].label, "1.21.1");
        assert!(!choices[0].extra, "正式版是默认候选");
        assert_eq!(choices[1].label, "24w33a（快照）");
        assert!(choices[1].extra);
        assert_eq!(choices[2].label, "1.0（旧版）");
        assert!(choices[2].extra);
    }

    /// `lts` 与 `latest` 同义；其余（含 `latest-snapshot`）原样保留
    #[test]
    fn lts_alias_maps_to_latest() {
        assert_eq!(normalize_version_alias("lts"), "latest");
        assert_eq!(normalize_version_alias("LTS"), "latest");
        assert_eq!(normalize_version_alias("latest"), "latest");
        assert_eq!(
            normalize_version_alias("latest-snapshot"),
            "latest-snapshot"
        );
        assert_eq!(normalize_version_alias("1.21.1"), "1.21.1");
    }

    /// 缺省版本来源：核心未装成时沿用记录版本（重试不重选），否则按交互性
    #[test]
    fn default_version_source_prefers_recorded_when_core_missing() {
        // 下载失败 / 已被清理：无论是否交互都沿用记录版本
        assert_eq!(default_version_source(false, true), VersionSource::Recorded);
        assert_eq!(
            default_version_source(false, false),
            VersionSource::Recorded
        );
        // 已装成核心：交互重选，非交互沿用 latest 口径
        assert_eq!(default_version_source(true, true), VersionSource::Prompt);
        assert_eq!(default_version_source(true, false), VersionSource::Latest);
    }
}
