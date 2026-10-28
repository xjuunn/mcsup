//! `server create --variant modrinth-modpack` 的整合包选择与布置
//!
//! 这个模块只服务「按整合包建服务端」这一条路径，因此不对外暴露子命令。
//! 流程：
//!
//! 1. 打开即展示**能在服务端运行**的整合包默认列表（上游按下载量排序），
//!    输入框与列表同屏；输入关键字回车即按关键词远端搜索
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
use crate::diagnostic::AppDiagnostic;
use crate::output::Printer;
use crate::progress::{BarSink, TaskSpinner};
use crate::prompts::{Backable, EnterMode, ScopeHint, ScopedChoice, select_filter_backable};
use anyhow::{Context, Result};
use console::measure_text_width;
use mcsup::domain::server::variant::ServerVariant;
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
    /// 清单加载器对应的服务端变体
    ///
    /// 注册表存这个而不是 `modrinth-modpack`：之后 `server install` 要按变体
    /// 找核心来源，`modrinth-modpack` 本身没有可下载的核心。
    pub variant: ServerVariant,
}

/// 每页整合包数量
const PAGE_SIZE: u32 = 20;

/// 结果列表里的翻页项：用不可能出现在标题里的前缀，避免与真实整合包重名
const NEXT_PAGE: &str = "→ 下一页";
const PREV_PAGE: &str = "← 上一页";

/// 向导状态
///
/// 显式状态机而非嵌套循环：每一步只回答「按 Esc 回到哪」，返回路径因此是
/// 可见的、可测试的，不会因为嵌套层次变深而漏掉某个返回分支。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Wizard {
    /// 浏览一页结果；`keyword` 为空即上游默认推荐列表
    Results { keyword: Option<String>, page: u32 },
    /// 已选定整合包：详情与版本列表在同一屏（Esc 回到结果页）
    Modpack {
        id: String,
        keyword: Option<String>,
        page: u32,
    },
}

/// 向导起点：直接展示来源的默认列表
///
/// 上游在**没有关键词**时返回自己的默认列表（Modrinth 按下载量排序），
/// 输入框与列表同屏：不输关键字就是推荐列表，输入后回车转成搜索结果。
fn initial_state() -> Wizard {
    Wizard::Results {
        keyword: None,
        page: 0,
    }
}

/// 选择整合包：浏览 / 搜索 → 结果 → 详情并选版本
///
/// 交互路径由 [`Wizard`] 驱动，Esc 逐级返回；`--modpack` 给出项目时跳过
/// 搜索与详情，直接选版本。非交互模式必须用 `--modpack` 指定项目，否则
/// 没有任何可自动选择的信息。
///
/// `version_hint` 来自 `--version`：给出时只按它挑版本，不进入交互选择
/// （脚本场景需要可复现）。
///
/// # Errors
///
/// 上游失败、没有可开服的整合包、没有版本，或用户在搜索步取消时返回错误
pub(crate) async fn pick_modpack(
    project_hint: Option<&str>,
    version_hint: Option<&str>,
    printer: Printer,
) -> Result<PickedModpack> {
    let service = bootstrap::modpack_service()?;
    if let Some(project) = project_hint {
        return pick_by_project(&service, project, version_hint, printer).await;
    }
    if !printer.is_interactive() {
        anyhow::bail!(
            "{}",
            crate::i18n::pick(
                printer.lang(),
                "非交互模式请用 --modpack 指定整合包".to_owned(),
                "use --modpack to pick a modpack in non-interactive mode".to_owned(),
            )
        );
    }
    run_wizard(&service, version_hint, printer).await
}

/// 显式给了 `--modpack`：跳过搜索，直接取详情与版本
async fn pick_by_project(
    service: &mcsup::service::ModpackService,
    project: &str,
    version_hint: Option<&str>,
    printer: Printer,
) -> Result<PickedModpack> {
    // 详情与版本一起取回、取完再打印：分两次取会让详情先上屏，随后才出现
    // 版本选择，看起来闪一下。
    let source = service.select_source(None)?;
    let spinner = TaskSpinner::new(format!("获取 {project} 详情与版本"), printer.is_human());
    let (detail, versions) = tokio::join!(source.detail(project), source.versions(project));
    spinner.finish();
    let detail = detail.context("获取整合包详情失败")?;
    let versions = versions.context("获取整合包版本失败")?;
    print_detail(&detail, printer)?;
    let title = detail.summary.title.clone();
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
    finish_pick(service, project, &title, version).await
}

