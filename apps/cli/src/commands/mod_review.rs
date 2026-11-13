//! 安装后复核：把「会让服务端起不来」的 mod 列出来，让用户选择停用哪些
//!
//! 整合包装完不等于能开服。清单的 `env` 与上游的 `server_side` 都是**声明**，
//! 声明与实际不符是常态：作者把仅客户端模组标成 `server: required`、上游
//! 标注写错、包里混进别的加载器的 jar、依赖没有一并打包。这些在装完那一刻
//! 全都查得出来，等到 `server start` 再拦，用户已经以为装好了。
//!
//! 判定不在这里：`service` 的安装复核与启动预检共用同一份扫描，本模块只负责
//! 把结论摆成一张表、收一个选择、把选中的文件移进停用目录。
//!
//! # 为什么是「停用」而不是「删除」
//!
//! 判定是工具的推断，不是用户的决定。实测 `side="CLIENT"` 的 `CreativeCore`、
//! 依赖全写 `side="CLIENT"` 的 `embeddiumplus` 都能在 dedicated server 上正常
//! 装载。直接删掉就把用户的文件连同「判错了怎么办」一起消灭了。移进
//! [`mcsup::service::DISABLED_DIR`] 后原名不变，用户随时能搬回来。

use anyhow::Result;
use mcsup::backend::remote::RemoteModBlocked;
use mcsup::domain::server::content::ContentKind;
use mcsup::port::CancellationToken;
use mcsup::service::{ApplyOptions, ModBlocked, ModBlocker};
use std::path::Path;

use crate::i18n::pick;
use crate::locale::Lang;
use crate::output::{Printer, compact_table};
use crate::progress::TaskSpinner;
use crate::prompts::input_text;

/// 表格的一行：文件名 + 一句话原因
///
/// 本地与远端的判定来源不同（`ModBlocker` vs 线格式的 `kind`/`detail`），
/// 渲染成行之后就一样了——表格、序号、输入框只认识这一种形状。
struct Row {
    file: String,
    reason: String,
}

/// 复核结果：分两阶段处置
///
/// `name` 是服务端名，停用要经 `ModService`（它负责「运行中不许动文件」与
/// 路径越界检查），因此这里不直接操作文件系统。
///
/// 阶段一：只有「仅客户端」与「不匹配」进停用表并提问；「缺依赖」条目不
/// 进停用表——它们靠补装解决，停用是错的动作。
/// 阶段二：停用之后，缺依赖那批自动补装（见 [`auto_install_missing_deps`]）。
///
/// # Errors
///
/// 停用失败（服务端在跑、文件已被移走、目录不可写）或补装失败时返回错误
pub(crate) async fn review_local(
    name: &str,
    blocked: &[ModBlocked],
    printer: Printer,
) -> Result<()> {
    if blocked.is_empty() {
        return Ok(());
    }
    let lang = printer.lang();
    let (disableable, missing) = partition(blocked);

    // 阶段一：停用。有可停用条目才摆表、才问；缺依赖那批完全不出现在这里。
    if !disableable.is_empty() {
        let rows: Vec<Row> = disableable
            .iter()
            .map(|entry| Row {
                file: file_name(&entry.file),
                reason: reason_text(&entry.blocker, lang),
            })
            .collect();
        print_table(&rows, lang, printer);

        if printer.is_interactive() {
            let picked = ask_selection(disableable.len(), lang, printer)?;
            if picked.is_empty() {
                printer.print_outcome(
                    &pick(
                        lang,
                        "未停用任何文件；启动时仍会被预检拦下".to_owned(),
                        "nothing disabled; the start pre-check will still block".to_owned(),
                    ),
                    &serde_json::json!({ "disabled": 0 }),
                )?;
            } else {
                let files: Vec<String> = picked
                    .iter()
                    .map(|&i| disableable[i].file.clone())
                    .collect();
                let mc = crate::bootstrap::open_mcsup()?;
                let done = mc.mods.disable(name, &files)?;
                let moved: Vec<&str> = done.moved.iter().map(|(_, to)| to.as_str()).collect();
                printer.print_outcome(
                    &pick(
                        lang,
                        format!(
                            "已停用 {} 个 mod，移到 {}：{}",
                            done.moved.len(),
                            mcsup::service::DISABLED_DIR,
                            moved.join("、")
                        ),
                        format!(
                            "disabled {} mod(s) into {}: {}",
                            done.moved.len(),
                            mcsup::service::DISABLED_DIR,
                            moved.join(", ")
                        ),
                    ),
                    &serde_json::json!({
                        "disabled": done.moved.len(),
                        "dir": mcsup::service::DISABLED_DIR,
                        "files": moved,
                    }),
                )?;
            }
        } else {
            // 非交互（`--output json` / 管道）不能提问：把结论和下一步说清楚，
            // 副作用（停用 / 补装都写文件系统）留给用户显式发起，不在脚本路径
            // 偷偷落盘
            printer.print_warning(&pick(
                lang,
                format!(
                    "以上 {} 个 mod 会让服务端起不来；用 mcsup server mod disable 停用",
                    disableable.len()
                ),
                format!(
                    "{} mod(s) above would stop the server; run mcsup server mod disable",
                    disableable.len()
                ),
            ));
        }
    }

    // 阶段二：停用之后自动补装缺失依赖。没缺依赖就没有这一步。
    if missing.is_empty() {
        return Ok(());
    }
    if !printer.is_interactive() {
        printer.print_warning(&pick(
            lang,
            format!(
                "另有 {} 个 mod 缺少依赖；用 mcsup server mod fix 补装",
                missing.len()
            ),
            format!(
                "{} more mod(s) have missing dependencies; run mcsup server mod fix",
                missing.len()
            ),
        ));
        return Ok(());
    }
    auto_install_missing_deps(name, printer).await
}

