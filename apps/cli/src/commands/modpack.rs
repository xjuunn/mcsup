//! `server create --variant modrinth-modpack` 的整合包选择与布置
//!
//! 这个模块只服务「按整合包建服务端」这一条路径，因此不对外暴露子命令。
//! 流程：
//!
//! 1. 搜索**能在服务端运行**的整合包（可再加关键词过滤）
//! 2. 展示所选整合包的详情（标题、作者、加载器、MC 版本、下载量、简介、来源页）
//! 3. 选版本（默认只列正式版）
//! 4. 返回清单里的 MC 版本与版本 ID，交给 `server create` 落库
//! 5. 创建后由 [`install_modpack`] 完成「取回 → 装加载器 → 铺 overrides → 装文件」
//!
//! # 回显
//!
//! 搜索、取详情、下载都是网络操作：每一步都有 spinner，下载阶段切进度条。
//! `--output json` 下全部隐藏。

use crate::bootstrap;
use crate::output::Printer;
use crate::progress::{BarSink, TaskSpinner};
use anyhow::{Context, Result};
use comfy_table::{ContentArrangement, Table};
use mcsup::port::CancellationToken;
use mcsup::port::modpack::{ModpackDetail, ModpackQuery, ModpackVersion};
use mcsup::service::ModpackInstallOptions;
use std::path::Path;

/// 已选定的整合包
#[derive(Debug, Clone)]
pub(crate) struct PickedModpack {
    /// 项目 ID
    pub project_id: String,
    /// 项目标题
    pub title: String,
    /// 版本 ID
    pub version_id: String,
    /// 版本号
    pub version_number: String,
    /// 清单声明的 MC 版本
    pub minecraft: String,
}

/// 选择整合包：搜索 → 详情 → 版本
///
/// `version_hint` 来自 `--version`：给出时**只**用它挑版本，跳过搜索与交互
/// （脚本场景需要可复现）。项目本身仍需交互选择或由 `--version` 隐含？
/// 不——项目必须由用户给出，因此非交互下用 `--variant modrinth-modpack` 且
/// 没有指定项目会报用法错误，见 [`pick_modpack`] 的调用方。
///
/// # Errors
///
/// 上游失败、没有可开服的整合包、没有版本或用户中断时返回错误
pub(crate) async fn pick_modpack(
    project_hint: Option<&str>,
    version_hint: Option<&str>,
    printer: Printer,
) -> Result<PickedModpack> {
    let service = bootstrap::modpack_service()?;
    // 显式给了项目（`--modpack`）：跳过搜索，直接取详情
    if let Some(project) = project_hint {
        let detail = service.select_source(None)?.detail(project).await;
        print_detail(&detail.context("获取整合包详情失败")?, printer)?;
        let versions = service.select_source(None)?.versions(project).await?;
        let version = match version_hint {
            Some(hint) => versions
                .into_iter()
                .find(|version| version.id == hint || version.version_number == hint)
                .ok_or_else(|| anyhow::anyhow!("整合包 {project} 没有版本 {hint}"))?,
            None if printer.is_interactive() => pick_version(&versions, printer)?,
            None => versions
                .iter()
                .find(|version| version.is_release())
                .or_else(|| versions.first())
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("整合包 {project} 没有可用版本"))?,
        };
        return finish_pick(&service, project, version).await;
    }
    let spinner = TaskSpinner::new("搜索可开服的整合包", printer.is_human());
    let search = service
        .select_source(None)?
        .search(&ModpackQuery::new().page(0, 30))
        .await;
    spinner.finish();
    let search = search.context("搜索整合包失败")?;
    if search.hits.is_empty() {
        anyhow::bail!("没有找到可在服务端运行的整合包");
    }

    // 交互：先按关键词过滤出候选，再选择
    let picked_summary = if let Some(keyword) = prompt_keyword(printer)? {
        let filtered: Vec<String> = search
            .hits
            .iter()
            .filter(|hit| {
                hit.title.to_lowercase().contains(&keyword)
                    || hit
                        .slug
                        .as_deref()
                        .is_some_and(|slug| slug.contains(&keyword))
            })
            .map(|hit| format!("{}（{}）", hit.title, hit.id))
            .collect();
        if filtered.is_empty() {
            anyhow::bail!("没有匹配「{keyword}」的整合包");
        }
        filtered
    } else {
        search
            .hits
            .iter()
            .map(|hit| format!("{}（{}）", hit.title, hit.id))
            .collect()
    };
    let choice = crate::prompts::select_filter("选择整合包", &picked_summary)?;
    let id = extract_id(&choice).ok_or_else(|| anyhow::anyhow!("无效选择: {choice}"))?;

    // 详情：把「这个包是什么」写给用户看
    let spinner = TaskSpinner::new(format!("获取 {id} 详情"), printer.is_human());
    let detail = service.select_source(None)?.detail(&id).await;
    spinner.finish();
    let detail = detail.context("获取整合包详情失败")?;
    print_detail(&detail, printer)?;

    // 版本：默认正式版优先，`--version` 直接指定
    let versions = service.select_source(None)?.versions(&id).await?;
    let version = match version_hint {
        Some(hint) => versions
            .into_iter()
            .find(|version| version.id == hint || version.version_number == hint)
            .ok_or_else(|| anyhow::anyhow!("整合包 {id} 没有版本 {hint}"))?,
        None => pick_version(&versions, printer)?,
    };

    finish_pick(&service, &id, version).await
}

