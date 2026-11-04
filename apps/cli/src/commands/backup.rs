//! `server backup` 子命令：create / list / restore / delete / verify / prune
//!
//! 只做参数解析、交互补齐与结果渲染；快照编排（发 save-off/save-on、扫描、
//! CAS、恢复）在 `service::BackupService`，存储与领域判定在 `backup` / `domain`。
//!
//! 参数缺失时在 TTY 下交互补齐（label、快照 id、恢复目标），非交互缺失即
//! 用法错误（2）或在语义上取缺省（全量备份、全部范围）。

use crate::bootstrap;
use crate::diagnostic::AppDiagnostic;
use crate::output::{self, Printer};
use crate::progress::TaskSpinner;
use crate::prompts;
use anyhow::{Context, Result};
use comfy_table::{ContentArrangement, Table};
use mcsup::domain::backup::{
    BackupScope, ContentHash, Page, RestoreTarget, RetentionInput, Snapshot, SnapshotId, parse_age,
};
use mcsup::domain::server::meta::ServerName;
use mcsup::domain::server::settings::parse_backup_scope;
use mcsup::protocol::cli::{
    BackupCreateJson, BackupDeleteJson, BackupListJson, BackupPruneJson, BackupRestoreJson,
    BackupRow, BackupVerifyJson,
};
use mcsup::service::{BackupService, CreateOptions};

/// 装配备份服务：经库侧组合根统一装配（注册表 + supervisor）
fn backup_service() -> Result<BackupService> {
    // 带远端装配：备份按来源路由（本地直写、远端走 RPC）
    let mcsup = bootstrap::open_mcsup_with_remotes()?;
    Ok(mcsup.backup.clone())
}

/// `backup create`：默认全量；`--scope` 覆盖服务属性范围
///
/// # Errors
///
/// 服务器不存在、范围为空、CAS / 扫描 / 记录写入失败时返回错误
pub(crate) async fn create(
    name: &str,
    label: Option<&str>,
    scope: Option<&str>,
    no_reuse: bool,
    printer: Printer,
) -> Result<()> {
    let service = backup_service()?;
    let scope = match scope {
        Some(raw) => Some(parse_scope(printer.lang(), raw)?),
        None => None,
    };
    // 标签缺省且交互时询问（允许留空 = 无标签）
    let label = match label {
        Some(raw) => Some(raw.to_owned()),
        None if printer.is_interactive() => Some(prompts::input_text(
            printer.lang(),
            &crate::i18n::pick(
                printer.lang(),
                "快照标签（可留空）",
                "snapshot label (optional)",
            ),
            "",
            &crate::i18n::pick(
                printer.lang(),
                "如 before-upgrade；回车留空",
                "e.g. before-upgrade; enter for none",
            ),
        )?),
        None => None,
    };

    let options = CreateOptions {
        label,
        scope,
        no_reuse,
        flush_wait_ms: None,
    };
    let spinner = TaskSpinner::new(
        crate::i18n::pick(
            printer.lang(),
            format!("正在备份 {name}…"),
            format!("backing up {name}…"),
        ),
        printer.is_human(),
    );
    let owned = name.to_owned();
    // 令牌来自 spinner：Ctrl-C 已在 `TaskSpinner` 里接管，扫描随即中止
    let cancel = spinner.cancel().clone();
    let result = tokio::task::spawn_blocking(move || service.create(&owned, &options, &cancel))
        .await
        .context(crate::i18n::pick(printer.lang(), "备份任务", "backup task"))?;
    spinner.finish();
    let outcome = result?;

    let row = row_of(&outcome.snapshot);
    printer.print_outcome(
        &crate::i18n::pick(
            printer.lang(),
            format!(
                "已创建备份 {}（{} 个文件，新增 {}，复用 {}）",
                outcome.snapshot.id.as_str(),
                outcome.snapshot.content.len(),
                outcome.snapshot.added,
                outcome.snapshot.reused
            ),
            format!(
                "created backup {} ({} files, {} added, {} reused)",
                outcome.snapshot.id.as_str(),
                outcome.snapshot.content.len(),
                outcome.snapshot.added,
                outcome.snapshot.reused
            ),
        ),
        &BackupCreateJson {
            snapshot: row,
            skipped: outcome.skipped,
        },
    )?;
    Ok(())
}

