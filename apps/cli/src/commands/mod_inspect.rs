//! `mcsup mod inspect`：报告 mod 文件的服务端可用性
//!
//! 只是入口：检测在 [`mcsup::service::ModInspectService`]，判定在
//! [`mcsup::domain::mod_`]。本文件负责收集路径、调服务、渲染结果。
//!
//! # 输出约定
//!
//! 表格只给结论，理由在下方逐条列出。判不出的条目**必须**带证据行说明
//! 为什么判不出——否则用户会以为工具有 bug。

use crate::cli::ModInspectAction;
use crate::diagnostic::AppDiagnostic;
use crate::i18n::text;
use crate::locale::Lang;
use crate::output::Printer;
use anyhow::Result;
use comfy_table::Table;
use mcsup::domain::mod_::{
    Confidence, FitVerdict, ModCapability, ModLoader, Risk, ServerFit, ServerTarget, SideSupport,
};
use mcsup::modkit::ZipModInspector;
use mcsup::port::mod_::ModInspector;
use mcsup::service::{ModAudit, ModInspectService, ModReport, collect_jars};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;

/// JSON 契约里的单条结果
#[derive(Debug, Serialize)]
struct ModJson {
    path: String,
    id: Option<String>,
    version: Option<String>,
    loaders: Vec<String>,
    mc_versions: McRangeJson,
    java_required: Option<u32>,
    java_compiled: Option<u32>,
    client: String,
    server: String,
    confidence: String,
    risk: String,
    missing_deps: Vec<String>,
    /// 依赖校验使用的目标加载器；`null` 表示未确定
    target_loader: Option<String>,
    /// 与目标服务端的匹配结论；未指定目标（无 `--mc` / `--server`）时为 `null`
    fit: Option<FitJson>,
    /// 判定所用的目标条件；未指定目标时为 `null`
    target: Option<TargetJson>,
    evidence: Vec<EvidenceJson>,
}

/// JSON 契约里的匹配结论
///
/// `server mod list` 复用同一形状：两条命令对「能不能装」必须给出同一种
/// 表达，否则脚本要按命令名分两套解析。
#[derive(Debug, Serialize)]
pub(crate) struct FitJson {
    /// `installable` / `unclear` / `rejected`
    pub(crate) verdict: String,
    /// `match` / `mismatch` / `unknown`
    pub(crate) loader: String,
    pub(crate) mc_version: String,
    pub(crate) java: String,
    /// 不匹配或判不出的逐条说明
    pub(crate) issues: Vec<FitIssueJson>,
}

/// JSON 契约里的一条匹配说明
#[derive(Debug, Serialize)]
pub(crate) struct FitIssueJson {
    /// `loader` / `mc_version` / `java`
    pub(crate) dimension: String,
    pub(crate) detail: String,
}

/// JSON 契约里的目标条件
#[derive(Debug, Serialize)]
struct TargetJson {
    loader: Option<String>,
    mc_version: Option<String>,
    java_major: Option<u32>,
}

/// 匹配结论转 JSON（`server mod list` 共用）
pub(crate) fn fit_json(fit: &ServerFit) -> FitJson {
    FitJson {
        verdict: fit.verdict.as_str().to_owned(),
        loader: fit.loader.as_str().to_owned(),
        mc_version: fit.mc_version.as_str().to_owned(),
        java: fit.java.as_str().to_owned(),
        issues: fit
            .issues
            .iter()
            .map(|issue| FitIssueJson {
                dimension: issue.dimension.as_str().to_owned(),
                detail: issue.detail.clone(),
            })
            .collect(),
    }
}

/// 目标条件转 JSON
fn target_json(target: &ServerTarget) -> Option<TargetJson> {
    if target.is_empty() {
        return None;
    }
    Some(TargetJson {
        loader: target.loader.map(|l| l.as_str().to_owned()),
        mc_version: target.mc_version.clone(),
        java_major: target.java_major,
    })
}

/// JSON 契约里的版本区间
#[derive(Debug, Serialize)]
struct McRangeJson {
    raw: String,
    min: Option<String>,
    max: Option<String>,
}

/// JSON 契约里的一条依据
#[derive(Debug, Serialize)]
struct EvidenceJson {
    source: String,
    detail: String,
}

pub(crate) async fn run(action: ModInspectAction, printer: Printer) -> Result<()> {
    match action {
        ModInspectAction::Inspect {
            paths,
            offline,
            loader,
            mc,
            server,
        } => {
            inspect(
                paths,
                offline,
                loader.as_deref(),
                mc.as_deref(),
                server.as_deref(),
                printer,
            )
            .await
        }
    }
}

