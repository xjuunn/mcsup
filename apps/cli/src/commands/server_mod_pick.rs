//! `server mod add` 的交互选择：浏览 / 搜索 → 翻页 → 详情 → 选版本
//!
//! 与 `server create --variant modrinth-modpack` 的整合包向导同一套手感：
//! 输入框与列表同屏，回车按关键词远端搜索，翻页项排在列表末尾，Esc 逐级
//! 返回。差别只在筛选条件与落点——这里列的是这台服务端装了能跑的 mod /
//! 插件，选完交给 `server mod add` 走既有的「计划 → 确认 → 落盘」流程。
//!
//! # 为什么不在这一层做确认
//!
//! 整合包向导选完版本就下载，所以它自己必须确认一次；这里返回项目与版本后
//! 还有计划和确认两步，`--dry-run` / `-y` 也都挂在那两步上。再确认一次等于
//! 让用户连按两次回车，且会让 `-y` 与 `--dry-run` 的语义分叉。
//!
//! # 回显
//!
//! 搜索与取候选版本都是网络操作：各有 spinner。`--output json` 下交互不会
//! 发生（`Printer::is_interactive` 恒假），因此不需要额外隐藏。

use crate::diagnostic::AppDiagnostic;
use crate::i18n::{pick, text};
use crate::locale::Lang;
use crate::output::Printer;
use crate::progress::TaskSpinner;
use crate::prompts::{
    Backable, EnterMode, NEXT_PAGE, PREV_PAGE, ScopeHint, ScopedChoice, Single,
    select_filter_backable, with_pagers,
};
use anyhow::Result;
use mcsup::domain::server::content::{ContentCandidate, ContentKind};
use mcsup::port::content::{ContentProject, ContentQuery};
use mcsup::service::{InstallSpec, ModService};

/// 每页结果数
const PAGE_SIZE: u32 = 20;

/// 版本列表展示上限（防止终端刷屏）
const VERSION_SHOW_LIMIT: usize = 60;

/// 已选定的项目与版本
///
/// `version_id` 为 `None` 表示用户没有指定版本，交给服务按适配度自动挑——
/// 与 `--version` 缺省时同一条路径。
pub(crate) struct PickedMod {
    /// 项目 ID
    pub project_id: String,
    /// 版本 ID；`None` 表示自动挑
    pub version_id: Option<String>,
}

/// 一轮选择里不变的条件
///
/// 三项分别由 `--kind` / `--source` / `--prerelease` 决定，整轮向导里不变；
/// 凑成结构体后每层少两个参数，也避免把「类别」和「来源」在调用链上错位。
#[derive(Debug, Clone, Copy)]
pub(crate) struct PickOptions<'a> {
    /// 内容类别
    pub kind: ContentKind,
    /// 来源名；`None` 用默认来源
    pub source: Option<&'a str>,
    /// 允许测试版 / 内测版
    pub allow_prerelease: bool,
}

/// 向导状态
///
/// 显式状态机而非嵌套循环：每一步只回答「按 Esc 回到哪」，返回路径因此是
/// 可见、可测试的。项目用 `Box` 装：它带一串 `Vec`，按值放进枚举会让每个
/// 变体都背上同样的大小。
enum Wizard {
    /// 浏览一页结果；`keyword` 为空即上游默认列表
    Results { keyword: Option<String>, page: u32 },
    /// 已选定项目：详情与版本列表在同一屏（Esc 回到结果页）
    Project {
        project: Box<ContentProject>,
        keyword: Option<String>,
        page: u32,
    },
}

