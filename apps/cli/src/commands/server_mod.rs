//! `mcsup server mod …`：管理某台服务端的 `mods/` 目录
//!
//! 入口只做三件事：收集参数、调 [`mcsup::service::ModService`]、渲染结果。
//! 判定（能不能装、装到哪、运行中能不能改、加载器口径）全部在
//! `domain` / `service`，这里一条都不复制。
//!
//! # 两阶段：先给计划，再落盘
//!
//! `add` / `update` 都是「先算计划（零写入）→ 展示 → 用户确认 → 执行」。
//! `--dry-run` 停在第一步；`-y` 只跳过确认提问，不跳过计划展示。
//!
//! 不带项目时 `add` 进 [`server_mod_pick`] 的搜索向导（浏览 / 搜索 / 翻页 /
//! 选版本），选完仍回到这条两阶段流程，`--dry-run` 与 `-y` 的语义因此与
//! 非交互路径完全一致。
//!
//! # 回显
//!
//! 识别、查候选版本、下载都是网络操作：每一步都有 spinner，下载阶段切进度条。
//! `--output json` 下全部隐藏，stdout 仍是单行 JSON。

use crate::bootstrap;
use crate::cli::ModAction;
use crate::commands::server_mod_pick;
use crate::diagnostic::AppDiagnostic;
use crate::i18n::{pick, text};
use crate::locale::Lang;
use crate::output::{Printer, human_bytes};
use crate::progress::{BarSink, TaskSpinner};
use anyhow::Result;
use comfy_table::Table;
use mcsup::domain::mod_::{FitVerdict, Risk, ServerFit, SideSupport};
use mcsup::domain::server::content::{ContentKind, UpdateDecision};
use mcsup::port::CancellationToken;
use mcsup::service::{
    ApplyOptions, InstallPlan, InstallSpec, ListOptions, ModEntry, ModList, ModService, UpdatePlan,
};
use serde::Serialize;

/// 子命令入口
pub(crate) async fn run(action: ModAction, printer: Printer) -> Result<()> {
    match action {
        ModAction::List {
            name,
            kind,
            project_info,
            source,
            outdated,
        } => {
            list(
                name.as_deref(),
                kind.as_deref(),
                project_info,
                source.as_deref(),
                outdated,
                printer,
            )
            .await
        }
        ModAction::Add {
            name,
            spec,
            version,
            kind,
            source,
            prerelease,
            force,
            dry_run,
        } => {
            add(
                name.as_deref(),
                spec,
                version.as_deref(),
                kind.as_deref(),
                source.as_deref(),
                prerelease,
                force,
                dry_run,
                printer,
            )
            .await
        }
        ModAction::Remove {
            name,
            m,
            kind,
            dry_run,
        } => remove(name.as_deref(), m, kind.as_deref(), dry_run, printer),
        ModAction::Update {
            name,
            m,
            kind,
            source,
            force,
            dry_run,
        } => {
            update(
                name.as_deref(),
                m.as_deref(),
                kind.as_deref(),
                source.as_deref(),
                force,
                dry_run,
                printer,
            )
            .await
        }
        ModAction::Outdated { name, kind, source } => {
            outdated(name.as_deref(), kind.as_deref(), source.as_deref(), printer).await
        }
    }
}

// ---------------------------------------------------------------- list

/// `server mod list`
async fn list(
    name: Option<&str>,
    kind: Option<&str>,
    project_info: bool,
    source: Option<&str>,
    only_outdated: bool,
    printer: Printer,
) -> Result<()> {
    let lang = printer.lang();
    let name = crate::commands::server_ops::require_server_name(name, "server mod list", printer)?;
    let kind = parse_kind(kind, lang)?;
    let mc = bootstrap::open_mcsup_with_remotes()?;

    // 识别要联网，只读命令默认不联网；`--outdated` 依赖识别结果，因此自动打开
    let identify = project_info || only_outdated;
    let options = ListOptions {
        kind,
        identify,
        source,
        updates: only_outdated,
    };
    let spinner = TaskSpinner::new(
        pick(
            lang,
            format!("正在读取 {} 目录…", kind.install_dir().display()),
            format!("reading the {} directory…", kind.install_dir().display()),
        ),
        printer.is_human(),
    );
    let listed = mc.mods.list(&name, options).await;
    spinner.finish();
    let listed = listed?;

    let entries: Vec<&ModEntry> = if only_outdated {
        listed
            .mods
            .iter()
            .filter(|entry| matches!(entry.update, Some(UpdateDecision::Available { .. })))
            .collect()
    } else {
        listed.mods.iter().collect()
    };

    let rows: Vec<ModEntryJson> = entries.iter().map(|entry| entry_json(entry)).collect();
    printer.print_rows(&rows, &empty_hint(kind, lang), || {
        mods_table(&entries, lang)
    })?;
    // 只列「有更新」时，没被识别的条目不会出现在表里：明说个数，
    // 否则用户会把「没列出来」读成「已是最新」
    if only_outdated {
        let unknown = listed
            .mods
            .iter()
            .filter(|entry| entry.update.is_none())
            .count();
        if unknown > 0 {
            let warn = pick(
                lang,
                format!("{unknown} 个文件无法判断是否有更新（未识别）"),
                format!("{unknown} file(s) could not be checked (not identified)"),
            );
            if printer.is_human() {
                printer.print_warning(&warn);
            } else {
                crate::output::print_error_line(&format!("mcsup: {warn}"));
            }
        }
    }
    report_unreadable(&listed, printer);
    // 装不进这台服务端的文件必须逐条说明：留在 mods/ 里会让服务端起不来
    report_fit_problems(&listed, printer);
    // 「适配」只答和这台服务端对不对得上；「风险」答的是 jar 自己能不能在
    // 服务端跑（仅客户端、缺必需依赖）。后者不合格同样起不来，也要说清楚，
    // 否则用户只能从服务端日志里的一行 Fabric 报错反推。
    report_blockers(&listed, printer);
    Ok(())
}