async fn inspect(
    paths: Vec<PathBuf>,
    offline: bool,
    loader: Option<&str>,
    mc: Option<&str>,
    server: Option<&str>,
    printer: Printer,
) -> Result<()> {
    let jars = expand_paths(&paths)?;
    if jars.is_empty() {
        printer.print_warning(text(
            printer.lang(),
            "没有找到任何 jar 文件",
            "no jar files found",
        ));
        return Ok(());
    }

    // 进度回显：解压 + 字节码扫描在几十个 mod 上会超过数百毫秒
    let spinner = crate::progress::TaskSpinner::new(
        crate::i18n::pick(
            printer.lang(),
            "正在检测 mod…".to_owned(),
            "inspecting mods…".to_owned(),
        ),
        printer.is_human(),
    );
    // 目标加载器决定采信哪个清单的依赖；非法值报用法错误（2），不猜
    let loader = match loader {
        Some(raw) => {
            Some(ModLoader::parse(raw).ok_or_else(|| unknown_loader_error(printer.lang(), raw))?)
        }
        None => None,
    };
    let target = build_target(server, loader, mc, printer.lang())?;
    let service = inspect_service(offline, printer.lang())?;
    let audit = service
        .inspect_paths_for(&jars, !offline, Some(&target))
        .await;
    spinner.finish();
    let audit = audit?;

    render(&audit, &target, printer)
}

/// 组装目标服务端条件
///
/// `--server` 直接取那台服务端（加载器、MC 版本、Java 全部来自元数据），与
/// `server mod list` 用同一个构造函数；`--loader` / `--mc` 用来描述一台还没
/// 登记的服务端。两条路都走 [`mcsup::service::server_target`]，入口不自己
/// 推算 Java 版本。
fn build_target(
    server: Option<&str>,
    loader: Option<ModLoader>,
    mc: Option<&str>,
    lang: Lang,
) -> Result<ServerTarget> {
    if let Some(name) = server {
        let registry = crate::commands::server_ops::open_registry()?;
        let found = registry.get(name).map_err(|err| {
            AppDiagnostic::failure(
                crate::i18n::pick(
                    lang,
                    format!("找不到服务端 {name}：{err}"),
                    format!("server {name} not found: {err}"),
                ),
                None,
            )
        })?;
        return Ok(mcsup::service::server_target(&found));
    }
    Ok(ServerTarget {
        loader,
        mc_version: mc.map(ToOwned::to_owned),
        java_major: None,
        bridged_loaders: Vec::new(),
    })
}

/// 按 `--offline` 组装检测服务
///
/// 联网兜底是「本地判不出时才向上游查」，因此 `--offline` 必须真的少调一次
/// 上游：差别的唯一来源就是这里注入的服务。上游客户端构造失败（TLS 初始化
/// 之类）不降级——那会让「联网检测」静默变成「离线检测」，用户看不到任何
/// 提示却以为查过了。
fn inspect_service(offline: bool, lang: Lang) -> Result<ModInspectService> {
    let inspector: Arc<dyn ModInspector> = Arc::new(ZipModInspector::new());
    if offline {
        return Ok(ModInspectService::local(inspector));
    }
    let upstream = mcsup::provider::modrinth::ModrinthSource::new().map_err(|err| {
        AppDiagnostic::failure(
            crate::i18n::pick(
                lang,
                format!("初始化上游来源失败：{err}"),
                format!("cannot initialize upstream source: {err}"),
            ),
            Some(crate::i18n::pick(
                lang,
                "使用 --offline 只做本地检测".to_owned(),
                "use --offline to inspect locally only".to_owned(),
            )),
        )
    })?;
    Ok(ModInspectService::with_upstream(
        inspector,
        Arc::new(upstream),
    ))
}