/// 交互向导主循环：状态转移全部集中在这里
async fn run_wizard(
    service: &mcsup::service::ModpackService,
    version_hint: Option<&str>,
    printer: Printer,
) -> Result<PickedModpack> {
    let mut state = initial_state();
    loop {
        state = match state {
            Wizard::Results { keyword, page } => {
                match browse_results(service, keyword.as_deref(), page, printer).await? {
                    // 搜索结果页按 Esc：回到推荐列表，而不是退出整个命令
                    Browse::Back if keyword.is_some() => Wizard::Results {
                        keyword: None,
                        page: 0,
                    },
                    // 推荐列表本身再按 Esc：没有上一层可选，取消本次创建
                    Browse::Back => return Err(AppDiagnostic::cancelled().into()),
                    Browse::Search(next) => Wizard::Results {
                        keyword: Some(next),
                        page: 0,
                    },
                    Browse::Page(next) => Wizard::Results {
                        keyword,
                        page: next,
                    },
                    Browse::Pick(id) => Wizard::Modpack { id, keyword, page },
                }
            }
            Wizard::Modpack { id, keyword, page } => {
                // 详情与版本同屏：两者一起取回、一起呈现，打印完详情紧接着就是
                // 版本选择；先打印再取版本会让简介和版本列表分两次出现。
                let source = service.select_source(None)?;
                let spinner = TaskSpinner::new(format!("获取 {id} 详情与版本"), printer.is_human());
                let (detail, versions) = tokio::join!(source.detail(&id), source.versions(&id));
                spinner.finish();
                let detail = detail.context("获取整合包详情失败")?;
                let versions = versions.context("获取整合包版本失败")?;
                print_detail(&detail, printer)?;
                let title = detail.summary.title.clone();

                // `--version` 已指定则直接取；否则逐版选择（Esc 回结果页）
                let picked = if let Some(hint) = version_hint {
                    versions
                        .into_iter()
                        .find(|version| version.id == hint || version.version_number == hint)
                        .ok_or_else(|| anyhow::anyhow!("整合包 {id} 没有版本 {hint}"))?
                } else {
                    let Some(version) = pick_version_backable(printer.lang(), &versions)? else {
                        state = Wizard::Results { keyword, page };
                        continue;
                    };
                    version
                };
                // 选中版本即开始创建，不再额外确认
                return finish_pick(service, &id, &title, picked).await;
            }
        };
    }
}

/// 浏览一页结果后用户的去向
enum Browse {
    /// Esc：搜索结果页回到推荐列表；推荐列表本身由调用方决定是否取消
    Back,
    /// 输入了关键词：按它重新搜索第一页
    Search(String),
    /// 翻到第 `page` 页
    Page(u32),
    /// 选中某个整合包
    Pick(String),
}

/// 拉取并展示一页结果，返回用户去向
async fn browse_results(
    service: &mcsup::service::ModpackService,
    keyword: Option<&str>,
    page: u32,
    printer: Printer,
) -> Result<Browse> {
    let offset = page.saturating_mul(PAGE_SIZE);
    let mut query = ModpackQuery::new().page(offset, PAGE_SIZE);
    if let Some(keyword) = keyword {
        query = query.keyword(keyword);
    }
    let spinner = TaskSpinner::new(
        match keyword {
            Some(keyword) => format!("搜索整合包「{keyword}」"),
            None => "浏览可开服的整合包".to_owned(),
        },
        printer.is_human(),
    );
    let search = service.select_source(None)?.search(&query).await;
    spinner.finish();
    let search = search.context("搜索整合包失败")?;

    let hits: Vec<ScopedChoice> = search
        .hits
        .iter()
        .map(|hit| {
            ScopedChoice::new(format!("{} · {}", hit.title, hit.id), false)
                .with_prefix(hit.id.clone())
                .with_display(hit.title.clone())
        })
        .collect();
    let choices = result_choices(hits, search.offset, search.total, page);

    // 0 条也要进入选择器：空列表 + Esc 返回上一级，而不是让整个命令退出。
    let label = results_label(keyword, search.total, page);
    // 搜索结果页的 Esc 回到推荐列表；推荐列表本身按 Esc 取消本次创建。
    let esc_help = if keyword.is_some() {
        crate::i18n::pick(
            printer.lang(),
            "返回推荐列表",
            "goes back to recommendations",
        )
    } else {
        crate::i18n::pick(printer.lang(), "取消", "cancels")
    };
    // 列表只是推荐 / 上一页结果：回车不拿它做局部校验，而是按输入远端搜索
    match select_filter_backable(
        printer.lang(),
        &label,
        &choices,
        no_scope(),
        &esc_help,
        EnterMode::Search,
    )? {
        Backable::Back => Ok(Browse::Back),
        Backable::Search(keyword) => Ok(Browse::Search(keyword)),
        Backable::Picked(picked) if picked == NEXT_PAGE => Ok(Browse::Page(page.saturating_add(1))),
        Backable::Picked(picked) if picked == PREV_PAGE => Ok(Browse::Page(page.saturating_sub(1))),
        Backable::Picked(picked) => extract_id(&picked)
            .map(Browse::Pick)
            .ok_or_else(|| anyhow::anyhow!("无效选择: {picked}")),
    }
}