/// 取回清单拿到 MC 版本，组装 [`PickedModpack`]
///
/// 清单里的 MC 版本只有下载并解析后才拿得到，而注册表需要在创建时就知道它，
/// 因此这里先做一次「取回 + 读清单」；真正的布置发生在 `server create` 落库
/// 之后，用同一份版本重新取回（本地缓存命中时不重复下载）。
async fn finish_pick(
    service: &mcsup::service::ModpackService,
    project: &str,
    version: ModpackVersion,
) -> Result<PickedModpack> {
    let prepared = service
        .prepare(
            project,
            ModpackInstallOptions::new(std::env::temp_dir().as_path()).version(&version.id),
            &CancellationToken::new(),
        )
        .await
        .context("读取整合包清单失败")?;
    let minecraft = prepared
        .minecraft_version()
        .map_err(|err| anyhow::anyhow!(err.to_string()))?
        .to_owned();
    // 暂存目录用后就删：真正的产物在 server create 之后由 install_modpack 重新取回
    if let Some(staging) = prepared.archive_path.parent() {
        let _ = std::fs::remove_dir_all(staging);
    }
    Ok(PickedModpack {
        project_id: prepared.resolved.project_id,
        title: project.to_owned(),
        version_id: prepared.resolved.version_id,
        version_number: prepared.resolved.version_number,
        minecraft,
    })
}

/// 让用户先输关键词（可留空表示浏览全部）
fn prompt_keyword(printer: Printer) -> Result<Option<String>> {
    if !printer.is_interactive() {
        return Ok(None);
    }
    let keyword = crate::prompts::input_text("搜索整合包（回车浏览全部）", "", "")?;
    if keyword.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(keyword.to_lowercase()))
    }
}

/// 从 `标题（ID）` 里取回 ID
///
/// 标题里可能含全角括号，因此从**最后一个**左括号开始切；拿不到就返回
/// `None`，由调用方报无效选择。
fn extract_id(label: &str) -> Option<String> {
    let start = label.rfind('（')?;
    let end = label.rfind('）')?;
    if end <= start + '（'.len_utf8() {
        return None;
    }
    Some(label[start + '（'.len_utf8()..end].to_owned())
}

/// 整合包详情的 JSON 形状
#[derive(serde::Serialize)]
struct DetailJson<'a> {
    id: &'a str,
    slug: Option<&'a str>,
    title: &'a str,
    author: Option<&'a str>,
    loaders: &'a [String],
    game_versions: &'a [String],
    downloads: u64,
    license: Option<&'a str>,
    page_url: &'a str,
    source: &'a str,
}