/// 把阻断项分成两个处置阶段
///
/// 返回 `(可停用, 缺依赖)`：缺依赖靠补装解决、不靠停用，因此不进停用表。
/// `可停用` 是「仅客户端」或「与服务端不匹配」，只能停用或换版本。
fn partition(blocked: &[ModBlocked]) -> (Vec<&ModBlocked>, Vec<&ModBlocked>) {
    blocked
        .iter()
        .partition(|entry| !matches!(&entry.blocker, ModBlocker::MissingDeps { .. }))
}

/// 停用之后自动补装缺失依赖：先计划（零写入）再落盘，不询问
///
/// 复用 `server mod fix` 的判定（[`mcsup::service::ModService::plan_fix`]），
/// 但只取「补装」那一半：计划里的 `removals` 针对「仅客户端 / 不匹配」，那批
/// 已经在停用阶段由用户决定过了，这里清空，绝不替用户把他们选择保留的文件
/// 移走。补不上的进 `unresolved` 报告，不擅自移除。
async fn auto_install_missing_deps(name: &str, printer: Printer) -> Result<()> {
    let lang = printer.lang();
    let mc = crate::bootstrap::open_mcsup()?;

    let spinner = TaskSpinner::new(
        pick(lang, "正在分析缺失依赖…", "analyzing missing dependencies…"),
        printer.is_human(),
    );
    let mut plan = mc.mods.plan_fix(name, ContentKind::Mod).await?;
    spinner.finish();
    plan.removals.clear();

    if plan.installs.is_empty() {
        printer.print_outcome(
            &pick(
                lang,
                "缺失依赖没有找到可自动补装的提供方，需人工处置".to_owned(),
                "missing dependencies have no auto-installable provider".to_owned(),
            ),
            &serde_json::json!({ "installed": 0, "unresolved": plan.unresolved.len() }),
        )?;
        return Ok(());
    }

    let apply_spinner = TaskSpinner::new(
        pick(
            lang,
            "正在补装缺失依赖…",
            "installing missing dependencies…",
        ),
        printer.is_human(),
    );
    let applied = mc
        .mods
        .apply_fix(
            name,
            &plan,
            ApplyOptions {
                allow_blocking: false,
                progress: None,
                cancel: &CancellationToken::new(),
            },
        )
        .await;
    apply_spinner.finish();
    super::server_mod::report_fix_applied(&applied?, printer)
}

/// 复核结果：文件在远端，只能报告
///
/// 停用要动对端的文件系统，而这条路径上客户端没有对端的执行权（`server
/// create` 的远端分支只回统计量）。如实说明「在哪儿停用」，不假装已经处理。
pub(crate) fn report_remote(blocked: &[RemoteModBlocked], origin: &str, printer: Printer) {
    if blocked.is_empty() {
        return;
    }
    let lang = printer.lang();
    let rows: Vec<Row> = blocked
        .iter()
        .map(|entry| Row {
            file: file_name(&entry.file),
            reason: remote_reason_text(entry, lang),
        })
        .collect();
    print_table(&rows, lang, printer);
    printer.print_warning(&pick(
        lang,
        format!(
            "以上 {} 个 mod 会让 {origin} 上的服务端起不来；在 {origin} 上执行 mcsup server mod disable 停用",
            blocked.len()
        ),
        format!(
            "{} mod(s) above would stop the server on {origin}; run mcsup server mod disable there",
            blocked.len()
        ),
    ));
}