/// 「一个都没有」的文案：按类别说准目录名
fn empty_hint(kind: ContentKind, lang: Lang) -> String {
    let dir = kind.install_dir().display().to_string();
    pick(
        lang,
        format!("{dir}/ 里还没有任何内容"),
        format!("nothing installed in {dir}/ yet"),
    )
}

// ---------------------------------------------------------------- add

/// `server mod add`
///
/// 项目来源三选一，三者都在同一层决定：
///
/// - 直接给项目 ID / slug / 链接：不再提问
/// - `@search <关键词>`：进向导并从该关键词的第一页开始
/// - 什么都不给：交互下进向导（推荐列表起步，可搜索可翻页）；非交互报用法错误
#[allow(clippy::too_many_arguments)] // 每个参数对应一个 clap 选项，收结构体只会多一层包装
async fn add(
    name: Option<&str>,
    spec: Option<String>,
    version: Option<&str>,
    kind: Option<&str>,
    source: Option<&str>,
    prerelease: bool,
    force: bool,
    dry_run: bool,
    printer: Printer,
) -> Result<()> {
    let lang = printer.lang();
    let name = crate::commands::server_ops::require_server_name(name, "server mod add", printer)?;
    let kind = parse_kind(kind, lang)?;
    let mc = bootstrap::open_mcsup_with_remotes()?;
    let options = server_mod_pick::PickOptions {
        kind,
        source,
        allow_prerelease: prerelease,
    };

    let (project, version) =
        resolve_target(&mc.mods, &name, spec, version, options, printer).await?;

    let spinner = TaskSpinner::new(
        pick(
            lang,
            "正在确定版本…".to_owned(),
            "resolving the version…".to_owned(),
        ),
        printer.is_human(),
    );
    let planned = mc
        .mods
        .plan_install(
            &name,
            &project,
            kind,
            InstallSpec {
                version: version.as_deref(),
                source,
                allow_prerelease: prerelease,
            },
        )
        .await;
    spinner.finish();
    let plan = planned?;

    print_plan(&plan, printer)?;
    if dry_run {
        return Ok(());
    }
    if !crate::prompts::confirm_or_yes(printer, &install_question(&plan, lang), true)? {
        return Err(AppDiagnostic::cancelled().into());
    }

    let installed = download_and_apply(&mc.mods, &name, &plan, force, printer).await?;
    report_installed(&name, &installed, printer)
}

/// 从 `SPEC` 得到项目标识与版本
///
/// 三种写法各自对应一条路径，闭合在 `server_mod_pick::pick_mod` 与
/// `resolve_spec` 两个出口上：向导负责「搜索 → 选项目 → 选版本」，其余写法
/// 只做字符串解析。`version` 在向导路径里是版本提示（`--version` 与候选对
/// 不上就报错），在直给路径里原样交给服务端做精确版本。
async fn resolve_target(
    mods: &ModService,
    name: &str,
    spec: Option<String>,
    version: Option<&str>,
    options: server_mod_pick::PickOptions<'_>,
    printer: Printer,
) -> Result<(String, Option<String>)> {
    let lang = printer.lang();
    match spec.as_deref().map(str::trim) {
        // 没给项目：交互下走搜索向导，非交互没有可自动选择的信息
        None if printer.is_interactive() => {
            let picked =
                server_mod_pick::pick_mod(mods, name, options, None, version, printer).await?;
            Ok((picked.project_id, picked.version_id))
        }
        None => Err(AppDiagnostic::usage(
            pick(
                lang,
                "server mod add 缺少项目".to_owned(),
                "server mod add requires a project".to_owned(),
            ),
            Some(pick(
                lang,
                "交互模式直接运行 mcsup server mod add <名称> 搜索选择；脚本请给项目 ID".to_owned(),
                "run `mcsup server mod add <name>` to search and pick; scripts pass a project id"
                    .to_owned(),
            )),
        )
        .into()),
        Some("") => Err(AppDiagnostic::usage(
            pick(lang, "项目标识为空".to_owned(), "empty project".to_owned()),
            None,
        )
        .into()),
        // `@search <关键词>`：进同一个向导，只是从关键词的第一页开始
        Some(spec) if spec.starts_with("@search") => {
            let keyword = spec.trim_start_matches("@search").trim();
            if !printer.is_interactive() {
                return Err(AppDiagnostic::usage(
                    pick(
                        lang,
                        "@search 需要交互选择，非交互模式请直接给项目 ID".to_owned(),
                        "@search needs an interactive terminal; pass a project id instead"
                            .to_owned(),
                    ),
                    None,
                )
                .into());
            }
            if keyword.is_empty() {
                return Err(AppDiagnostic::usage(
                    pick(
                        lang,
                        "请给出搜索关键词".to_owned(),
                        "missing search keyword".to_owned(),
                    ),
                    Some(pick(
                        lang,
                        "mcsup server mod add <名称> '@search <关键词>'".to_owned(),
                        "mcsup server mod add <name> '@search <keyword>'".to_owned(),
                    )),
                )
                .into());
            }
            let picked =
                server_mod_pick::pick_mod(mods, name, options, Some(keyword), version, printer)
                    .await?;
            Ok((picked.project_id, picked.version_id))
        }
        Some(spec) => Ok((resolve_spec(spec, lang)?, version.map(<str>::to_owned))),
    }
}