/// 展示整合包详情
fn print_detail(detail: &ModpackDetail, printer: Printer) -> Result<()> {
    let summary = &detail.summary;
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(["项", "值"]);
    table.add_row(["标题", summary.title.as_str()]);
    table.add_row(["ID", summary.id.as_str()]);
    if let Some(slug) = summary.slug.as_deref() {
        table.add_row(["slug", slug]);
    }
    if let Some(author) = summary.author.as_deref() {
        table.add_row(["作者", author]);
    }
    table.add_row(["加载器", &summary.loaders.join(", ")]);
    table.add_row(["游戏版本", &join_versions(&summary.game_versions)]);
    table.add_row(["下载量", &summary.downloads.to_string()]);
    if let Some(license) = summary.license.as_deref() {
        table.add_row(["许可证", license]);
    }
    table.add_row(["来源", detail.source_name]);
    table.add_row(["页面", detail.page_url.as_str()]);
    table.add_row(["简介", summary.description.as_str()]);

    if printer.is_human() {
        crate::output::print_table(&table);
        // 长描述只取前几行：终端里刷几百行 Markdown 没有意义
        let excerpt: String = detail
            .body
            .lines()
            .filter(|line| !line.trim().is_empty())
            .take(6)
            .collect::<Vec<_>>()
            .join("\n");
        if !excerpt.is_empty() {
            printer.print_section(&excerpt);
        }
        return Ok(());
    }
    // JSON：输出详情对象，供脚本读取
    let payload = DetailJson {
        id: &summary.id,
        slug: summary.slug.as_deref(),
        title: &summary.title,
        author: summary.author.as_deref(),
        loaders: &summary.loaders,
        game_versions: &summary.game_versions,
        downloads: summary.downloads,
        license: summary.license.as_deref(),
        page_url: &detail.page_url,
        source: detail.source_name,
    };
    printer.print_outcome("", &payload)
}

/// 版本列表里展示的版本号上限（防止终端刷屏）
const VERSION_SHOW_LIMIT: usize = 60;

/// 选版本：默认只列正式版，选不到再放开测试版
fn pick_version(versions: &[ModpackVersion], _printer: Printer) -> Result<ModpackVersion> {
    if versions.is_empty() {
        anyhow::bail!("该整合包没有可用版本");
    }
    let releases: Vec<&ModpackVersion> = versions.iter().filter(|v| v.is_release()).collect();
    // 没有正式版时才退回全部版本（含测试版）
    let pool: Vec<&ModpackVersion> = if releases.is_empty() {
        versions.iter().collect()
    } else {
        releases
    };
    let labels: Vec<String> = pool
        .iter()
        .take(VERSION_SHOW_LIMIT)
        .map(|version| {
            format!(
                "{}（{} · MC {} · {}）",
                version.version_number,
                version.channel,
                join_versions(&version.game_versions),
                version.loaders.join(",")
            )
        })
        .collect();
    if labels.len() == 1 {
        return Ok((*pool[0]).clone());
    }
    let picked = crate::prompts::select_filter("选择整合包版本", &labels)?;
    pool.iter()
        .find(|version| {
            format!(
                "{}（{} · MC {} · {}）",
                version.version_number,
                version.channel,
                join_versions(&version.game_versions),
                version.loaders.join(",")
            ) == picked
        })
        .map(|version| (*version).clone())
        .ok_or_else(|| anyhow::anyhow!("无效选择: {picked}"))
}

/// 版本列表压缩展示：超过 6 个用 `…` 收尾
fn join_versions(versions: &[String]) -> String {
    const SHOW: usize = 6;
    if versions.len() <= SHOW {
        return versions.join(", ");
    }
    format!("{} 等 {} 个", versions[..SHOW].join(", "), versions.len())
}