/// `backup list`：分页的历史表格（最新在前）
///
/// # Errors
///
/// 服务器不存在或快照索引读取失败时返回错误
pub(crate) fn list(name: &str, page: usize, page_size: usize, printer: Printer) -> Result<()> {
    let service = backup_service()?;
    let page = Page::new(page.max(1), page_size.max(1));
    let window = service.list(name, page)?;

    let rows: Vec<BackupRow<'_>> = window.items.iter().map(row_of).collect();
    let payload = BackupListJson {
        items: rows,
        total: window.total,
        page: window.page,
        page_size: window.page_size,
        pages: window.pages(),
    };

    if printer.is_human() {
        if window.items.is_empty() {
            printer.print_section(&crate::i18n::pick(
                printer.lang(),
                format!("{name} 暂无备份"),
                format!("{name} has no backups"),
            ));
            return Ok(());
        }
        let mut table = Table::new();
        table.set_content_arrangement(ContentArrangement::Dynamic);
        table.set_header([
            "ID",
            crate::i18n::text(printer.lang(), "时间", "time"),
            crate::i18n::text(printer.lang(), "状态", "status"),
            crate::i18n::text(printer.lang(), "文件", "files"),
            crate::i18n::text(printer.lang(), "大小", "size"),
            crate::i18n::text(printer.lang(), "标签", "label"),
        ]);
        for snapshot in &window.items {
            table.add_row([
                snapshot.id.as_str(),
                &utc_display(snapshot.created_at),
                snapshot.status.as_str(),
                &snapshot.content.len().to_string(),
                &output::human_bytes(snapshot.total_bytes),
                snapshot.label.as_deref().unwrap_or("-"),
            ]);
        }
        output::print_table(&table);
        printer.print_section(&crate::i18n::pick(
            printer.lang(),
            format!(
                "第 {}/{} 页，共 {} 份备份",
                payload.page,
                payload.pages.max(1),
                payload.total
            ),
            format!(
                "page {}/{} of {} backups",
                payload.page,
                payload.pages.max(1),
                payload.total
            ),
        ));
        return Ok(());
    }
    let line = serde_json::to_string(&payload)?;
    output::print_json_line(&line);
    Ok(())
}

/// `backup restore`：缺 id 交互选择；`--to` 恢复到新服务，缺省原地
///
/// # Errors
///
/// 快照不存在 / 不可恢复、原地恢复时服务器在运行、目标服务名已占用、写盘
/// 失败时返回错误
pub(crate) async fn restore(
    name: &str,
    id: Option<&str>,
    to: Option<&str>,
    printer: Printer,
) -> Result<()> {
    let service = backup_service()?;
    let id = match id {
        Some(raw) => raw.to_owned(),
        None if printer.is_interactive() => prompt_snapshot(&service, name, printer)?,
        None => {
            return Err(AppDiagnostic::usage(
                crate::i18n::pick(
                    printer.lang(),
                    "server backup restore 缺少快照 id",
                    "server backup restore requires a snapshot id",
                ),
                Some(crate::i18n::pick(
                    printer.lang(),
                    "非交互模式：mcsup server backup restore <name> <id>；可用 ls 查看",
                    "non-interactive: mcsup server backup restore <name> <id>; see ls",
                )),
            )
            .into());
        }
    };
    let target = restore_target(printer.lang(), to)?;

    let spinner = TaskSpinner::new(
        crate::i18n::pick(
            printer.lang(),
            format!("正在恢复 {name}…"),
            format!("restoring {name}…"),
        ),
        printer.is_human(),
    );
    let owned = name.to_owned();
    let cancel = spinner.cancel().clone();
    let result =
        tokio::task::spawn_blocking(move || service.restore(&owned, &id, &target, &cancel))
            .await
            .context(crate::i18n::pick(
                printer.lang(),
                "恢复任务",
                "restore task",
            ))?;
    spinner.finish();
    let outcome = result?;

    printer.print_outcome(
        &crate::i18n::pick(
            printer.lang(),
            format!(
                "已从 {} 恢复到 {}（{} 个文件，{}）",
                outcome.snapshot.id.as_str(),
                outcome.destination.display(),
                outcome.files,
                output::human_bytes(outcome.bytes)
            ),
            format!(
                "restored {} into {} ({} files, {})",
                outcome.snapshot.id.as_str(),
                outcome.destination.display(),
                outcome.files,
                output::human_bytes(outcome.bytes)
            ),
        ),
        &BackupRestoreJson {
            snapshot: outcome.snapshot.id.as_str(),
            destination: outcome.destination.display().to_string(),
            files: outcome.files,
            bytes: outcome.bytes,
            safety_snapshot: outcome.safety_snapshot.as_ref().map(SnapshotId::as_str),
            deleted: outcome.deleted,
        },
    )?;
    if let Some(safety) = outcome.safety_snapshot.as_ref() {
        printer.print_section(&crate::i18n::pick(
            printer.lang(),
            format!(
                "恢复前的现场已备份为 {}，如需回退可 mcsup server backup restore {name} {}",
                safety.as_str(),
                safety.as_str()
            ),
            format!(
                "the pre-restore state was backed up as {}; to roll back run mcsup server backup restore {name} {}",
                safety.as_str(),
                safety.as_str()
            ),
        ));
    }
    Ok(())
}