/// 把用户给的路径展开成 jar 列表
///
/// 目录只取直接子项（见 `collect_jars` 的理由）；文件直接透传。
fn expand_paths(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for path in paths {
        if path.is_dir() {
            out.extend(collect_jars(path).map_err(anyhow::Error::from)?);
        } else {
            out.push(path.clone());
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// 「依赖按哪个加载器校验」的说明行
///
/// 说不清目标加载器时必须讲明白为什么没做校验，否则用户会以为检查过了。
fn target_loader_line(lang: Lang, target: Option<ModLoader>) -> String {
    match target {
        Some(loader) => crate::i18n::pick(
            lang,
            format!("依赖校验按 {} 口径", loader.as_str()),
            format!("dependency check uses the {} manifest", loader.as_str()),
        ),
        None => crate::i18n::pick(
            lang,
            "未确定目标加载器：这批文件分属多个加载器，只校验与加载器无关的依赖（可用 --loader 指定）",
            "no single target loader: the batch spans several loaders, so only loader-agnostic dependencies were checked (use --loader to pick one)",
        ),
    }
}

/// `--loader` 给了个不认识的加载器名：用法错误（退出码 2）
fn unknown_loader_error(lang: Lang, raw: &str) -> anyhow::Error {
    AppDiagnostic::usage(
        crate::i18n::pick(
            lang,
            format!("未知加载器 {raw}"),
            format!("unknown loader: {raw}"),
        ),
        Some(crate::i18n::pick(
            lang,
            "可用：fabric / quilt / forge / neoforge / legacy-forge / liteloader / bukkit"
                .to_owned(),
            "available: fabric / quilt / forge / neoforge / legacy-forge / liteloader / bukkit"
                .to_owned(),
        )),
    )
    .into()
}

/// 渲染检测结果
fn render(audit: &ModAudit, target: &ServerTarget, printer: Printer) -> Result<()> {
    let lang = printer.lang();
    let rows: Vec<ModJson> = audit
        .mods
        .iter()
        .map(|report| to_json(report, audit.target_loader, target))
        .collect();
    printer.print_rows(
        &rows,
        text(lang, "没有可报告的 mod", "nothing to report"),
        || build_table(audit, target, lang),
    )?;

    // 结论概览
    let (ok, review, blocking) = audit.counts();
    let summary = crate::i18n::pick(
        lang,
        format!("结论：{ok} 个可用，{review} 个需确认，{blocking} 个不可用于服务端"),
        format!("summary: {ok} usable, {review} need review, {blocking} server-incompatible"),
    );
    printer.print_section(&summary);
    printer.print_section(&target_loader_line(lang, audit.target_loader));
    // 匹配结论依赖目标条件，必须回显让用户核对「你是按什么判的」
    if !target.is_empty() {
        printer.print_section(&target_line(target, lang));
        let (fit_ok, unclear, rejected) = fit_counts(audit);
        let line = crate::i18n::pick(
            lang,
            format!("适配：{fit_ok} 个可装，{unclear} 个待确认，{rejected} 个不匹配"),
            format!("fit: {fit_ok} installable, {unclear} unclear, {rejected} rejected"),
        );
        printer.print_section(&line);
    }

    // 有问题的条目逐条给理由：blocking 要说清阻塞在哪，review 要说清为什么判不出
    for report in audit.mods.iter().filter(|r| r.risk != Risk::Ok) {
        print_reason_detail(report, printer);
    }
    // 匹配结论独立成段：装不进这台服务端的原因与「是不是服务端的东西」无关
    for report in audit.mods.iter().filter(|r| {
        r.fit
            .as_ref()
            .is_some_and(|f| f.verdict != FitVerdict::Installable)
    }) {
        print_fit_detail(report, printer);
    }

    // 读不出来的文件
    if !audit.unreadable.is_empty() {
        let warn_text = crate::i18n::pick(
            lang,
            format!("{} 个文件无法读取", audit.unreadable.len()),
            format!("{} file(s) could not be read", audit.unreadable.len()),
        );
        printer.print_warning(&warn_text);
        for (path, reason) in &audit.unreadable {
            printer.print_section(&format!("  {path}: {reason}"));
        }
    }

    Ok(())
}

/// 各匹配结论的计数
fn fit_counts(audit: &ModAudit) -> (usize, usize, usize) {
    let mut ok = 0;
    let mut unclear = 0;
    let mut rejected = 0;
    for fit in audit.mods.iter().filter_map(|r| r.fit.as_ref()) {
        match fit.verdict {
            FitVerdict::Installable => ok += 1,
            FitVerdict::Unclear => unclear += 1,
            FitVerdict::Rejected => rejected += 1,
        }
    }
    (ok, unclear, rejected)
}

/// 目标条件回显
fn target_line(target: &ServerTarget, lang: Lang) -> String {
    let mut parts = Vec::new();
    if let Some(loader) = target.loader {
        parts.push(loader.as_str().to_owned());
    }
    if let Some(mc) = &target.mc_version {
        parts.push(format!("MC {mc}"));
    }
    if let Some(java) = target.java_major {
        parts.push(format!("Java {java}"));
    }
    crate::i18n::pick(
        lang,
        format!("目标服务端：{}", parts.join(" / ")),
        format!("target server: {}", parts.join(" / ")),
    )
}

/// 一条匹配结论不理想的条目
fn print_fit_detail(report: &ModReport, printer: Printer) {
    let Some(fit) = &report.fit else {
        return;
    };
    let lang = printer.lang();
    let cap = &report.capability;
    let name = cap.id.as_deref().unwrap_or_else(|| file_name(&cap.path));
    let title = match fit.verdict {
        FitVerdict::Rejected => crate::i18n::pick(
            lang,
            format!("{name} 装不进这台服务端："),
            format!("{name} cannot be installed on this server:"),
        ),
        FitVerdict::Unclear => crate::i18n::pick(
            lang,
            format!("{name} 无法判定能否装入："),
            format!("{name} cannot be judged for this server:"),
        ),
        FitVerdict::Installable => return,
    };
    printer.print_section(&title);
    for issue in &fit.issues {
        printer.print_section(&format!("  ↳ [{}] {}", issue.dimension, issue.detail));
    }
}

/// 表格：只放结论，不放长理由
fn build_table(audit: &ModAudit, target: &ServerTarget, lang: Lang) -> Table {
    // 区间按目标加载器取值：表里显示的必须与判定用的是同一份
    let target_loader = target.loader.or(audit.target_loader);
    let mut table = crate::output::compact_table();
    let mut header = vec![
        text(lang, "mod", "mod"),
        text(lang, "加载器", "loader"),
        text(lang, "MC 区间", "MC range"),
        text(lang, "Java", "Java"),
        text(lang, "服务端", "server"),
        text(lang, "风险", "risk"),
    ];
    // 没有目标服务端时不给「适配」列：整列 unknown 只会误导
    let show_fit = !target.is_empty();
    if show_fit {
        header.push(text(lang, "适配", "fit"));
    }
    table.set_header(header);
    for report in &audit.mods {
        let cap = &report.capability;
        let name = cap.id.as_deref().unwrap_or_else(|| file_name(&cap.path));
        let loaders = if cap.loaders.is_empty() {
            text(lang, "未知", "unknown").to_owned()
        } else {
            cap.loaders
                .iter()
                .map(|l| l.as_str())
                .collect::<Vec<_>>()
                .join(",")
        };
        let java = match (cap.java_required, cap.java_compiled) {
            (Some(req), Some(comp)) if req == comp => comp.to_string(),
            (Some(req), Some(comp)) => format!("{req}(声明)/{comp}(编译)"),
            (Some(req), None) => req.to_string(),
            (None, Some(comp)) => comp.to_string(),
            (None, None) => "-".to_owned(),
        };
        let mut row = vec![
            name.to_owned(),
            loaders,
            range_text(cap, target_loader, lang),
            java,
            side_text(cap.side.server, lang).to_owned(),
            risk_text(report.risk, lang).to_owned(),
        ];
        if show_fit {
            row.push(fit_text(report.fit.as_ref(), lang).to_owned());
        }
        table.add_row(row);
    }
    table
}

/// 把非 ok 条目的理由摊开讲
///
/// 没有理由的结论用户无法采信，也无法判断工具是不是判错了——这正是要
/// 避免的「静默误判」。
fn print_reason_detail(report: &ModReport, printer: Printer) {
    let lang = printer.lang();
    let cap = &report.capability;
    let name = cap.id.as_deref().unwrap_or_else(|| file_name(&cap.path));

    let title = match report.risk {
        Risk::Blocking => crate::i18n::pick(
            lang,
            format!("{name} 不可用于服务端："),
            format!("{name} cannot run on a server:"),
        ),
        Risk::Review => crate::i18n::pick(
            lang,
            format!("{name} 需确认："),
            format!("{name} needs review:"),
        ),
        Risk::Ok => return,
    };
    printer.print_section(&title);

    // 阻塞的首要原因是服务端明确不支持
    if cap.side.server == SideSupport::Unsupported {
        let reason = crate::i18n::pick(
            lang,
            "  ↳ 服务端侧明确不支持（见下列依据）".to_owned(),
            "  ↳ explicitly unsupported on the server side (see evidence)".to_owned(),
        );
        printer.print_section(&reason);
    }
    // 缺依赖同样是阻塞原因，必须排在证据之前说清
    if !report.missing_deps.is_empty() {
        let deps = crate::i18n::pick(
            lang,
            format!("  ↳ 缺少必需依赖: {}", report.missing_deps.join(", ")),
            format!(
                "  ↳ missing required deps: {}",
                report.missing_deps.join(", ")
            ),
        );
        printer.print_section(&deps);
    }
    for ev in &cap.side.evidence {
        printer.print_section(&format!("  ↳ [{}] {}", ev.source, ev.detail));
    }
    if cap.side.server == SideSupport::Unknown {
        printer.print_section(text(
            lang,
            "  ↳ 建议：装入 mods/ 后启动一次服务端确认",
            "  ↳ suggestion: place in mods/ and start the server once to verify",
        ));
    }
}

/// 文件 basename（无 modid 时兜底显示）
fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// MC 区间展示
///
/// 按**目标加载器**取值：同一个 jar 可以有多份清单各写各的区间（实测
/// `Cell-Forge-1.19.2.jar` 的 Fabric 清单写 `~1.20`、Forge 清单写
/// `[1.19.2]`）。表里给的是「装上这台服务端会按哪个区间校验」，与判定同源，
/// 否则会出现「表里说不符、结论说可装」。
fn range_text(cap: &ModCapability, loader: Option<ModLoader>, lang: Lang) -> String {
    let r = cap.mc_range_for(loader);
    if r.is_any() {
        return text(lang, "未声明", "undeclared").to_owned();
    }
    match (&r.min, &r.max) {
        (Some(min), Some(max)) => format!(">={min} <{max}"),
        (Some(min), None) => format!(">={min}"),
        (None, Some(max)) => format!("<{max}"),
        (None, None) => r.raw.clone(),
    }
}

/// 服务端支持度展示
fn side_text(side: SideSupport, lang: Lang) -> &'static str {
    match side {
        SideSupport::Required => text(lang, "✓ 必需", "✓ required"),
        SideSupport::Optional => text(lang, "✓ 可选", "✓ optional"),
        SideSupport::Unsupported => text(lang, "✗ 不支持", "✗ unsupported"),
        // `Unknown` 与未来新增档位都按「判不出」展示：不会把未知状态渲染成
        // 「必需」或「不支持」。
        _ => text(lang, "? 判不出", "? unknown"),
    }
}

/// 风险展示
fn risk_text(risk: Risk, lang: Lang) -> &'static str {
    match risk {
        Risk::Ok => text(lang, "ok", "ok"),
        Risk::Review => text(lang, "review", "review"),
        Risk::Blocking => text(lang, "blocking", "blocking"),
    }
}

/// 匹配结论展示
fn fit_text(fit: Option<&ServerFit>, lang: Lang) -> &'static str {
    match fit.map(|f| f.verdict) {
        Some(FitVerdict::Installable) => text(lang, "✓ 可装", "✓ ok"),
        Some(FitVerdict::Unclear) => text(lang, "? 待确认", "? unclear"),
        Some(FitVerdict::Rejected) => text(lang, "✗ 不匹配", "✗ mismatch"),
        None => "-",
    }
}