/// 把整合包布置到服务端目录
///
/// 顺序：装加载器核心 → 铺 overrides → 下载清单文件。核心安装复用
/// `download_jar`（与 `server install` 同一套流程），因此 Forge / `NeoForge`
/// 的安装器运行、Java 选择都不会出现两套逻辑。
///
/// # Errors
///
/// 取回失败、清单非法、核心安装失败、文件下载失败时返回错误
pub(crate) async fn install_modpack(
    name: &str,
    server_dir: &Path,
    picked: &PickedModpack,
    printer: Printer,
) -> Result<()> {
    let service = bootstrap::modpack_service()?;
    let source = service.select_source(None)?;

    // 1. 取回并读清单（此时才拿到 loader 版本）
    let spinner = TaskSpinner::new(
        format!("取回整合包 {} {}", picked.title, picked.version_number),
        printer.is_human(),
    );
    let prepared = service
        .prepare(
            &picked.project_id,
            ModpackInstallOptions::new(server_dir).version(&picked.version_id),
            &CancellationToken::new(),
        )
        .await
        .context("取回整合包失败")?;
    spinner.finish();

    // 2. 装加载器核心：清单声明的 loader 决定变体
    let variant = prepared
        .require_loader()
        .map_err(|err| anyhow::anyhow!(err.to_string()))?;
    let core_source = bootstrap::source_for(&variant, printer.lang())?;
    let version_id = mcsup::domain::server::version::VersionId::new(
        prepared
            .minecraft_version()
            .map_err(|err| anyhow::anyhow!(err.to_string()))?,
    );
    super::install::download_jar(name, &version_id, core_source, variant, printer).await?;

    // 3. 铺 overrides + 下载清单文件
    let label = format!("布置整合包 {name}");
    let spinner = TaskSpinner::new(label.clone(), printer.is_human());
    let sink = BarSink::new(spinner.handle(), label);
    let cancel = CancellationToken::new();
    let result = service
        .apply(&prepared, server_dir, Some(&sink), &cancel)
        .await;
    spinner.finish();
    let applied = result?;

    // 4. 暂存目录（.mrpack）用完即删
    if let Some(staging) = prepared.archive_path.parent() {
        let _ = std::fs::remove_dir_all(staging);
    }

    let payload = ModpackInstallJson {
        source: source.name(),
        project_id: &picked.project_id,
        version_id: &picked.version_id,
        version_number: &picked.version_number,
        minecraft: &picked.minecraft,
        loader: prepared
            .index
            .loader()
            .map(|(loader, version)| format!("{loader} {version}")),
        files: applied.file_count,
        downloaded_bytes: applied.downloaded_bytes,
        reused: applied.reused,
        path: server_dir.display().to_string(),
    };
    let human = crate::i18n::pick(
        printer.lang(),
        format!(
            "已按整合包 {} {} 布置服务端：{} 个文件，下载 {}",
            picked.title,
            picked.version_number,
            applied.file_count,
            crate::output::human_bytes(applied.downloaded_bytes)
        ),
        format!(
            "applied modpack {} {}: {} files, downloaded {}",
            picked.title,
            picked.version_number,
            applied.file_count,
            crate::output::human_bytes(applied.downloaded_bytes)
        ),
    );
    printer.print_outcome(&human, &payload)
}

/// 整合包布置结果的 JSON 形状
#[derive(serde::Serialize)]
struct ModpackInstallJson<'a> {
    source: &'a str,
    project_id: &'a str,
    version_id: &'a str,
    version_number: &'a str,
    minecraft: &'a str,
    loader: Option<String>,
    files: usize,
    downloaded_bytes: u64,
    reused: usize,
    path: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_id_reads_trailing_id() {
        assert_eq!(
            extract_id("Fabulously Optimized（1KVo5zza）").as_deref(),
            Some("1KVo5zza")
        );
        assert_eq!(extract_id("no id here"), None);
        assert_eq!(extract_id("bad（）"), None);
    }

    #[test]
    fn join_versions_truncates_long_lists() {
        let short = vec!["1.21.1".to_owned()];
        assert_eq!(join_versions(&short), "1.21.1");
        let long: Vec<String> = (0..10).map(|i| format!("1.{i}")).collect();
        let joined = join_versions(&long);
        assert!(joined.contains("等 10 个"), "{joined}");
    }
}
