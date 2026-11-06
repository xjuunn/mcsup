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
use comfy_table::{ContentArrangement, Table};
use mcsup::domain::mod_::{Confidence, ModCapability, ModLoader, Risk, SideSupport};
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
    evidence: Vec<EvidenceJson>,
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
        } => inspect(paths, offline, loader.as_deref(), printer).await,
    }
}

async fn inspect(
    paths: Vec<PathBuf>,
    offline: bool,
    loader: Option<&str>,
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
    let target = match loader {
        Some(raw) => {
            Some(ModLoader::parse(raw).ok_or_else(|| unknown_loader_error(printer.lang(), raw))?)
        }
        None => None,
    };
    let service = inspect_service(offline, printer.lang())?;
    let audit = service.inspect_paths_for(&jars, !offline, target).await;
    spinner.finish();
    let audit = audit?;

    render(&audit, printer)
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
fn render(audit: &ModAudit, printer: Printer) -> Result<()> {
    let lang = printer.lang();
    let rows: Vec<ModJson> = audit
        .mods
        .iter()
        .map(|report| to_json(report, audit.target_loader))
        .collect();
    printer.print_rows(
        &rows,
        text(lang, "没有可报告的 mod", "nothing to report"),
        || build_table(audit, lang),
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

    // 有问题的条目逐条给理由：blocking 要说清阻塞在哪，review 要说清为什么判不出
    for report in audit.mods.iter().filter(|r| r.risk != Risk::Ok) {
        print_reason_detail(report, printer);
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

/// 表格：只放结论，不放长理由
fn build_table(audit: &ModAudit, lang: Lang) -> Table {
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header([
        text(lang, "mod", "mod"),
        text(lang, "加载器", "loader"),
        text(lang, "MC 区间", "MC range"),
        text(lang, "Java", "Java"),
        text(lang, "服务端", "server"),
        text(lang, "风险", "risk"),
    ]);
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
        table.add_row([
            name.to_owned(),
            loaders,
            range_text(cap, lang),
            java,
            side_text(cap.side.server, lang).to_owned(),
            risk_text(report.risk, lang).to_owned(),
        ]);
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
fn range_text(cap: &ModCapability, lang: Lang) -> String {
    let r = &cap.mc_versions;
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
        SideSupport::Unknown => text(lang, "? 判不出", "? unknown"),
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

/// 置信度转机器名（JSON 契约用稳定英文，不做翻译）
fn confidence_name(c: Confidence) -> String {
    c.as_str().to_owned()
}

fn to_json(report: &ModReport, target_loader: Option<ModLoader>) -> ModJson {
    let cap = &report.capability;
    ModJson {
        path: cap.path.clone(),
        id: cap.id.clone(),
        version: cap.version.clone(),
        loaders: cap.loaders.iter().map(|l| l.as_str().to_owned()).collect(),
        mc_versions: McRangeJson {
            raw: cap.mc_versions.raw.clone(),
            min: cap.mc_versions.min.clone(),
            max: cap.mc_versions.max.clone(),
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