/// 解析 `backup restore --to` 的目标
///
/// 名字要拼进 `servers/<name>`，入口先按领域规则校验：非法名字是用法错误
/// （2），不是运行期失败。判定本身在 [`ServerName::parse`]，这里只负责把
/// 失败映射成入口的错误分类。
///
/// # Errors
///
/// `--to` 不是合法服务器名时返回用法错误
fn restore_target(lang: crate::locale::Lang, to: Option<&str>) -> Result<RestoreTarget> {
    let Some(new_name) = to else {
        return Ok(RestoreTarget::InPlace);
    };
    let name = ServerName::parse(new_name).map_err(|_| {
        AppDiagnostic::usage(
            crate::i18n::pick(
                lang,
                format!("非法的服务器名：{new_name}"),
                format!("invalid server name: {new_name}"),
            ),
            Some(crate::i18n::pick(
                lang,
                "只允许 A-Za-z0-9_-，长度 1-64",
                "only A-Za-z0-9_- allowed, 1-64 characters",
            )),
        )
    })?;
    Ok(RestoreTarget::NewServer { name })
}

/// `backup delete`：缺 id 交互选择
///
/// # Errors
///
/// 快照不存在或索引写入失败时返回错误
pub(crate) fn delete(name: &str, id: Option<&str>, printer: Printer) -> Result<()> {
    let service = backup_service()?;
    let id = match id {
        Some(raw) => raw.to_owned(),
        None if printer.is_interactive() => prompt_snapshot(&service, name, printer)?,
        None => {
            return Err(AppDiagnostic::usage(
                crate::i18n::pick(
                    printer.lang(),
                    "server backup delete 缺少快照 id",
                    "server backup delete requires a snapshot id",
                ),
                Some(crate::i18n::pick(
                    printer.lang(),
                    "非交互模式：mcsup server backup delete <name> <id>；可用 ls 查看",
                    "non-interactive: mcsup server backup delete <name> <id>; see ls",
                )),
            )
            .into());
        }
    };
    let outcome = service.delete(name, &id)?;
    printer.print_outcome(
        &crate::i18n::pick(
            printer.lang(),
            format!(
                "已删除备份 {}（释放 {} 个对象，可执行 prune 回收）",
                outcome.snapshot.id.as_str(),
                outcome.orphaned
            ),
            format!(
                "deleted backup {} ({} objects orphaned; run prune to reclaim)",
                outcome.snapshot.id.as_str(),
                outcome.orphaned
            ),
        ),
        &BackupDeleteJson {
            id: outcome.snapshot.id.as_str(),
            orphaned: outcome.orphaned,
        },
    )?;
    Ok(())
}