/// 无边框表格：序号 / 文件 / 原因
///
/// 序号是给下面那个输入框用的，因此固定右对齐、不换行；文件名与原因按终端
/// 宽度自动折行。不画边框：这张表紧接在提问之前，边框只会挤占宽度。
fn print_table(rows: &[Row], lang: Lang, printer: Printer) {
    let mut table = compact_table();
    table.set_header([
        pick(lang, "#", "#").clone(),
        pick(lang, "文件", "file").clone(),
        pick(lang, "原因", "reason").clone(),
    ]);
    for (index, row) in rows.iter().enumerate() {
        table.add_row([
            (index + 1).to_string(),
            row.file.clone(),
            row.reason.clone(),
        ]);
    }
    printer.print_table(&table);
}

/// 管线格式的阻断项渲染成一句话
///
/// `kind` 是 agent 侧 `ModBlocker` 的稳定机器名，这里按名渲染；对不上（
/// 新 agent 旧客户端）时退回显示 `kind` 本身，总比空白好。
fn remote_reason_text(entry: &RemoteModBlocked, lang: Lang) -> String {
    match entry.kind.as_str() {
        "missing_deps" => pick(
            lang,
            format!("缺少依赖 {}（可 mod fix 补装）", entry.detail.join("、")),
            format!(
                "missing {} (mod fix can install it)",
                entry.detail.join(", ")
            ),
        ),
        "client_only" => pick(
            lang,
            "仅客户端，服务端无法装载".to_owned(),
            "client-only, cannot load on a server".to_owned(),
        ),
        "mismatch" => pick(
            lang,
            format!(
                "{}（可停用或换适配版本）",
                entry.detail.first().unwrap_or(&entry.kind)
            ),
            format!(
                "{} (disable it or switch version)",
                entry.detail.first().unwrap_or(&entry.kind)
            ),
        ),
        _ => entry.kind.clone(),
    }
}