/// 选择要添加的 mod：浏览 / 搜索 → 结果 → 详情并选版本
///
/// `keyword` 来自 `@search <关键词>`：给出时从该关键词的第一页开始，
/// 否则从上游默认列表开始。
///
/// `version_hint` 来自 `--version`：给出时只按它挑版本，不进入交互选择
/// （脚本场景需要可复现）。`options.allow_prerelease` 来自 `--prerelease`：
/// 放开版本列表，与「自动挑版本」的口径一致，不会出现「交互里选不到、自动
/// 路径却装了测试版」的分叉。
///
/// # Errors
///
/// 上游失败、该项目没有可安装版本，或用户在结果列表按 Esc 取消时返回错误
pub(crate) async fn pick_mod(
    service: &ModService,
    name: &str,
    options: PickOptions<'_>,
    keyword: Option<&str>,
    version_hint: Option<&str>,
    printer: Printer,
) -> Result<PickedMod> {
    let mut state = Wizard::Results {
        keyword: keyword.map(<str>::to_owned),
        page: 0,
    };
    loop {
        state = match state {
            Wizard::Results { keyword, page } => {
                match browse_results(service, name, options, keyword.as_deref(), page, printer)
                    .await?
                {
                    // 搜索结果页按 Esc：回到推荐列表，而不是退出整个命令
                    Browse::Back if keyword.is_some() => Wizard::Results {
                        keyword: None,
                        page: 0,
                    },
                    // 推荐列表本身再按 Esc：没有上一层，取消本次添加
                    Browse::Back => return Err(AppDiagnostic::cancelled().into()),
                    Browse::Search(next) => Wizard::Results {
                        keyword: Some(next),
                        page: 0,
                    },
                    Browse::Page(next) => Wizard::Results {
                        keyword,
                        page: next,
                    },
                    Browse::Pick(project) => Wizard::Project {
                        project,
                        keyword,
                        page,
                    },
                }
            }
            Wizard::Project {
                project,
                keyword,
                page,
            } => {
                let candidates =
                    fetch_candidates(service, name, &project, options, printer).await?;
                print_detail(&project, printer);
                let picked = if let Some(hint) = version_hint {
                    find_hint(&candidates, hint)
                        .ok_or_else(|| version_missing(printer.lang(), &project.title, hint))?
                } else {
                    let Some(candidate) = pick_version(printer, &project.title, &candidates)?
                    else {
                        // Esc：回到结果页，当前关键词与页码原样保留
                        state = Wizard::Results { keyword, page };
                        continue;
                    };
                    candidate
                };
                return Ok(PickedMod {
                    project_id: project.id,
                    version_id: Some(picked.version_id),
                });
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
    /// 选中某个项目（`Box` 的理由同 [`Wizard::Project`]）
    Pick(Box<ContentProject>),
}

/// 拉取并展示一页结果，返回用户去向
async fn browse_results(
    service: &ModService,
    name: &str,
    options: PickOptions<'_>,
    keyword: Option<&str>,
    page: u32,
    printer: Printer,
) -> Result<Browse> {
    let lang = printer.lang();
    let noun = kind_noun(lang, options.kind);
    let offset = page.saturating_mul(PAGE_SIZE);
    let mut query = ContentQuery::new().page(offset, PAGE_SIZE);
    if let Some(keyword) = keyword {
        query = query.keyword(keyword);
    }
    let label = match keyword {
        Some(keyword) => pick(
            lang,
            format!("搜索{noun}「{keyword}」"),
            format!("searching {noun}s for {keyword}"),
        ),
        None => pick(
            lang,
            format!("浏览可用的{noun}"),
            format!("browsing {noun}s"),
        ),
    };
    let spinner = TaskSpinner::new(label, printer.is_human());
    let search = service
        .search(name, options.kind, &query, options.source)
        .await;
    spinner.finish();
    let search = search?;

    // 命中项与「title · id」的对应关系要先记住：选择器只回报标签文本，
    // 回来时按它找回完整项目（详情与类别都在里面，不必再查一次上游）。
    let hits = search.hits;
    let labels: Vec<String> = hits
        .iter()
        .map(|hit| format!("{} · {}", hit.title, hit.id))
        .collect();
    let choices = with_pagers(
        hits.iter()
            .zip(&labels)
            .map(|(hit, label)| {
                ScopedChoice::new(label.clone(), false)
                    .with_prefix(hit.id.clone())
                    .with_display(hit.title.clone())
            })
            .collect(),
        search.offset,
        search.total,
        page,
    );

    // 0 条也要进入选择器：空列表 + Esc 返回上一级，而不是让整个命令退出
    let title = results_label(lang, options.kind, keyword, search.total, page);
    let esc_help = if keyword.is_some() {
        pick(lang, "返回推荐列表", "goes back to recommendations")
    } else {
        pick(lang, "取消", "cancels")
    };
    // 列表只是推荐 / 上一页结果：回车不拿它做局部校验，而是按输入远端搜索
    match select_filter_backable(
        lang,
        &title,
        &choices,
        ScopeHint::none(),
        &esc_help,
        EnterMode::Search,
        // 结果页只是入口列表：唯一命中时直接进详情，不额外消耗一次回车
        Single::Skip,
    )? {
        Backable::Back => Ok(Browse::Back),
        Backable::Search(keyword) => Ok(Browse::Search(keyword)),
        Backable::Picked(picked) if picked == NEXT_PAGE => Ok(Browse::Page(page.saturating_add(1))),
        Backable::Picked(picked) if picked == PREV_PAGE => Ok(Browse::Page(page.saturating_sub(1))),
        Backable::Picked(picked) => labels
            .iter()
            .position(|label| *label == picked)
            .and_then(|index| hits.into_iter().nth(index))
            .map(|project| Browse::Pick(Box::new(project)))
            .ok_or_else(|| anyhow::anyhow!("invalid choice: {picked}")),
    }
}

/// 结果页标题：0 条时说明没有匹配，其余显示页码与总数
///
/// 列表已经滤掉仅客户端的项目（见 `ContentQuery::server_usable_only`），
/// 所以 0 条时要把这层口径讲清楚：用户搜「iris」搜不到，很可能只是因为它
/// 是客户端专属，而不是搜错了词。
fn results_label(
    lang: Lang,
    kind: ContentKind,
    keyword: Option<&str>,
    total: u64,
    page: u32,
) -> String {
    let noun = kind_noun(lang, kind);
    if total == 0 {
        return match keyword {
            Some(keyword) => pick(
                lang,
                format!("搜索{noun}「{keyword}」 · 没有可装到服务端的{noun}（仅客户端的已排除）"),
                format!(
                    "searching {noun}s for {keyword} · no server-installable {noun}s (client-only ones are excluded)"
                ),
            ),
            None => pick(
                lang,
                format!("搜索{noun} · 没有可添加到服务端的{noun}（仅客户端的已排除）"),
                format!(
                    "searching {noun}s · nothing installable on a server (client-only ones are excluded)"
                ),
            ),
        };
    }
    match keyword {
        Some(keyword) => pick(
            lang,
            format!(
                "搜索{noun}「{keyword}」 · 第 {} 页 · 共 {total} 个结果",
                page + 1
            ),
            format!(
                "searching {noun}s for {keyword} · page {} · {total} results",
                page + 1
            ),
        ),
        None => pick(
            lang,
            format!("搜索{noun} · 第 {} 页 · 共 {total} 个可添加", page + 1),
            format!(
                "searching {noun}s · page {} · {total} installable",
                page + 1
            ),
        ),
    }
}

/// 界面文案里的类别名词：中文只有「插件」需要区分，其余都叫 mod
fn kind_noun(lang: Lang, kind: ContentKind) -> &'static str {
    match kind {
        ContentKind::Plugin => text(lang, "插件", "plugin"),
        _ => text(lang, "mod", "mod"),
    }
}

/// 取这台服务端上该项目的可安装版本（已按适配度排序）
async fn fetch_candidates(
    service: &ModService,
    name: &str,
    project: &ContentProject,
    options: PickOptions<'_>,
    printer: Printer,
) -> Result<Vec<ContentCandidate>> {
    let lang = printer.lang();
    let spinner = TaskSpinner::new(
        pick(
            lang,
            format!("查看 {} 的可安装版本…", project.title),
            format!("checking installable versions of {}…", project.title),
        ),
        printer.is_human(),
    );
    let candidates = service
        .candidates(
            name,
            &project.id,
            options.kind,
            InstallSpec {
                version: None,
                source: options.source,
                allow_prerelease: options.allow_prerelease,
            },
        )
        .await;
    spinner.finish();
    let candidates = candidates?;
    if candidates.is_empty() {
        return Err(no_available_version(lang, &project.title, options.allow_prerelease).into());
    }
    Ok(candidates)
}

/// 「该项目没有可安装版本」：资源不存在（3）
fn no_available_version(lang: Lang, title: &str, prerelease: bool) -> AppDiagnostic {
    let help = if prerelease {
        pick(
            lang,
            "换一个项目，或稍后再试".to_owned(),
            "pick another project, or retry later".to_owned(),
        )
    } else {
        pick(
            lang,
            "换一个项目，或加 --prerelease 允许测试版".to_owned(),
            "pick another project, or add --prerelease to allow beta versions".to_owned(),
        )
    };
    AppDiagnostic::not_found(
        pick(
            lang,
            format!("{title} 没有可用于这台服务端的版本"),
            format!("{title} has no version that runs on this server"),
        ),
        Some(help),
    )
}

/// 「该项目没有这个版本」：资源不存在（3）
fn version_missing(lang: Lang, title: &str, hint: &str) -> AppDiagnostic {
    AppDiagnostic::not_found(
        pick(
            lang,
            format!("{title} 没有版本 {hint}"),
            format!("{title} has no version {hint}"),
        ),
        Some(pick(
            lang,
            "去掉 --version 查看全部可用版本".to_owned(),
            "drop --version to list the available versions".to_owned(),
        )),
    )
}

/// 按 `--version` 给的值在候选里找：版本 ID 与版本号都认
fn find_hint(candidates: &[ContentCandidate], hint: &str) -> Option<ContentCandidate> {
    candidates
        .iter()
        .find(|candidate| candidate.version_id == hint || candidate.version_number == hint)
        .cloned()
}

/// 交互选版本；Esc 返回 `None` 表示回到结果页
///
/// 单候选也照常提问：选中之后就要下载，用户必须先看到自己要装哪个版本。
fn pick_version(
    printer: Printer,
    title: &str,
    candidates: &[ContentCandidate],
) -> Result<Option<ContentCandidate>> {
    let lang = printer.lang();
    let shown: Vec<&ContentCandidate> = candidates.iter().take(VERSION_SHOW_LIMIT).collect();
    let choices: Vec<ScopedChoice> = shown
        .iter()
        .map(|candidate| ScopedChoice::new(version_label(candidate), false))
        .collect();
    let label = pick(
        lang,
        format!("选择 {title} 的版本"),
        format!("pick a version of {title}"),
    );
    let esc_help = pick(lang, "返回结果列表", "goes back to the results");
    match select_filter_backable(
        lang,
        &label,
        &choices,
        ScopeHint::none(),
        &esc_help,
        EnterMode::Choice,
        Single::Ask,
    )? {
        Backable::Back => Ok(None),
        // 版本候选就是全部可选值，不存在「按关键词去远端搜」这一步
        Backable::Search(_) => Err(anyhow::anyhow!("version pick does not take a keyword")),
        Backable::Picked(picked) => shown
            .into_iter()
            .find(|candidate| version_label(candidate) == picked)
            .cloned()
            .map(Some)
            .ok_or_else(|| anyhow::anyhow!("invalid choice: {picked}")),
    }
}

/// 版本列表里的一行展示
fn version_label(candidate: &ContentCandidate) -> String {
    format!(
        "{} · {} · MC {} · {}",
        candidate.version_number,
        candidate.channel,
        join_versions(&candidate.game_versions),
        candidate.loaders.join(",")
    )
}

/// 版本号列表压缩展示：超过 6 个用「等 N 个」收尾
fn join_versions(versions: &[String]) -> String {
    const SHOW: usize = 6;
    if versions.len() <= SHOW {
        return versions.join(", ");
    }
    format!("{} 等 {} 个", versions[..SHOW].join(", "), versions.len())
}

/// 详情字段：标签 + 值，顺序即展示顺序
///
/// 可选字段（slug / 作者）缺省时不占行。
fn detail_rows(project: &ContentProject) -> Vec<(&'static str, String)> {
    let mut rows = vec![("标题", project.title.clone()), ("ID", project.id.clone())];
    if let Some(slug) = project.slug.as_deref() {
        rows.push(("slug", slug.to_owned()));
    }
    if let Some(author) = project.author.as_deref() {
        rows.push(("作者", author.to_owned()));
    }
    let mut kinds: Vec<&str> = project.kinds().iter().map(|kind| kind.as_str()).collect();
    kinds.dedup();
    rows.push(("类别", kinds.join(", ")));
    rows.push(("加载器", project.loaders.join(", ")));
    rows.push(("游戏版本", join_versions(&project.game_versions)));
    rows.push(("下载量", project.downloads.to_string()));
    rows.push(("简介", project.description.clone()));
    rows
}

/// 展示项目详情（人类模式；JSON 模式下向导不会启动）
fn print_detail(project: &ContentProject, printer: Printer) {
    printer.print_section(&crate::output::render_fields(&detail_rows(project)));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 标题说明当前关键词、页码与总数；0 条时说明没有匹配
    #[test]
    fn results_label_reports_page_and_total() {
        let zh = results_label(Lang::Zh, ContentKind::Mod, Some("sodium"), 42, 2);
        assert!(zh.contains("sodium"), "{zh}");
        assert!(zh.contains("第 3 页"), "{zh}");
        assert!(zh.contains("42"), "{zh}");

        let empty = results_label(Lang::Zh, ContentKind::Mod, Some("zzz"), 0, 0);
        assert!(empty.contains("没有可装到服务端"), "{empty}");
        assert!(
            empty.contains("仅客户端的已排除"),
            "要说明过滤口径：{empty}"
        );

        let plugin = results_label(Lang::Zh, ContentKind::Plugin, None, 7, 0);
        assert!(plugin.contains("插件"), "插件要用自己的名词：{plugin}");
    }

    /// 候选按上游顺序展示，版本行带版本号、渠道、MC 与加载器
    #[test]
    fn version_label_shows_the_facts_users_need() {
        let candidate = ContentCandidate {
            version_id: "abc".to_owned(),
            version_number: "1.2.3".to_owned(),
            channel: "release".to_owned(),
            featured: false,
            loaders: vec!["fabric".to_owned()],
            game_versions: vec!["1.21.1".to_owned()],
            date_published: "2026-01-01T00:00:00Z".to_owned(),
            score: 100,
        };
        let label = version_label(&candidate);
        assert!(label.contains("1.2.3"), "{label}");
        assert!(label.contains("fabric"), "{label}");
        assert!(label.contains("1.21.1"), "{label}");
    }

    /// `--version` 既认版本 ID 也认版本号
    #[test]
    fn version_hint_accepts_id_and_number() {
        let candidates = vec![ContentCandidate {
            version_id: "abc".to_owned(),
            version_number: "1.2.3".to_owned(),
            channel: "release".to_owned(),
            featured: false,
            loaders: Vec::new(),
            game_versions: Vec::new(),
            date_published: String::new(),
            score: 0,
        }];
        assert_eq!(
            find_hint(&candidates, "abc").map(|c| c.version_number),
            Some("1.2.3".to_owned())
        );
        assert_eq!(
            find_hint(&candidates, "1.2.3").map(|c| c.version_id),
            Some("abc".to_owned())
        );
        assert!(find_hint(&candidates, "nope").is_none());
    }

    /// 可选字段缺省时不占行
    #[test]
    fn detail_rows_skip_missing_optional_fields() {
        let project = ContentProject {
            id: "AABBCC".to_owned(),
            slug: None,
            title: "Some Mod".to_owned(),
            description: "does things".to_owned(),
            kind: ContentKind::Mod,
            also_kinds: Vec::new(),
            loaders: vec!["fabric".to_owned()],
            game_versions: Vec::new(),
            author: None,
            downloads: 1,
            icon_url: None,
        };
        let labels: Vec<&str> = detail_rows(&project)
            .iter()
            .map(|(label, _)| *label)
            .collect();
        assert!(!labels.contains(&"slug"), "{labels:?}");
        assert!(!labels.contains(&"作者"), "{labels:?}");
        assert_eq!(labels.first().copied(), Some("标题"));
        assert_eq!(labels.last().copied(), Some("简介"));
    }
}