/// 下载 + 检查 + 落盘，全程一条进度回显
async fn download_and_apply(
    mods: &ModService,
    name: &str,
    plan: &InstallPlan,
    force: bool,
    printer: Printer,
) -> Result<mcsup::service::ModInstalled> {
    let lang = printer.lang();
    let label = pick(
        lang,
        format!("下载 {}", plan.file),
        format!("downloading {}", plan.file),
    );
    let spinner = TaskSpinner::new(label.clone(), printer.is_human());
    let sink = BarSink::new(spinner.handle(), label);
    let cancel = CancellationToken::new();
    let result = mods
        .install(
            name,
            plan,
            ApplyOptions {
                allow_blocking: force,
                progress: Some(&sink),
                cancel: &cancel,
            },
        )
        .await;
    spinner.finish();
    result.map_err(anyhow::Error::from)
}

// ---------------------------------------------------------------- remove

/// `server mod remove`
fn remove(
    name: Option<&str>,
    selector: Option<String>,
    kind: Option<&str>,
    dry_run: bool,
    printer: Printer,
) -> Result<()> {
    let lang = printer.lang();
    let name =
        crate::commands::server_ops::require_server_name(name, "server mod remove", printer)?;
    let kind = parse_kind(kind, lang)?;
    let selector = crate::commands::server_ops::require_text(
        selector,
        &pick(lang, "缺少要删除的 mod", "missing mod to remove"),
        "mcsup server mod remove <name> <文件或 modid>",
        printer,
    )?;
    let mc = bootstrap::open_mcsup_with_remotes()?;
    let removed = mc.mods.remove(&name, &selector, kind, dry_run)?;
    let files: Vec<String> = removed
        .files
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    let payload = RemoveJson {
        server: &name,
        removed: &files,
        dry_run: removed.dry_run,
    };
    let human = if removed.dry_run {
        pick(
            lang,
            format!("将删除 {} 个文件：{}", files.len(), files.join("、")),
            format!("would remove {} file(s): {}", files.len(), files.join(", ")),
        )
    } else {
        pick(
            lang,
            format!("已删除 {} 个文件：{}", files.len(), files.join("、")),
            format!("removed {} file(s): {}", files.len(), files.join(", ")),
        )
    };
    printer.print_outcome(&human, &payload)
}

// ---------------------------------------------------------------- outdated

/// `server mod outdated`
async fn outdated(
    name: Option<&str>,
    kind: Option<&str>,
    source: Option<&str>,
    printer: Printer,
) -> Result<()> {
    let lang = printer.lang();
    let name =
        crate::commands::server_ops::require_server_name(name, "server mod outdated", printer)?;
    let kind = parse_kind(kind, lang)?;
    let mc = bootstrap::open_mcsup_with_remotes()?;
    let plan = plan_update(&mc.mods, &name, kind, None, source, printer).await?;
    let payload = update_plan_json(&name, &plan);
    if plan.items.is_empty() {
        let human = pick(
            lang,
            "没有可更新的 mod".to_owned(),
            "no mods to update".to_owned(),
        );
        printer.print_outcome(&human, &payload)?;
        // 一条更新都没有时更要说清「哪些没判断出来」，否则等于宣称全都最新
        report_skipped(&plan, printer);
        return Ok(());
    }
    // JSON 一律输出带 `server` / `skipped` 的完整对象：有没有更新项都同一个形状，
    // 脚本才不会因为「这次恰好没有更新」而解析失败
    printer.print_table_or_payload(
        &payload.items,
        text(lang, "没有可更新的 mod", "no mods to update"),
        || update_table(&plan, lang),
        &payload,
    )?;
    report_skipped(&plan, printer);
    Ok(())
}

// ---------------------------------------------------------------- update