/// 只显示文件名：`mods/` 前缀对每一行都一样，占宽度不提供信息
pub(crate) fn file_name(path: &str) -> String {
    Path::new(path).file_name().map_or_else(
        || path.to_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// 一句话说明「为什么不支持」
///
/// 处置建议跟在原因后面而不是单开一列：缺依赖能补装、加载器不匹配只能换，
/// 两者的下一步不同，用户看完原因就该知道该按哪个键。
fn reason_text(blocker: &ModBlocker, lang: Lang) -> String {
    match blocker {
        ModBlocker::MissingDeps { mods } => pick(
            lang,
            format!("缺少依赖 {}（可 mod fix 补装）", mods.join("、")),
            format!("missing {} (mod fix can install it)", mods.join(", ")),
        ),
        ModBlocker::ClientOnly => pick(
            lang,
            "仅客户端，服务端无法装载".to_owned(),
            "client-only, cannot load on a server".to_owned(),
        ),
        ModBlocker::Mismatch { detail } => pick(
            lang,
            format!("{detail}（可停用或换适配版本）"),
            format!("{detail} (disable it or switch version)"),
        ),
    }
}

/// 收一个选择：逗号分隔序号 / `A` 全选 / `N` 不选
///
/// 输入非法时说明哪里不对再问一次，不把用户踢回上一步——他刚看完表，重来
/// 一遍要重新装包。
fn ask_selection(len: usize, lang: Lang, printer: Printer) -> Result<Vec<usize>> {
    let help = pick(
        lang,
        "逗号分隔序号，A 全选，N 不选",
        "comma-separated numbers, A for all, N for none",
    );
    loop {
        let raw = input_text(lang, &pick(lang, "停用哪些", "disable which"), "N", &help)?;
        match parse_selection(&raw, len) {
            Ok(picked) => return Ok(picked),
            Err(err) => {
                // 说清楚错在哪：只说「输入无效」用户还得自己猜是序号越界
                // 还是分隔符写错
                printer.print_warning(&err.message(len, lang));
            }
        }
    }
}

/// 选择输入的错误
#[derive(Debug, Clone, PartialEq, Eq)]
enum SelectionError {
    /// 不是数字，也不是 A / N
    NotANumber(String),
    /// 序号越界（1 起算）
    OutOfRange(usize),
    /// 只有分隔符，没有任何序号
    Empty,
}

impl SelectionError {
    /// 面向用户的一句话
    fn message(&self, len: usize, lang: Lang) -> String {
        match self {
            Self::NotANumber(token) => pick(
                lang,
                format!("「{token}」不是序号；请输入 1-{len} 之间的数字，或 A / N"),
                format!("`{token}` is not a number; enter 1-{len}, or A / N"),
            ),
            Self::OutOfRange(value) => pick(
                lang,
                format!("序号 {value} 超出范围；本表只有 1-{len}"),
                format!("{value} is out of range; this table has 1-{len}"),
            ),
            Self::Empty => pick(
                lang,
                "没有读到任何序号；请输入 1-3 这样的列表，或 A / N".to_owned(),
                "no number found; enter a list like 1,3, or A / N".to_owned(),
            ),
        }
    }
}

/// 解析选择输入，返回 0 起的下标（去重、保持输入顺序）
///
/// 接受 `,` `，` 与空白作分隔符：中文输入法下逗号常是全角，为此让用户重输
/// 一遍没有意义。
fn parse_selection(input: &str, len: usize) -> Result<Vec<usize>, SelectionError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let lowered = trimmed.to_ascii_lowercase();
    if matches!(lowered.as_str(), "a" | "all" | "全部") {
        return Ok((0..len).collect());
    }
    if matches!(lowered.as_str(), "n" | "none" | "无" | "不") {
        return Ok(Vec::new());
    }

    let mut picked: Vec<usize> = Vec::new();
    let mut saw_token = false;
    for token in trimmed.split([',', '，', ' ', '\t']) {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        saw_token = true;
        let Ok(value) = token.parse::<usize>() else {
            return Err(SelectionError::NotANumber(token.to_owned()));
        };
        if value == 0 || value > len {
            return Err(SelectionError::OutOfRange(value));
        }
        let index = value - 1;
        if !picked.contains(&index) {
            picked.push(index);
        }
    }
    if !saw_token {
        return Err(SelectionError::Empty);
    }
    Ok(picked)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_comma_separated_numbers() {
        assert_eq!(parse_selection("1,3", 4), Ok(vec![0, 2]));
        assert_eq!(parse_selection(" 2 , 4 ", 4), Ok(vec![1, 3]));
    }

    #[test]
    fn accepts_full_width_comma() {
        // 中文输入法下逗号常是全角，为此让用户重输没有意义
        assert_eq!(parse_selection("1，2", 3), Ok(vec![0, 1]));
    }

    #[test]
    fn deduplicates_keeping_input_order() {
        assert_eq!(parse_selection("3,1,3", 3), Ok(vec![2, 0]));
    }

    #[test]
    fn all_and_none_keywords() {
        assert_eq!(parse_selection("A", 3), Ok(vec![0, 1, 2]));
        assert_eq!(parse_selection("all", 3), Ok(vec![0, 1, 2]));
        assert_eq!(parse_selection("N", 3), Ok(Vec::new()));
        assert_eq!(parse_selection("none", 3), Ok(Vec::new()));
        assert_eq!(parse_selection("", 3), Ok(Vec::new()));
    }

    #[test]
    fn rejects_out_of_range() {
        assert_eq!(parse_selection("5", 3), Err(SelectionError::OutOfRange(5)));
        assert_eq!(parse_selection("0", 3), Err(SelectionError::OutOfRange(0)));
    }

    #[test]
    fn rejects_non_numbers() {
        assert_eq!(
            parse_selection("1,x", 3),
            Err(SelectionError::NotANumber("x".to_owned()))
        );
    }

    #[test]
    fn rejects_separator_only() {
        assert_eq!(parse_selection(",,", 3), Err(SelectionError::Empty));
    }

    #[test]
    fn partition_separates_missing_deps_from_disableable() {
        let blocked = vec![
            ModBlocked {
                file: "mods/a.jar".to_owned(),
                blocker: ModBlocker::MissingDeps {
                    mods: vec!["lib".to_owned()],
                },
            },
            ModBlocked {
                file: "mods/b.jar".to_owned(),
                blocker: ModBlocker::ClientOnly,
            },
            ModBlocked {
                file: "mods/c.jar".to_owned(),
                blocker: ModBlocker::Mismatch {
                    detail: "loader".to_owned(),
                },
            },
            ModBlocked {
                file: "mods/d.jar".to_owned(),
                blocker: ModBlocker::MissingDeps {
                    mods: vec!["other".to_owned()],
                },
            },
        ];
        let (disableable, missing) = partition(&blocked);
        assert_eq!(disableable.len(), 2);
        assert_eq!(missing.len(), 2);
        assert!(
            disableable
                .iter()
                .all(|e| !matches!(&e.blocker, ModBlocker::MissingDeps { .. }))
        );
        assert!(
            missing
                .iter()
                .all(|e| matches!(&e.blocker, ModBlocker::MissingDeps { .. }))
        );
    }
}