/// 结果页的可选项：命中项在前，翻页项在后
///
/// 翻页项按「上一页在上、下一页在下」排列：返回上一页更贴近列表，继续向下
/// 翻页放在最末。是否出现只由 `offset + len` 与 `total` 决定，与网络无关，
/// 因此抽成纯函数单独测试。
fn result_choices(
    mut choices: Vec<ScopedChoice>,
    offset: u32,
    total: u64,
    page: u32,
) -> Vec<ScopedChoice> {
    let more = u64::from(offset) + choices.len() as u64;
    if page > 0 {
        choices.push(ScopedChoice::new(PREV_PAGE, false));
    }
    if more < total {
        choices.push(ScopedChoice::new(NEXT_PAGE, false));
    }
    choices
}

/// 结果页标题：0 条时说明没有匹配，其余显示页码与总数
///
/// 标题同时是输入行的前缀：有关键词时带上它，让用户一眼看到当前列表
/// 是按什么搜出来的；写「搜索」而不是「选择」，提示这一行是搜索框。
fn results_label(keyword: Option<&str>, total: u64, page: u32) -> String {
    if total == 0 {
        return match keyword {
            Some(keyword) => format!("搜索整合包「{keyword}」 · 没有匹配结果"),
            None => "搜索整合包 · 没有可在服务端运行的整合包".to_owned(),
        };
    }
    match keyword {
        Some(keyword) => format!(
            "搜索整合包「{keyword}」 · 第 {} 页 · 共 {total} 个结果",
            page + 1
        ),
        None => format!("搜索整合包 · 第 {} 页 · 共 {total} 个支持服务端", page + 1),
    }
}

/// 无范围切换时的占位提示
const fn no_scope() -> ScopeHint {
    ScopeHint {
        show_all: "",
        show_default: "",
    }
}

/// 取回清单拿到 MC 版本，组装 [`PickedModpack`]
///
/// 清单里的 MC 版本只有下载并解析后才拿得到，而注册表需要在创建时就知道它，
/// 因此这里先做一次「取回 + 读清单」；真正的布置发生在 `server create` 落库
/// 之后，用同一份版本重新取回（本地缓存命中时不重复下载）。
async fn finish_pick(
    service: &mcsup::service::ModpackService,
    project: &str,
    title: &str,
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
    let variant = prepared
        .require_loader()
        .map_err(|err| anyhow::anyhow!(err.to_string()))?;
    // 暂存目录用后就删：真正的产物在 server create 之后由 install_modpack 重新取回
    if let Some(staging) = prepared.archive_path.parent() {
        let _ = std::fs::remove_dir_all(staging);
    }
    Ok(PickedModpack {
        project_id: prepared.resolved.project_id,
        title: title.to_owned(),
        version_id: prepared.resolved.version_id,
        version_number: prepared.resolved.version_number,
        minecraft,
        variant,
    })
}