/// `server mod update`
#[allow(clippy::too_many_arguments)] // 每个参数对应一个 clap 选项，收结构体只会多一层包装
async fn update(
    name: Option<&str>,
    selector: Option<&str>,
    kind: Option<&str>,
    source: Option<&str>,
    force: bool,
    dry_run: bool,
    printer: Printer,
) -> Result<()> {
    let lang = printer.lang();
    let name =
        crate::commands::server_ops::require_server_name(name, "server mod update", printer)?;
    let kind = parse_kind(kind, lang)?;
    let mc = bootstrap::open_mcsup_with_remotes()?;
    let plan = plan_update(&mc.mods, &name, kind, selector, source, printer).await?;
    let payload = update_plan_json(&name, &plan);
    if plan.items.is_empty() {
        let human = pick(
            lang,
            "没有可更新的 mod".to_owned(),
            "no mods to update".to_owned(),
        );
        printer.print_outcome(&human, &payload)?;
        report_skipped(&plan, printer);
        return Ok(());
    }
    // 同 `outdated`：形状不随内容变化
    printer.print_table_or_payload(
        &payload.items,
        text(lang, "没有可更新的 mod", "no mods to update"),
        || update_table(&plan, lang),
        &payload,
    )?;
    report_skipped(&plan, printer);

    if dry_run {
        return Ok(());
    }
    let question = pick(
        lang,
        format!("更新以上 {} 个 mod？", plan.items.len()),
        format!("update these {} mod(s)?", plan.items.len()),
    );
    if !crate::prompts::confirm_or_yes(printer, &question, true)? {
        return Err(AppDiagnostic::cancelled().into());
    }

    let label = pick(lang, "正在更新…".to_owned(), "updating…".to_owned());
    let spinner = TaskSpinner::new(label.clone(), printer.is_human());
    let sink = BarSink::new(spinner.handle(), label);
    let cancel = CancellationToken::new();
    let applied = mc
        .mods
        .apply_update(
            &name,
            &plan,
            ApplyOptions {
                allow_blocking: force,
                progress: Some(&sink),
                cancel: &cancel,
            },
        )
        .await;
    spinner.finish();
    let applied = applied?;

    let changes: Vec<ChangedJson> = applied
        .changes
        .iter()
        .map(|change| ChangedJson {
            file: change.file.clone(),
            from_version: change.from_version.clone(),
            to_version: change.to_version.clone(),
            risk: change.installed.risk.as_str().to_owned(),
            removed: change
                .removed
                .as_ref()
                .map(|path| path.display().to_string()),
        })
        .collect();
    let out = UpdateAppliedJson {
        server: &name,
        changes: &changes,
    };
    let human = pick(
        lang,
        format!("已更新 {} 个 mod", applied.changes.len()),
        format!("updated {} mod(s)", applied.changes.len()),
    );
    printer.print_outcome(&human, &out)?;
    // 换了文件名就等于删了旧文件：不说明的话用户会在目录里找不见旧文件
    for change in &applied.changes {
        if let Some(removed) = &change.removed {
            let note = pick(
                lang,
                format!("  ↳ 已删除旧文件 {}", removed.display()),
                format!("  ↳ removed the old file {}", removed.display()),
            );
            printer.print_section(&note);
        }
    }
    Ok(())
}

/// 算更新计划（带 spinner）：识别 + 逐项查候选
async fn plan_update(
    mods: &ModService,
    name: &str,
    kind: ContentKind,
    selector: Option<&str>,
    source: Option<&str>,
    printer: Printer,
) -> Result<UpdatePlan> {
    let lang = printer.lang();
    let spinner = TaskSpinner::new(
        pick(
            lang,
            "正在检查更新…".to_owned(),
            "checking for updates…".to_owned(),
        ),
        printer.is_human(),
    );
    let planned = mods
        .plan_update(
            name,
            selector,
            ListOptions {
                kind,
                identify: true,
                source,
                updates: true,
            },
        )
        .await;
    spinner.finish();
    planned.map_err(anyhow::Error::from)
}

// ---------------------------------------------------------------- 参数解析

/// `--kind`：只接受服务端从**顶层目录**直接装载的类别
///
/// 其余类别不是「不好用」，而是落点根本不在这里：数据包要进
/// `world/datapacks/`，服务端资源包由 `server.properties` 下发。把它们写进
/// `<server>/datapacks/` 或 `resourcepacks/` 只是放一个永远不会被读到的文件，
/// 所以入口直接拒绝并说清原因，而不是默默装进去。
fn parse_kind(raw: Option<&str>, lang: Lang) -> Result<ContentKind> {
    let Some(raw) = raw else {
        return Ok(ContentKind::Mod);
    };
    let kind = ContentKind::parse(raw).map_err(|_| {
        AppDiagnostic::usage(
            pick(
                lang,
                format!("未知的内容类别 {raw}"),
                format!("unknown kind: {raw}"),
            ),
            Some(pick(
                lang,
                "可用：mod / plugin".to_owned(),
                "available: mod / plugin".to_owned(),
            )),
        )
    })?;
    match kind {
        ContentKind::Mod | ContentKind::Plugin => Ok(kind),
        ContentKind::DataPack => Err(AppDiagnostic::usage(
            pick(
                lang,
                "数据包装在 world/datapacks/ 里，本版本还不支持".to_owned(),
                "datapacks live in world/datapacks/, which this version does not manage".to_owned(),
            ),
            Some(pick(
                lang,
                "需要数据包时请手动放到 <服务端>/world/datapacks/".to_owned(),
                "place datapacks in <server>/world/datapacks/ manually".to_owned(),
            )),
        )
        .into()),
        ContentKind::ResourcePack => Err(AppDiagnostic::usage(
            pick(
                lang,
                "服务端资源包由 server.properties 下发，不是本地目录".to_owned(),
                "server resource packs are delivered via server.properties, not a local directory"
                    .to_owned(),
            ),
            None,
        )
        .into()),
        _ => Err(AppDiagnostic::usage(
            pick(
                lang,
                format!("{raw} 不能装到服务端"),
                format!("{raw} cannot be installed on a server"),
            ),
            None,
        )
        .into()),
    }
}