/// `backup verify`：校验完整性；有损坏时返回错误（退出码 1）
///
/// # Errors
///
/// 快照不存在；或校验发现损坏 / 缺失对象（[`mcsup::service::BackupError::VerifyFailed`]）
pub(crate) fn verify(name: &str, id: Option<&str>, printer: Printer) -> Result<()> {
    let service = backup_service()?;
    let id = match id {
        Some(raw) => raw.to_owned(),
        None if printer.is_interactive() => prompt_snapshot(&service, name, printer)?,
        None => {
            return Err(AppDiagnostic::usage(
                crate::i18n::pick(
                    printer.lang(),
                    "server backup verify 缺少快照 id",
                    "server backup verify requires a snapshot id",
                ),
                Some(crate::i18n::pick(
                    printer.lang(),
                    "非交互模式：mcsup server backup verify <name> <id>；可用 ls 查看",
                    "non-interactive: mcsup server backup verify <name> <id>; see ls",
                )),
            )
            .into());
        }
    };
    let spinner = TaskSpinner::new(
        crate::i18n::pick(
            printer.lang(),
            format!("正在校验 {id}…"),
            format!("verifying {id}…"),
        ),
        printer.is_human(),
    );
    let outcome = service.verify(name, &id, &mut |_, _| {});
    spinner.finish();
    let outcome = outcome?;

    let broken: Vec<&str> = outcome.broken.iter().map(ContentHash::as_str).collect();
    let payload = BackupVerifyJson {
        id: outcome.snapshot.id.as_str(),
        healthy: outcome.is_healthy(),
        objects: outcome.objects,
        bytes: outcome.bytes,
        broken: broken.clone(),
    };
    if printer.is_human() {
        if outcome.is_healthy() {
            printer.print_section(&crate::i18n::pick(
                printer.lang(),
                format!(
                    "{} 完好：{} 个对象，{}",
                    outcome.snapshot.id.as_str(),
                    outcome.objects,
                    output::human_bytes(outcome.bytes)
                ),
                format!(
                    "{} is intact: {} objects, {}",
                    outcome.snapshot.id.as_str(),
                    outcome.objects,
                    output::human_bytes(outcome.bytes)
                ),
            ));
        } else {
            printer.print_warning(&crate::i18n::pick(
                printer.lang(),
                format!(
                    "{} 有 {} 个对象损坏或缺失",
                    outcome.snapshot.id.as_str(),
                    broken.len()
                ),
                format!(
                    "{} has {} broken or missing objects",
                    outcome.snapshot.id.as_str(),
                    broken.len()
                ),
            ));
            for hash in &broken {
                printer.print_section(&format!("  - {hash}"));
            }
        }
    } else {
        let line = serde_json::to_string(&payload)?;
        output::print_json_line(&line);
    }
    // 损坏即失败：脚本据此判断快照是否可用
    if !outcome.is_healthy() {
        return Err(mcsup::service::BackupError::VerifyFailed {
            id: outcome.snapshot.id.as_str().to_owned(),
            broken: outcome.broken,
        }
        .into());
    }
    Ok(())
}

/// `backup prune`：按保留策略清理并回收；`--keep` / `--older-than` 二选一
///
/// # Errors
///
/// 服务器不存在、保留策略非法（同时给出 / 时长非法）、索引读写或对象删除失败
pub(crate) fn prune(
    name: &str,
    keep: Option<usize>,
    older_than: Option<&str>,
    dry_run: bool,
    printer: Printer,
) -> Result<()> {
    let service = backup_service()?;
    let retention = parse_retention(printer.lang(), keep, older_than)?;
    let outcome = service.prune(name, retention, dry_run)?;

    let removed: Vec<&str> = outcome.removed.iter().map(|s| s.id.as_str()).collect();
    let payload = BackupPruneJson {
        removed: removed.clone(),
        objects_reclaimed: outcome.objects_reclaimed,
        bytes_reclaimed: outcome.bytes_reclaimed,
        dry_run: outcome.dry_run,
    };
    if printer.is_human() {
        printer.print_section(&crate::i18n::pick(
            printer.lang(),
            {
                let prefix = if dry_run { "将" } else { "已" };
                format!(
                    "{prefix}删除 {} 份备份，{prefix}回收 {} 个对象（{}）",
                    removed.len(),
                    outcome.objects_reclaimed,
                    output::human_bytes(outcome.bytes_reclaimed)
                )
            },
            {
                let verb = if dry_run { "would delete" } else { "deleted" };
                let reclaim = if dry_run {
                    "would reclaim"
                } else {
                    "reclaimed"
                };
                format!(
                    "{verb} {} backups and {reclaim} {} objects ({})",
                    removed.len(),
                    outcome.objects_reclaimed,
                    output::human_bytes(outcome.bytes_reclaimed)
                )
            },
        ));
        if !removed.is_empty() {
            for id in &removed {
                printer.print_section(&format!("  - {id}"));
            }
        }
        if dry_run {
            printer.print_section(&crate::i18n::pick(
                printer.lang(),
                "这是试运行，未实际修改；去掉 --dry-run 生效",
                "dry run: nothing was changed; drop --dry-run to apply",
            ));
        }
        return Ok(());
    }
    let line = serde_json::to_string(&payload)?;
    output::print_json_line(&line);
    Ok(())
}