/// 置信度转机器名（JSON 契约用稳定英文，不做翻译）
fn confidence_name(c: Confidence) -> String {
    c.as_str().to_owned()
}

fn to_json(report: &ModReport, target_loader: Option<ModLoader>, target: &ServerTarget) -> ModJson {
    let cap = &report.capability;
    ModJson {
        path: cap.path.clone(),
        id: cap.id.clone(),
        version: cap.version.clone(),
        loaders: cap.loaders.iter().map(|l| l.as_str().to_owned()).collect(),
        mc_versions: {
            // 与判定同源：JSON 消费方拿到的区间必须就是用来判「装不装得上」
            // 的那一份，否则会与同一份输出里的 `fit` 自相矛盾
            let r = cap.mc_range_for(target_loader);
            McRangeJson {
                raw: r.raw.clone(),
                min: r.min.clone(),
                max: r.max.clone(),
            }
        },
        java_required: cap.java_required,
        java_compiled: cap.java_compiled,
        client: cap.side.client.as_str().to_owned(),
        server: cap.side.server.as_str().to_owned(),
        confidence: confidence_name(cap.side.confidence),
        risk: report.risk.as_str().to_owned(),
        missing_deps: report.missing_deps.clone(),
        // 目标加载器是整批共享的口径，单条记录也带上，JSON 消费方不必另取
        target_loader: target_loader.map(|loader| loader.as_str().to_owned()),
        fit: report.fit.as_ref().map(fit_json),
        target: target_json(target),
        evidence: cap
            .side
            .evidence
            .iter()
            .map(|e| EvidenceJson {
                source: e.source.as_str().to_owned(),
                detail: e.detail.clone(),
            })
            .collect(),
    }
}