/// 从 `标题 · ID` 里取回 ID
///
/// ID 由本模块拼在最后，因此从**最后一个**分隔符切；标题里出现同样
/// 分隔符也不会切错。拿不到就返回 `None`，由调用方报无效选择。
fn extract_id(label: &str) -> Option<String> {
    let (_, id) = label.rsplit_once(" · ")?;
    (!id.is_empty()).then(|| id.to_owned())
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

/// 详情字段：标签 + 值，顺序即展示顺序
///
/// 可选字段（slug / 作者 / 许可证）缺省时不占行。
fn detail_rows(detail: &ModpackDetail) -> Vec<(&'static str, String)> {
    let summary = &detail.summary;
    let mut rows = vec![("标题", summary.title.clone()), ("ID", summary.id.clone())];
    if let Some(slug) = summary.slug.as_deref() {
        rows.push(("slug", slug.to_owned()));
    }
    if let Some(author) = summary.author.as_deref() {
        rows.push(("作者", author.to_owned()));
    }
    rows.push(("加载器", summary.loaders.join(", ")));
    rows.push(("游戏版本", join_versions(&summary.game_versions)));
    rows.push(("下载量", summary.downloads.to_string()));
    if let Some(license) = summary.license.as_deref() {
        rows.push(("许可证", license.to_owned()));
    }
    rows.push(("来源", detail.source_name.to_owned()));
    rows.push(("页面", detail.page_url.clone()));
    rows.push(("简介", summary.description.clone()));
    rows
}

/// 标签列与值列之间的固定间隔
const LABEL_GAP: usize = 2;

/// 标签列按最宽标签右补空格，值列起点对齐；不画表格边框，也不加冒号
///
/// 中文标签是双宽字符，补空格按显示宽度而不是字符数算，否则「ID」和
/// 「游戏版本」后面的值会错开。
fn render_detail(rows: &[(&str, String)]) -> String {
    let width = rows
        .iter()
        .map(|(label, _)| measure_text_width(label))
        .max()
        .unwrap_or(0);
    rows.iter()
        .map(|(label, value)| {
            let pad = width
                .saturating_sub(measure_text_width(label))
                .saturating_add(LABEL_GAP);
            format!("{label}{}{value}", " ".repeat(pad))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 展示整合包详情
///
/// 人类可读模式按「标签列 + 值列」对齐输出，不画表格边框：版本选择紧接在
/// 同一屏出现，边框只会挤占宽度。
fn print_detail(detail: &ModpackDetail, printer: Printer) -> Result<()> {
    if printer.is_human() {
        printer.print_section(&render_detail(&detail_rows(detail)));
        return Ok(());
    }
    let summary = &detail.summary;
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

/// 候选版本：正式版优先，正式版不存在时用全部；带服务端归档的排前面
///
/// 顺序会直接影响默认选中项：最新正式版可能只有客户端包，排到有服务端
/// 归档的版本之后，避免非交互建站默认装错。
fn version_pool(versions: &[ModpackVersion]) -> Vec<&ModpackVersion> {
    let releases: Vec<&ModpackVersion> = versions.iter().filter(|v| v.is_release()).collect();
    let mut pool: Vec<&ModpackVersion> = if releases.is_empty() {
        versions.iter().collect()
    } else {
        releases
    };
    pool.sort_by_key(|version| !version.has_server_file);
    pool
}

/// 选版本：默认只列正式版，选不到再放开测试版
fn pick_version(versions: &[ModpackVersion], printer: Printer) -> Result<ModpackVersion> {
    if versions.is_empty() {
        anyhow::bail!("该整合包没有可用版本");
    }
    let pool = version_pool(versions);
    let labels: Vec<String> = pool
        .iter()
        .take(VERSION_SHOW_LIMIT)
        .map(|version| version_label(version))
        .collect();
    if labels.len() == 1 {
        return Ok((*pool[0]).clone());
    }
    let picked = crate::prompts::select_filter(printer.lang(), "选择整合包版本", &labels)?;
    pool.iter()
        .find(|version| version_label(version) == picked)
        .map(|version| (*version).clone())
        .ok_or_else(|| anyhow::anyhow!("无效选择: {picked}"))
}

/// 版本列表里的一行展示
fn version_label(version: &ModpackVersion) -> String {
    let mut label = format!(
        "{} · {} · MC {} · {}",
        version.version_number,
        version.channel,
        join_versions(&version.game_versions),
        version.loaders.join(",")
    );
    if version.has_server_file {
        label.push_str(" · 服务端");
    }
    label
}

/// 交互选版本，Esc 返回 `None` 表示回到详情页
///
/// 与 [`pick_version`] 同一套候选口径（优先正式版、最多展示
/// [`VERSION_SHOW_LIMIT`] 项），区别只在 Esc 的语义。
fn pick_version_backable(
    lang: crate::locale::Lang,
    versions: &[ModpackVersion],
) -> Result<Option<ModpackVersion>> {
    if versions.is_empty() {
        anyhow::bail!("该整合包没有可用版本");
    }
    let pool = version_pool(versions);
    let shown: Vec<&ModpackVersion> = pool.iter().take(VERSION_SHOW_LIMIT).copied().collect();
    let choices: Vec<ScopedChoice> = shown
        .iter()
        .map(|version| ScopedChoice::new(version_label(version), false))
        .collect();
    let esc_help = crate::i18n::pick(lang, "返回上一步", "goes back");
    match select_filter_backable(
        lang,
        "选择整合包版本",
        &choices,
        no_scope(),
        &esc_help,
        EnterMode::Choice,
    )? {
        Backable::Back => Ok(None),
        // 版本候选就是全部可选值，不存在「按关键词去远端搜」这一步
        Backable::Search(_) => Err(anyhow::anyhow!("版本选择不接受搜索关键词")),
        Backable::Picked(picked) => shown
            .into_iter()
            .find(|version| version_label(version) == picked)
            .cloned()
            .map(Some)
            .ok_or_else(|| anyhow::anyhow!("无效选择: {picked}")),
    }
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
        skipped_client_only: applied.skipped_client_only,
        path: server_dir.display().to_string(),
    };
    let note = skipped_note(printer.lang(), applied.skipped_client_only);
    let human = crate::i18n::pick(
        printer.lang(),
        format!(
            "已按整合包 {} {} 布置服务端：{} 个文件，下载 {}{note}",
            picked.title,
            picked.version_number,
            applied.file_count,
            crate::output::human_bytes(applied.downloaded_bytes)
        ),
        format!(
            "applied modpack {} {}: {} files, downloaded {}{note}",
            picked.title,
            picked.version_number,
            applied.file_count,
            crate::output::human_bytes(applied.downloaded_bytes)
        ),
    );
    printer.print_outcome(&human, &payload)
}

/// 远端布置结果的回显：与本地同一 JSON 形状，文案标注目标机器
///
/// 帧只回统计量（项目 / 版本 / 文件数 / 字节数），展示口径因此完全一致；
/// 差异只有「在 <远端>」，用户能一眼看出东西装在哪儿。
pub(crate) fn report_remote_applied(
    result: &mcsup::backend::remote::RemoteModpackResult,
    path: String,
    origin: &str,
    printer: Printer,
) -> Result<()> {
    let lang = printer.lang();
    let payload = ModpackInstallJson {
        source: "modrinth",
        project_id: &result.project_id,
        version_id: &result.version_id,
        version_number: &result.version_number,
        minecraft: &result.minecraft,
        loader: result.loader.clone(),
        files: result.files,
        downloaded_bytes: result.downloaded_bytes,
        reused: result.reused,
        skipped_client_only: result.skipped_client_only,
        path,
    };
    let note = skipped_note(lang, result.skipped_client_only);
    let human = crate::i18n::pick(
        lang,
        format!(
            "已在 {origin} 按整合包 {} {} 布置服务端：{} 个文件，下载 {}{note}",
            result.project_id,
            result.version_number,
            result.files,
            crate::output::human_bytes(result.downloaded_bytes)
        ),
        format!(
            "applied modpack {} {} on {origin}: {} files, downloaded {}{note}",
            result.project_id,
            result.version_number,
            result.files,
            crate::output::human_bytes(result.downloaded_bytes)
        ),
    );
    printer.print_outcome(&human, &payload)
}

/// 「跳过仅客户端模组」的提示后缀；没跳过时为空串
///
/// 上游元数据判定与清单不一致时，文件数会比清单声明的少。不解释一句，
/// 用户会以为安装漏了文件。
fn skipped_note(lang: crate::locale::Lang, skipped: usize) -> String {
    if skipped == 0 {
        return String::new();
    }
    crate::i18n::pick(
        lang,
        format!("，跳过 {skipped} 个仅客户端模组"),
        format!(", skipped {skipped} client-only mods"),
    )
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
    /// 按上游元数据判定为仅客户端、已跳过的模组数
    skipped_client_only: usize,
    path: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_id_reads_trailing_id() {
        assert_eq!(
            extract_id("Fabulously Optimized · 1KVo5zza").as_deref(),
            Some("1KVo5zza")
        );
        assert_eq!(extract_id("no id here"), None);
        assert_eq!(extract_id("bad · "), None);
    }

    fn hit(id: &str, title: &str) -> ScopedChoice {
        ScopedChoice::new(format!("{title} · {id}"), false)
            .with_prefix(id)
            .with_display(title)
    }

    #[test]
    fn result_choices_adds_next_only_when_more_remain() {
        let hits = vec![hit("1", "A"), hit("2", "B")];
        let first = result_choices(hits.clone(), 0, 3, 0);
        assert_eq!(first.len(), 3);
        assert_eq!(first[2].label, NEXT_PAGE);
        assert!(!first.iter().any(|c| c.label == PREV_PAGE));

        let last = result_choices(hits, 2, 3, 1);
        assert!(!last.iter().any(|c| c.label == NEXT_PAGE));
        assert_eq!(last.last().map(|c| c.label.as_str()), Some(PREV_PAGE));
    }

    /// 中间页：上一页在上、下一页在下，选中一行不会把翻页项的顺序看反
    #[test]
    fn result_choices_orders_prev_above_next() {
        let hits = vec![hit("1", "A"), hit("2", "B")];
        let middle = result_choices(hits, 20, 138, 1);
        let labels: Vec<&str> = middle.iter().map(|choice| choice.label.as_str()).collect();
        assert_eq!(labels[labels.len() - 2], PREV_PAGE, "上一页在上");
        assert_eq!(labels[labels.len() - 1], NEXT_PAGE, "下一页在下");
    }

    #[test]
    fn result_choices_has_no_pager_when_everything_fits() {
        let choices = result_choices(vec![hit("1", "A")], 0, 1, 0);
        assert_eq!(choices.len(), 1);
    }

    /// 0 条时不生成翻页项，空列表原样交给可返回选择器
    #[test]
    fn result_choices_stays_empty_without_hits() {
        assert!(result_choices(Vec::new(), 0, 0, 0).is_empty());
    }

    /// 向导起点是默认列表：不发关键词、从第一页开始，用户不必先按回车
    #[test]
    fn wizard_starts_on_default_list() {
        assert_eq!(
            initial_state(),
            Wizard::Results {
                keyword: None,
                page: 0,
            },
            "首屏应是默认列表（无关键词过滤、第一页）"
        );
    }

    /// 空结果标题说明原因；有关键词时带上关键词
    #[test]
    fn results_label_explains_empty_search() {
        assert_eq!(
            results_label(Some("音乐"), 0, 0),
            "搜索整合包「音乐」 · 没有匹配结果"
        );
        assert_eq!(
            results_label(None, 0, 0),
            "搜索整合包 · 没有可在服务端运行的整合包"
        );
        assert_eq!(
            results_label(Some("fab"), 42, 2),
            "搜索整合包「fab」 · 第 3 页 · 共 42 个结果"
        );
        assert_eq!(
            results_label(None, 42, 2),
            "搜索整合包 · 第 3 页 · 共 42 个支持服务端"
        );
    }

    #[test]
    fn hit_choice_renders_id_first_and_submits_full_label() {
        let choice = hit("l9m9tuPN", "Zombie Invade 100 Days");
        assert_eq!(choice.prefix.as_deref(), Some("l9m9tuPN"));
        assert_eq!(choice.display.as_deref(), Some("Zombie Invade 100 Days"));
        assert_eq!(extract_id(&choice.label).as_deref(), Some("l9m9tuPN"));
    }

    fn modpack_version(id: &str, release: bool, server: bool) -> ModpackVersion {
        ModpackVersion {
            id: id.to_owned(),
            version_number: id.to_owned(),
            name: id.to_owned(),
            channel: if release { "release" } else { "beta" }.to_owned(),
            loaders: vec!["forge".to_owned()],
            game_versions: vec!["1.20.1".to_owned()],
            published: String::new(),
            downloads: 0,
            changelog: None,
            has_server_file: server,
        }
    }

    #[test]
    fn version_pool_prefers_server_capable_release() {
        let versions = vec![
            modpack_version("2.3-Fix", true, false),
            modpack_version("2.3", true, true),
            modpack_version("2.2", true, true),
        ];
        let pool = version_pool(&versions);
        assert_eq!(pool[0].version_number, "2.3");
        assert!(version_label(pool[0]).contains("服务端"));
        assert!(!version_label(pool[2]).contains("服务端"));
    }

    #[test]
    fn join_versions_truncates_long_lists() {
        let short = vec!["1.21.1".to_owned()];
        assert_eq!(join_versions(&short), "1.21.1");
        let long: Vec<String> = (0..10).map(|i| format!("1.{i}")).collect();
        let joined = join_versions(&long);
        assert!(joined.contains("等 10 个"), "{joined}");
    }

    fn modpack_detail(all_optional: bool) -> ModpackDetail {
        use mcsup::port::modpack::ModpackSummary;
        ModpackDetail {
            summary: ModpackSummary {
                id: "l9m9tuPN".to_owned(),
                slug: all_optional.then(|| "zombie-invade-100-days".to_owned()),
                title: "Zombie Invade 100 Days".to_owned(),
                description: "僵尸入侵 100 天".to_owned(),
                author: all_optional.then(|| "Forge Labs".to_owned()),
                loaders: vec!["forge".to_owned()],
                game_versions: vec!["1.19.2".to_owned(), "1.20.1".to_owned()],
                downloads: 15_413_875,
                follows: 0,
                icon_url: None,
                license: all_optional.then(|| "LicenseRef-All-Rights-Reserved".to_owned()),
                updated: None,
            },
            body: String::new(),
            gallery: Vec::new(),
            page_url: "https://modrinth.com/modpack/zombie-invade-100-days".to_owned(),
            source_name: "Modrinth",
        }
    }

    /// 标签按显示宽度补空格，值列起点一致；不画边框，也不加冒号
    #[test]
    fn render_detail_aligns_values_by_display_width() {
        let rows = detail_rows(&modpack_detail(true));
        let rendered = render_detail(&rows);
        let width = rows
            .iter()
            .map(|(label, _)| measure_text_width(label))
            .max()
            .expect("详情至少有一行");
        assert_eq!(width, measure_text_width("游戏版本"), "最宽标签决定列宽");
        for ((label, value), line) in rows.iter().zip(rendered.lines()) {
            let prefix = format!(
                "{label}{}",
                " ".repeat(width + LABEL_GAP - measure_text_width(label))
            );
            let rest = line
                .strip_prefix(&prefix)
                .unwrap_or_else(|| panic!("值列起点不一致: {line}"));
            assert_eq!(rest, value.as_str(), "标签后直接是值，不加冒号");
            assert!(!line.contains('│') && !line.contains('|'), "不画表格边框");
        }
    }

    /// 可选字段缺省时不占行，剩余行仍对齐
    #[test]
    fn detail_rows_skip_missing_optional_fields() {
        let rows = detail_rows(&modpack_detail(false));
        let labels: Vec<&str> = rows.iter().map(|(label, _)| *label).collect();
        assert!(!labels.contains(&"slug"), "{labels:?}");
        assert!(!labels.contains(&"作者"), "{labels:?}");
        assert!(!labels.contains(&"许可证"), "{labels:?}");
        assert_eq!(labels.last().copied(), Some("简介"), "简介是最后一行");
        assert_eq!(labels.first().copied(), Some("标题"));
        assert_eq!(labels.len(), 8);

        let rendered = render_detail(&rows);
        assert_eq!(rendered.lines().count(), rows.len());
        for ((label, value), line) in rows.iter().zip(rendered.lines()) {
            let prefix = format!(
                "{label}{}",
                " ".repeat(8 + LABEL_GAP - measure_text_width(label))
            );
            assert_eq!(
                line.strip_prefix(&prefix),
                Some(value.as_str()),
                "列宽仍然是 8: {line}"
            );
        }
    }

    /// 没跳过时不留尾巴；有跳过时中英文都要说明原因
    #[test]
    fn skipped_note_explains_missing_mods() {
        use crate::locale::Lang;
        assert_eq!(skipped_note(Lang::Zh, 0), "", "没跳过时不应追加说明");
        assert_eq!(skipped_note(Lang::En, 0), "");
        assert_eq!(
            skipped_note(Lang::Zh, 3),
            "，跳过 3 个仅客户端模组",
            "中文提示要带上数量"
        );
        assert_eq!(
            skipped_note(Lang::En, 3),
            ", skipped 3 client-only mods",
            "英文提示要带上数量"
        );
    }
}