// ---- 辅助 ----

/// 快照 → JSON 行
fn row_of(snapshot: &Snapshot) -> BackupRow<'_> {
    BackupRow {
        id: snapshot.id.as_str(),
        server: snapshot.server.as_str(),
        label: snapshot.label.as_deref(),
        parent: snapshot.parent.as_ref().map(SnapshotId::as_str),
        status: snapshot.status.as_str(),
        scope: snapshot.scope.entries().to_vec(),
        files: snapshot.content.len(),
        added: snapshot.added,
        reused: snapshot.reused,
        total_bytes: snapshot.total_bytes,
        added_bytes: snapshot.added_bytes,
        server_online: snapshot.server_online,
        created_at: snapshot.created_at,
    }
}

/// `--scope` 解析：逗号分隔的顶层项
///
/// 转交 `domain::parse_backup_scope`——`server set backup-scope` 与 `--scope`
/// 必须接受同一套写法（含 `all` / `-` / 空表示全部），本层只负责把错误渲染成
/// 用法提示。
fn parse_scope(lang: crate::locale::Lang, raw: &str) -> Result<BackupScope> {
    parse_backup_scope(raw).map_err(|err| {
        AppDiagnostic::usage(
            crate::i18n::pick(
                lang,
                format!("备份范围非法：{raw}"),
                format!("invalid backup scope: {raw}"),
            ),
            Some(crate::i18n::pick(
                lang,
                format!("每个项须是顶层目录 / 文件名（不含路径分隔符）：{err}"),
                format!(
                    "each entry must be a top-level directory or file name (no path separators): {err}"
                ),
            )),
        )
        .into()
    })
}

/// `--keep` / `--older-than` → 保留策略的原始输入
///
/// 只做字面量到秒数的换算（文案需要 `--older-than` 的原文），**两个都给出
/// 的冲突判定交给 `service`**：远端 RPC 拿到的是同一对字段，判定写两处就会
/// 一处报错一处静默择一。
fn parse_retention(
    lang: crate::locale::Lang,
    keep: Option<usize>,
    older_than: Option<&str>,
) -> Result<RetentionInput> {
    let older_than_secs = match older_than {
        Some(raw) => Some(parse_age(raw).map_err(|err| {
            AppDiagnostic::usage(
                crate::i18n::pick(
                    lang,
                    format!("时长非法：{raw}"),
                    format!("invalid age: {raw}"),
                ),
                Some(crate::i18n::pick(
                    lang,
                    format!("用带单位的时长，如 30s / 15m / 12h / 7d / 4w（{err}）"),
                    format!("use a duration with a unit, e.g. 30s / 15m / 12h / 7d / 4w ({err})"),
                )),
            )
        })?),
        None => None,
    };
    Ok(RetentionInput {
        keep,
        older_than_secs,
    })
}