/// 把用户给的项目标识解析成上游项目标识
///
/// 只处理两种写法：项目 ID / slug、`https://modrinth.com/<type>/<slug>` 链接。
/// `@search` 与「什么都不给」都由 [`resolve_target`] 先分流到搜索向导，不经过
/// 这里。别的站点链接一律报用法错误——把 `curseforge.com/...` 当 slug 传给
/// 上游只会得到「没找到」，用户看不出是「不支持这个站点」。
fn resolve_spec(spec: &str, lang: Lang) -> Result<String> {
    if spec.starts_with("http://") || spec.starts_with("https://") {
        return slug_from_url(spec, lang);
    }
    Ok(spec.to_owned())
}

/// 从 modrinth.com 链接里取 slug
fn slug_from_url(raw: &str, lang: Lang) -> Result<String> {
    let rest = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))
        .unwrap_or(raw);
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = host.split(':').next().unwrap_or(host);
    if !(host.eq_ignore_ascii_case("modrinth.com") || host.eq_ignore_ascii_case("www.modrinth.com"))
    {
        return Err(AppDiagnostic::usage(
            pick(
                lang,
                format!("不支持的来源站点：{host}"),
                format!("unsupported source site: {host}"),
            ),
            Some(pick(
                lang,
                "目前只支持 modrinth.com 的链接，或直接给项目 ID / slug".to_owned(),
                "only modrinth.com links are supported; you can also pass a project id or slug"
                    .to_owned(),
            )),
        )
        .into());
    }
    // 路径形如 `<type>/<slug>[/...]`：取第二段
    let mut segments = path.split('/').filter(|segment| !segment.is_empty());
    let _type = segments.next();
    let slug = segments.next().unwrap_or_default();
    if slug.is_empty() {
        return Err(AppDiagnostic::usage(
            pick(
                lang,
                format!("链接里没有项目标识：{raw}"),
                format!("no project in this link: {raw}"),
            ),
            None,
        )
        .into());
    }
    Ok(slug.to_owned())
}

// ---------------------------------------------------------------- 渲染

/// 确认文案
fn install_question(plan: &InstallPlan, lang: Lang) -> String {
    let title = plan.project_title.as_deref().unwrap_or(&plan.project_id);
    let replaces = if plan.replaces.is_some() {
        pick(lang, "（会替换同名文件）", " (replaces the existing file)")
    } else {
        String::new()
    };
    pick(
        lang,
        format!(
            "安装 {title} {} 到 {}？{replaces}",
            plan.version_number,
            plan.target.display()
        ),
        format!(
            "install {title} {} to {}?{replaces}",
            plan.version_number,
            plan.target.display()
        ),
    )
}

/// 计划展示
fn print_plan(plan: &InstallPlan, printer: Printer) -> Result<()> {
    let lang = printer.lang();
    let payload = PlanJson {
        source: plan.source,
        project_id: &plan.project_id,
        project_title: plan.project_title.as_deref(),
        version_id: &plan.version_id,
        version_number: &plan.version_number,
        kind: plan.kind.as_str(),
        file: &plan.file,
        size: plan.size,
        target: plan.target.display().to_string(),
        replaces: plan
            .replaces
            .as_ref()
            .map(|path| path.display().to_string()),
    };
    let title = plan.project_title.as_deref().unwrap_or(&plan.project_id);
    let mut table = crate::output::compact_table();
    table.set_header([
        text(lang, "项", "field").to_owned(),
        text(lang, "值", "value").to_owned(),
    ]);
    table.add_row([
        text(lang, "项目", "project").to_owned(),
        format!("{title} ({})", plan.project_id),
    ]);
    table.add_row([
        text(lang, "来源", "source").to_owned(),
        plan.source.to_owned(),
    ]);
    table.add_row([
        text(lang, "版本", "version").to_owned(),
        format!("{} ({})", plan.version_number, plan.version_id),
    ]);
    table.add_row([
        text(lang, "类别", "kind").to_owned(),
        plan.kind.as_str().to_owned(),
    ]);
    table.add_row([text(lang, "文件", "file").to_owned(), plan.file.clone()]);
    table.add_row([
        text(lang, "大小", "size").to_owned(),
        human_bytes(plan.size),
    ]);
    table.add_row([
        text(lang, "落点", "target").to_owned(),
        plan.target.display().to_string(),
    ]);
    if let Some(replaces) = &plan.replaces {
        table.add_row([
            text(lang, "替换", "replaces").to_owned(),
            replaces.display().to_string(),
        ]);
    }
    if printer.is_human() {
        printer.print_table(&table);
        return Ok(());
    }
    printer.print_outcome("", &payload)
}

/// 安装结果回显
fn report_installed(
    name: &str,
    installed: &mcsup::service::ModInstalled,
    printer: Printer,
) -> Result<()> {
    let lang = printer.lang();
    let payload = InstalledJson {
        server: name,
        file: &installed.file,
        path: installed.path.display().to_string(),
        size: installed.size,
        risk: installed.risk.as_str(),
        missing_deps: &installed.missing_deps,
        evidence: &installed.evidence,
        replaced: installed
            .replaced
            .as_ref()
            .map(|path| path.display().to_string()),
    };
    let human = pick(
        lang,
        format!("已安装 {} 到 {}", installed.file, installed.path.display()),
        format!(
            "installed {} to {}",
            installed.file,
            installed.path.display()
        ),
    );
    printer.print_outcome(&human, &payload)?;
    // 风险非 ok 时必须说清依据：装了不让用户知道就失去了检查的意义
    if installed.risk != Risk::Ok {
        let warn = pick(
            lang,
            format!(
                "注意：该 mod 的检查结论为 {}，已按你的要求安装",
                installed.risk
            ),
            format!(
                "warning: this mod was flagged as {}; installed as requested",
                installed.risk
            ),
        );
        printer.print_warning(&warn);
        for item in &installed.evidence {
            printer.print_section(&format!("  ↳ {item}"));
        }
        if !installed.missing_deps.is_empty() {
            let deps = pick(
                lang,
                format!("  ↳ 缺少必需依赖: {}", installed.missing_deps.join(", ")),
                format!(
                    "  ↳ missing required deps: {}",
                    installed.missing_deps.join(", ")
                ),
            );
            printer.print_section(&deps);
        }
    }
    Ok(())
}

/// 列表表格
fn mods_table(entries: &[&ModEntry], lang: Lang) -> Table {
    let mut table = crate::output::compact_table();
    table.set_header([
        text(lang, "文件", "file"),
        text(lang, "modid", "modid"),
        text(lang, "版本", "version"),
        text(lang, "服务端", "server"),
        text(lang, "风险", "risk"),
        text(lang, "适配", "fit"),
        text(lang, "上游", "upstream"),
    ]);
    for entry in entries {
        let capability = &entry.report.capability;
        table.add_row([
            entry.file.clone(),
            capability.id.clone().unwrap_or_else(|| "-".to_owned()),
            capability.version.clone().unwrap_or_else(|| "-".to_owned()),
            capability.side.server.as_str().to_owned(),
            entry.report.risk.as_str().to_owned(),
            fit_text(entry.report.fit.as_ref(), lang).to_owned(),
            upstream_text(entry),
        ]);
    }
    table
}

/// 匹配结论展示
///
/// 「适配」与「风险」是两件事：前者答「和这台服务端对不对得上」，后者答
/// 「这个 jar 本身是不是服务端的东西」。合成一列就说不清到底是哪种问题。
fn fit_text(fit: Option<&ServerFit>, lang: Lang) -> &'static str {
    match fit.map(|f| f.verdict) {
        Some(FitVerdict::Installable) => text(lang, "✓ 可装", "✓ ok"),
        Some(FitVerdict::Unclear) => text(lang, "? 待确认", "? unclear"),
        Some(FitVerdict::Rejected) => text(lang, "✗ 不匹配", "✗ mismatch"),
        None => "-",
    }
}

/// 装不进这台服务端的条目：逐条给理由
///
/// 这类文件留在 `mods/` 里会让服务端直接起不来，光在表格里打个标记不够。
fn report_fit_problems(list: &ModList, printer: Printer) {
    let lang = printer.lang();
    let problems: Vec<&ModEntry> = list
        .mods
        .iter()
        .filter(|entry| {
            entry
                .report
                .fit
                .as_ref()
                .is_some_and(|fit| fit.verdict == FitVerdict::Rejected)
        })
        .collect();
    if problems.is_empty() {
        return;
    }
    let warn = pick(
        lang,
        format!("{} 个文件装不进这台服务端", problems.len()),
        format!("{} file(s) cannot run on this server", problems.len()),
    );
    printer.print_warning(&warn);
    for entry in problems {
        let Some(fit) = &entry.report.fit else {
            continue;
        };
        for issue in &fit.issues {
            let line = format!("  {}: [{}] {}", entry.file, issue.dimension, issue.detail);
            if printer.is_human() {
                printer.print_section(&line);
            } else {
                crate::output::print_error_line(&format!("mcsup: {line}"));
            }
        }
    }
}

/// 会让服务端起不来的条目：逐条给理由
///
/// 风险为 `blocking` 有两种来源：jar 本身只能在客户端跑，或者它依赖的必需
/// 模组不在 `mods/` 里。两种情况加载器都会在解析阶段直接终止启动，而服务端
/// 日志里只有加载器的英文报错——用户得自己把 modid 对上文件名。
fn report_blockers(list: &ModList, printer: Printer) {
    let lang = printer.lang();
    let blocked: Vec<&ModEntry> = list
        .mods
        .iter()
        .filter(|entry| entry.report.risk == Risk::Blocking)
        .collect();
    if blocked.is_empty() {
        return;
    }
    let warn = pick(
        lang,
        format!("{} 个文件会让服务端起不来", blocked.len()),
        format!(
            "{} file(s) will stop the server from starting",
            blocked.len()
        ),
    );
    printer.print_warning(&warn);
    for entry in blocked {
        let line = format!("  {}: {}", entry.file, blocker_reason(entry, lang));
        if printer.is_human() {
            printer.print_section(&line);
        } else {
            crate::output::print_error_line(&format!("mcsup: {line}"));
        }
    }
}