/// 交互选择快照：标签带时间与大小，回映到 id
fn prompt_snapshot(service: &BackupService, name: &str, printer: Printer) -> Result<String> {
    let page = service.list(name, Page::new(1, 100))?;
    if page.items.is_empty() {
        return Err(anyhow::anyhow!(crate::i18n::pick(
            printer.lang(),
            format!("{name} 暂无备份，可先 mcsup server backup create {name}"),
            format!("{name} has no backups; run mcsup server backup create {name} first"),
        )));
    }
    prompts::step(
        printer,
        1,
        1,
        &crate::i18n::pick(printer.lang(), "选择备份", "select a backup"),
    );
    let labels: Vec<String> = page
        .items
        .iter()
        .map(|snapshot| {
            format!(
                "{} · {} · {} · {}",
                snapshot.id.as_str(),
                utc_display(snapshot.created_at),
                snapshot.label.as_deref().unwrap_or("-"),
                output::human_bytes(snapshot.total_bytes)
            )
        })
        .collect();
    let picked = prompts::select_filter(
        printer.lang(),
        &crate::i18n::pick(printer.lang(), "备份", "backup"),
        &labels,
    )?;
    page.items
        .iter()
        .zip(labels.iter())
        .find(|(_, label)| **label == picked)
        .map(|(snapshot, _)| snapshot.id.as_str().to_owned())
        .ok_or_else(|| {
            anyhow::anyhow!(crate::i18n::pick(
                printer.lang(),
                format!("无效备份选择: {picked}"),
                format!("invalid backup choice: {picked}")
            ))
        })
}

/// Unix 秒 → `YYYY-MM-DD HH:MM:SS`（UTC）
#[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
fn utc_display(secs: i64) -> String {
    if secs <= 0 {
        return "-".to_owned();
    }
    let days = secs / 86_400;
    let rest = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}Z",
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    )
}

/// 自 1970-01-01 起的天数 → (年, 月, 日)；Howard Hinnant 的 `civil_from_days`
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (
        year,
        u32::try_from(m).unwrap_or(1),
        u32::try_from(d).unwrap_or(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_scope_accepts_comma_separated_entries() {
        let scope = parse_scope(crate::locale::Lang::Zh, "world, mods").expect("scope");
        assert_eq!(scope.entries(), ["mods".to_owned(), "world".to_owned()]);
    }

    #[test]
    fn parse_scope_rejects_path_separators() {
        let err = parse_scope(crate::locale::Lang::Zh, "world/region").expect_err("invalid");
        assert_eq!(Printer::exit_code(&err), 2);
    }

    /// `--scope all` 与 `server set backup-scope all` 同义
    ///
    /// 回归：CLI 曾自行按逗号切分，`all` 被当成名为 `all` 的顶层项，得到空范围，
    /// 随后以「范围内没有可备份内容」收场——错误信息不指向真正原因。
    #[test]
    fn parse_scope_all_matches_settings_semantics() {
        for raw in ["all", "ALL", "-", "", "  "] {
            let scope = parse_scope(crate::locale::Lang::Zh, raw).expect("all scope");
            assert!(scope.is_all(), "--scope {raw:?} 应表示全部");
        }
    }

    #[test]
    fn parse_retention_maps_each_option() {
        assert_eq!(
            parse_retention(crate::locale::Lang::Zh, Some(3), None).expect("keep"),
            RetentionInput {
                keep: Some(3),
                older_than_secs: None
            }
        );
        assert_eq!(
            parse_retention(crate::locale::Lang::Zh, None, Some("7d")).expect("age"),
            RetentionInput {
                keep: None,
                older_than_secs: Some(7 * 24 * 3600)
            }
        );
        assert_eq!(
            parse_retention(crate::locale::Lang::Zh, None, None).expect("none"),
            RetentionInput::default()
        );
    }

    /// 冲突不在 CLI 判定：两个都给出时照常递交，由 `service` 统一拒绝
    ///
    /// 这条断言的是「CLI 不自行择一」——若这里回退成本地报错，远端 RPC 就会
    /// 再次与本地分叉。
    #[test]
    fn parse_retention_defers_conflict_to_service() {
        let input = parse_retention(crate::locale::Lang::Zh, Some(1), Some("7d")).expect("passes");
        assert_eq!(input.keep, Some(1));
        assert_eq!(input.older_than_secs, Some(7 * 24 * 3600));
        assert!(
            input.resolve().is_err(),
            "冲突必须由 domain 的 resolve 判出"
        );
    }

    #[test]
    fn parse_retention_rejects_age_without_unit() {
        let err = parse_retention(crate::locale::Lang::Zh, None, Some("7")).expect_err("bad age");
        assert_eq!(Printer::exit_code(&err), 2);
    }

    #[test]
    fn utc_display_formats_known_epoch() {
        assert_eq!(utc_display(1_700_000_000), "2023-11-14 22:13:20Z");
        assert_eq!(utc_display(0), "-");
    }
}