/// 起不来的原因：缺依赖优先，其次仅客户端
fn blocker_reason(entry: &ModEntry, lang: Lang) -> String {
    let report = &entry.report;
    if !report.missing_deps.is_empty() {
        let names = report.missing_deps.join("、");
        return pick(
            lang,
            format!("缺少必需依赖 {names}，加载器会拒绝启动"),
            format!("missing required dependency {names}; the loader will refuse to start"),
        );
    }
    if report.capability.side.server == SideSupport::Unsupported {
        return pick(
            lang,
            "仅客户端，服务端不会加载它".to_owned(),
            "client-only; a server never loads it".to_owned(),
        );
    }
    pick(
        lang,
        "被判定为不可用于服务端".to_owned(),
        "judged unusable on a server".to_owned(),
    )
}

/// 上游列：已识别时给项目 + 版本，未识别时明说「未识别」
fn upstream_text(entry: &ModEntry) -> String {
    match &entry.identity {
        Some(identity) => {
            let name = identity
                .title
                .clone()
                .or_else(|| identity.slug.clone())
                .unwrap_or_else(|| identity.project_id.clone());
            format!("{name} {}", identity.version_number)
        }
        None => "-".to_owned(),
    }
}

/// 更新计划表格
fn update_table(plan: &UpdatePlan, lang: Lang) -> Table {
    let mut table = crate::output::compact_table();
    table.set_header([
        text(lang, "文件", "file"),
        text(lang, "项目", "project"),
        text(lang, "当前", "current"),
        text(lang, "可更新到", "available"),
    ]);
    for item in &plan.items {
        table.add_row([
            item.file.clone(),
            item.project_id.clone(),
            item.from_version.clone(),
            item.to_version.clone(),
        ]);
    }
    table
}

/// 无法判断的文件：不提示就等于让用户以为它们已是最新
fn report_skipped(plan: &UpdatePlan, printer: Printer) {
    if plan.skipped.is_empty() {
        return;
    }
    let lang = printer.lang();
    let warn = pick(
        lang,
        format!("{} 个文件无法判断是否有更新", plan.skipped.len()),
        format!("{} file(s) could not be checked", plan.skipped.len()),
    );
    printer.print_warning(&warn);
    // JSON 模式下 stdout 只能是数据，结论性的提示落到 stderr：
    // 静默丢掉等于让脚本以为「全都最新」
    for (file, reason) in &plan.skipped {
        if printer.is_human() {
            printer.print_section(&format!("  {file}: {reason}"));
        } else {
            crate::output::print_error_line(&format!("mcsup: {file}: {reason}"));
        }
    }
}

/// 读不出来的文件
fn report_unreadable(list: &ModList, printer: Printer) {
    if list.unreadable.is_empty() {
        return;
    }
    let lang = printer.lang();
    let warn = pick(
        lang,
        format!("{} 个文件无法读取", list.unreadable.len()),
        format!("{} file(s) could not be read", list.unreadable.len()),
    );
    printer.print_warning(&warn);
    for (path, reason) in &list.unreadable {
        if printer.is_human() {
            printer.print_section(&format!("  {path}: {reason}"));
        } else {
            crate::output::print_error_line(&format!("mcsup: {path}: {reason}"));
        }
    }
}

// ---------------------------------------------------------------- JSON 契约

#[derive(Debug, Serialize)]
struct ModEntryJson {
    file: String,
    path: String,
    id: Option<String>,
    version: Option<String>,
    loaders: Vec<String>,
    server: String,
    risk: String,
    /// 与这台服务端的匹配结论（`mod inspect` 用同一形状）
    fit: Option<crate::commands::mod_inspect::FitJson>,
    missing_deps: Vec<String>,
    /// 上游项目 ID（未识别时为 `null`）
    project_id: Option<String>,
    /// 上游 slug（未识别时为 `null`）
    project_slug: Option<String>,
    /// 上游版本号（未识别时为 `null`）
    project_version: Option<String>,
    /// 更新状态：`up_to_date` / `available`；未查更新时为 `null`
    update: Option<String>,
    /// 有更新时的目标版本号
    update_version: Option<String>,
}

fn entry_json(entry: &ModEntry) -> ModEntryJson {
    let capability = &entry.report.capability;
    ModEntryJson {
        file: entry.file.clone(),
        path: capability.path.clone(),
        id: capability.id.clone(),
        version: capability.version.clone(),
        loaders: capability
            .loaders
            .iter()
            .map(|loader| loader.as_str().to_owned())
            .collect(),
        server: capability.side.server.as_str().to_owned(),
        risk: entry.report.risk.as_str().to_owned(),
        fit: entry
            .report
            .fit
            .as_ref()
            .map(crate::commands::mod_inspect::fit_json),
        missing_deps: entry.report.missing_deps.clone(),
        project_id: entry.identity.as_ref().map(|id| id.project_id.clone()),
        project_slug: entry.identity.as_ref().and_then(|id| id.slug.clone()),
        project_version: entry.identity.as_ref().map(|id| id.version_number.clone()),
        update: entry.update.as_ref().map(|decision| match decision {
            UpdateDecision::UpToDate => "up_to_date".to_owned(),
            UpdateDecision::Available { .. } => "available".to_owned(),
        }),
        update_version: match &entry.update {
            Some(UpdateDecision::Available { version_number, .. }) => Some(version_number.clone()),
            _ => None,
        },
    }
}

#[derive(Debug, Serialize)]
struct PlanJson<'a> {
    source: &'a str,
    project_id: &'a str,
    project_title: Option<&'a str>,
    version_id: &'a str,
    version_number: &'a str,
    kind: &'a str,
    file: &'a str,
    size: u64,
    target: String,
    replaces: Option<String>,
}

#[derive(Debug, Serialize)]
struct InstalledJson<'a> {
    server: &'a str,
    file: &'a str,
    path: String,
    size: u64,
    risk: &'a str,
    missing_deps: &'a [String],
    evidence: &'a [String],
    replaced: Option<String>,
}

#[derive(Debug, Serialize)]
struct RemoveJson<'a> {
    server: &'a str,
    removed: &'a [String],
    dry_run: bool,
}

#[derive(Debug, Serialize)]
struct UpdatePlanJson<'a> {
    server: &'a str,
    items: Vec<UpdateItemJson>,
    skipped: Vec<SkippedJson>,
}

#[derive(Debug, Serialize)]
struct UpdateItemJson {
    file: String,
    current: String,
    project_id: String,
    from_version: String,
    to_version: String,
    version_id: String,
    target: String,
}

#[derive(Debug, Serialize)]
struct SkippedJson {
    file: String,
    reason: String,
}

fn update_plan_json<'a>(name: &'a str, plan: &'a UpdatePlan) -> UpdatePlanJson<'a> {
    UpdatePlanJson {
        server: name,
        items: plan
            .items
            .iter()
            .map(|item| UpdateItemJson {
                file: item.file.clone(),
                current: item.current.display().to_string(),
                project_id: item.project_id.clone(),
                from_version: item.from_version.clone(),
                to_version: item.to_version.clone(),
                version_id: item.plan.version_id.clone(),
                target: item.plan.target.display().to_string(),
            })
            .collect(),
        skipped: plan
            .skipped
            .iter()
            .map(|(file, reason)| SkippedJson {
                file: file.clone(),
                reason: reason.clone(),
            })
            .collect(),
    }
}

#[derive(Debug, Serialize)]
struct ChangedJson {
    file: String,
    from_version: String,
    to_version: String,
    risk: String,
    /// 换名更新时被删掉的旧文件
    removed: Option<String>,
}

#[derive(Debug, Serialize)]
struct UpdateAppliedJson<'a> {
    server: &'a str,
    changes: &'a [ChangedJson],
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_from_url_accepts_modrinth_links() {
        assert_eq!(
            slug_from_url("https://modrinth.com/mod/sodium", Lang::Zh).expect("slug"),
            "sodium"
        );
        assert_eq!(
            slug_from_url("https://modrinth.com/plugin/luckperms", Lang::Zh).expect("slug"),
            "luckperms"
        );
        // 末尾带斜杠或更深的路径也只取第二段
        assert_eq!(
            slug_from_url("https://modrinth.com/mod/sodium/versions", Lang::Zh).expect("slug"),
            "sodium"
        );
        assert_eq!(
            slug_from_url("https://www.modrinth.com/mod/sodium", Lang::Zh).expect("slug"),
            "sodium"
        );
    }

    /// 别的站点链接直接拒绝：当 slug 传给上游只会得到「没找到」
    #[test]
    fn slug_from_url_rejects_other_sites() {
        let err = slug_from_url("https://www.curseforge.com/minecraft/mc-mods/jei", Lang::Zh)
            .expect_err("必须拒绝");
        assert_eq!(
            crate::output::exit_code_of(&err),
            2,
            "不支持的站点是用法错误：{err}"
        );
    }

    /// 链接里没有项目段
    #[test]
    fn slug_from_url_rejects_missing_project() {
        let err = slug_from_url("https://modrinth.com/", Lang::Zh).expect_err("必须拒绝");
        assert_eq!(crate::output::exit_code_of(&err), 2);
    }

    #[test]
    fn parse_kind_defaults_to_mod_and_rejects_client_only() {
        assert_eq!(parse_kind(None, Lang::Zh).expect("mod"), ContentKind::Mod);
        assert_eq!(
            parse_kind(Some("plugin"), Lang::Zh).expect("plugin"),
            ContentKind::Plugin
        );
        let err = parse_kind(Some("shader"), Lang::Zh).expect_err("必须拒绝");
        assert_eq!(crate::output::exit_code_of(&err), 2);
        let err = parse_kind(Some("nope"), Lang::Zh).expect_err("必须拒绝");
        assert_eq!(crate::output::exit_code_of(&err), 2);
    }

    /// 数据包与资源包的落点不在本命令管理的目录里，一律拒绝而不是装错地方
    #[test]
    fn parse_kind_rejects_kinds_with_other_locations() {
        for raw in ["datapack", "resourcepack", "modpack"] {
            let err = parse_kind(Some(raw), Lang::Zh).expect_err("必须拒绝");
            assert_eq!(crate::output::exit_code_of(&err), 2, "{raw} 应是用法错误");
        }
    }
}
