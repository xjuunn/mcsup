use crate::bootstrap;
use crate::cli::WorkspaceAction;
use crate::diagnostic::AppDiagnostic;
use crate::output::Printer;
use crate::prompts::{self, WorkspaceLocation};
use anyhow::{Context, Result};
use comfy_table::Table;
use mcsup::protocol::cli::{
    DoctorFindingJson, DoctorJson, WorkspaceInitRow, WorkspaceRemoveJson, WorkspaceRow,
    WorkspaceUseJson,
};
use mcsup::service::{
    CheckKind, DoctorService, Finding, InitOutcome, ServerRegistry, Severity, WorkspaceService,
    absolute_path,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) fn run(action: WorkspaceAction, printer: Printer) -> Result<()> {
    let store = bootstrap::open_store()?;
    let reg = Arc::new(ServerRegistry::new(store.clone(), store.clone()));
    let svc = WorkspaceService::new(store.clone(), store.clone());
    match action {
        WorkspaceAction::Init { dir, name } => init(&svc, dir.as_deref(), name.as_deref(), printer),
        WorkspaceAction::List => list(&svc, &reg, printer),
        WorkspaceAction::Use { name } => use_ws(&svc, name.as_deref(), printer),
        WorkspaceAction::Remove { name, purge } => remove(&svc, &name, purge, printer),
        WorkspaceAction::Doctor { only, strict } => {
            let doctor = DoctorService::new(store.clone(), store);
            doctor_cmd(&doctor, &only, strict, printer)
        }
    }
}

/// 名称默认值：数据目录模式取序号；有显式目录取目录名，不合法退回序号
fn name_default(
    loc: Option<&WorkspaceLocation>,
    dir_flag: Option<&Path>,
    cwd: &Path,
    seq: &str,
) -> String {
    match loc {
        Some(WorkspaceLocation::DataDir) => seq.to_owned(),
        Some(WorkspaceLocation::CurrentDir) => default_for(cwd, seq),
        Some(WorkspaceLocation::Custom(path)) => default_for(path, seq),
        None => default_for(dir_flag.unwrap_or(cwd), seq),
    }
}

fn default_for(dir: &Path, seq: &str) -> String {
    prompts::default_name_for_dir(dir).unwrap_or_else(|| seq.to_owned())
}

/// 已选位置 + 名称 → 最终目录；数据目录模式必须先有名称
///
/// # Errors
///
/// 数据目录模式缺少工作区名称时返回错误（交互下名称由上游保证已录入）
fn resolve_dir(
    loc: Option<&WorkspaceLocation>,
    dir_flag: Option<&Path>,
    name: Option<&str>,
    cwd: &Path,
    data_root: &Path,
) -> Result<PathBuf> {
    match loc {
        Some(WorkspaceLocation::DataDir) => {
            let name = name.ok_or_else(|| anyhow::anyhow!("数据目录模式缺少工作区名称"))?;
            Ok(data_root.join("workspaces").join(name))
        }
        Some(WorkspaceLocation::CurrentDir) => Ok(cwd.to_path_buf()),
        Some(WorkspaceLocation::Custom(path)) => Ok(path.clone()),
        None => Ok(dir_flag.map_or_else(|| cwd.to_path_buf(), Path::to_path_buf)),
    }
}

pub(crate) fn init(
    svc: &WorkspaceService,
    dir_flag: Option<&Path>,
    name_flag: Option<&str>,
    printer: Printer,
) -> Result<()> {
    let interactive = printer.is_interactive();
    let wizard = interactive && dir_flag.is_none();
    let seq = svc.next_default_name()?.as_str().to_owned();
    let cwd = std::env::current_dir().context("获取当前工作目录失败")?;
    let data_root = crate::config::platform_data_dir()?;

    // 1/3 位置：--dir / 非交互直接给定；完整向导走三选项
    let loc = if wizard {
        prompts::step(printer, 1, 3, "选择目录");
        let loc = prompts::pick_workspace_location()?;
        // 当前目录在进名称步骤前先判可用，免得白填两步
        if matches!(loc, WorkspaceLocation::CurrentDir)
            && let Some(issue) = prompts::dir_issue(&cwd)
        {
            return Err(anyhow::anyhow!(
                "当前目录不可用：{issue}；重新运行并选择其他位置"
            ));
        }
        Some(loc)
    } else {
        None
    };

    // 2/3 名称：--name > 交互输入（默认=目录名，非法退回序号）> 非交互交 service 序号
    let name: Option<String> = match name_flag {
        Some(name) => Some(name.to_owned()),
        None if interactive => {
            if wizard {
                prompts::step(printer, 2, 3, "工作区名称");
            }
            let default = name_default(loc.as_ref(), dir_flag, &cwd, &seq);
            Some(prompts::input_name("工作区名称", &default)?)
        }
        None => None,
    };

    let dir = resolve_dir(loc.as_ref(), dir_flag, name.as_deref(), &cwd, &data_root)?;

    // 数据目录路径由名称合成，选不到可用目录时在这里亮出来
    if matches!(loc, Some(WorkspaceLocation::DataDir))
        && let Some(issue) = prompts::dir_issue(&dir)
    {
        return Err(anyhow::anyhow!("数据目录不可用：{issue}"));
    }

    if wizard {
        prompts::step(printer, 3, 3, "确认创建");
    }
    if interactive {
        let abs = absolute_path(&dir);
        let name_display = name.as_deref().unwrap_or("(自动序号)");
        let confirmed = prompts::confirm_yes(
            &format!("在 {} 创建并注册工作区 {name_display}?", abs.display()),
            true,
        )?;
        if !confirmed {
            printer.print_section("已取消");
            return Ok(());
        }
    }

    match svc.init(&dir, name.as_deref())? {
        InitOutcome::Created(ws) => printer.print_outcome(
            &format!("已创建并注册: {} ({})", ws.name, ws.path.display()),
            &WorkspaceInitRow {
                name: ws.name.as_str(),
                path: ws.path.display().to_string(),
                status: "created",
            },
        )?,
        InitOutcome::Recovered(ws) => printer.print_outcome(
            &format!("已补注册（上次中断）: {} ({})", ws.name, ws.path.display()),
            &WorkspaceInitRow {
                name: ws.name.as_str(),
                path: ws.path.display().to_string(),
                status: "recovered",
            },
        )?,
        InitOutcome::AlreadyRegistered(ws) => printer.print_outcome(
            &format!(
                "已注册过: {} ({})，可用 mcsup workspace use {}",
                ws.name,
                ws.path.display(),
                ws.name
            ),
            &WorkspaceInitRow {
                name: ws.name.as_str(),
                path: ws.path.display().to_string(),
                status: "already",
            },
        )?,
    }
    Ok(())
}

fn list(svc: &WorkspaceService, reg: &ServerRegistry, printer: Printer) -> Result<()> {
    let all = svc.list()?;
    let rows: Vec<WorkspaceRow<'_>> = all
        .iter()
        .map(|ws| {
            Ok(WorkspaceRow {
                name: ws.name.as_str(),
                path: ws.path.display().to_string(),
                servers: reg.count_in(&ws.id)?,
                current: ws.is_current,
            })
        })
        .collect::<Result<_>>()?;
    printer.print_rows(
        &rows,
        "尚无工作目录，先运行 mcsup workspace init",
        || {
            let mut table = Table::new();
            table.set_header(["名称", "路径", "服务器数", "当前"]);
            for (ws, row) in all.iter().zip(&rows) {
                table.add_row([
                    ws.name.as_str().to_owned(),
                    row.path.clone(),
                    row.servers.to_string(),
                    if row.current { "*" } else { "" }.to_owned(),
                ]);
            }
            table
        },
    )
}

fn use_ws(svc: &WorkspaceService, name: Option<&str>, printer: Printer) -> Result<()> {
    let selector = match name {
        Some(name) => name.to_owned(),
        None => pick_workspace(svc, printer)?,
    };
    let ws = svc.set_current(&selector)?;
    printer.print_outcome(
        &format!("当前工作区已切换为 {} ({})", ws.name, ws.path.display()),
        &WorkspaceUseJson {
            name: ws.name.as_str(),
            path: ws.path.display().to_string(),
        },
    )
}

/// 无参 `workspace use`：交互选择器
fn pick_workspace(svc: &WorkspaceService, printer: Printer) -> Result<String> {
    // 缺必选参数：非交互 / JSON 下不提问、不落库，直接用法错误（退出码 2）
    if !printer.is_interactive() {
        return Err(AppDiagnostic::usage(
            "workspace use 缺少工作区名称",
            Some("非交互模式：mcsup workspace use <名称>".to_owned()),
        )
        .into());
    }
    let all = svc.list()?;
    if all.is_empty() {
        anyhow::bail!("尚无工作目录，先运行 mcsup workspace init");
    }
    let current = svc.current_workspace().ok();
    let options: Vec<String> = all
        .iter()
        .map(|w| {
            let mark = if current.as_ref().is_some_and(|c| c.id == w.id) {
                " (当前)"
            } else {
                ""
            };
            format!("{}{mark}", w.name.as_str())
        })
        .collect();
    let choice = inquire::Select::new("选择工作区", options)
        .prompt()
        .context("读取工作区选择")?;
    let name = choice.split(" (当前)").next().unwrap_or(&choice);
    Ok(name.to_owned())
}

fn remove(svc: &WorkspaceService, name: &str, purge: bool, printer: Printer) -> Result<()> {
    // 先解析：名字不对时不必问，顺带把路径给用户看（删除不可恢复）
    let ws = svc.resolve(name)?;
    // 区内任一服务器在运行则拒绝（purge 会连着运行中的存档一起删）：
    // 判定收敛在 lifecycle service，入口不各自补检查
    super::lifecycle::lifecycle_service()?.ensure_workspace_stopped(&ws.id)?;
    // 删了不可恢复：可交互时问一句，默认不删；`--purge` 是显式指令，不再问
    let purge = if purge || !printer.is_interactive() {
        purge
    } else {
        let confirmed = prompts::confirm_yes(
            &format!(
                "删除 {} ({}) 的磁盘数据?",
                ws.name.as_str(),
                ws.path.display()
            ),
            false,
        )
        .context("已取消注销")?;
        if confirmed {
            // 同意删盘后再上一道闸：红色警示 + 手输工作区名称，防误删存档与备份
            printer.print_warning(&format!(
                "警告: 将永久删除 {} ({}) 中的所有存档、备份等文件，此操作无法恢复",
                ws.name.as_str(),
                ws.path.display()
            ));
            prompts::confirm_by_name(
                &format!("输入工作区名称 {} 以确认删除", ws.name.as_str()),
                ws.name.as_str(),
            )?;
        }
        confirmed
    };
    if purge {
        // 自身工作目录在目标里时我们自己也持有句柄，先退到目标之外；
        // 终端的工作目录不由我们释放，那种情况只能提示用户 cd 出去
        super::chdir_out_of(&ws.path);
    }
    let ws = svc.remove(name, purge)?;
    printer.print_outcome(
        &if purge {
            format!("已注销并删除磁盘数据: {}", ws.name)
        } else {
            format!("已注销（磁盘数据保留）: {}", ws.name)
        },
        &WorkspaceRemoveJson {
            name: ws.name.as_str(),
            path: ws.path.display().to_string(),
            purged: purge,
        },
    )?;
    // 删除的是当前工作区时告知移交结果，避免用户找不到「当前」去哪了
    if ws.is_current
        && let Ok(cur) = svc.current_workspace()
    {
        printer.print_section(&format!("当前工作区已移交给 {}", cur.name));
    }
    Ok(())
}

/// 全部检查项标识（`service::KNOWN_KINDS` 是本模块的唯一来源）
const KNOWN_KINDS: &[&str] = mcsup::service::KNOWN_KINDS;

fn doctor_cmd(
    doctor: &DoctorService,
    only: &[String],
    strict: bool,
    printer: Printer,
) -> Result<()> {
    for kind in only {
        if !KNOWN_KINDS.contains(&kind.as_str()) {
            return Err(AppDiagnostic::usage(
                format!("未知检查项: {kind}"),
                Some(format!("可用检查项: {}", KNOWN_KINDS.join(", "))),
            )
            .into());
        }
    }

    let report = doctor.run()?;
    // --only 过滤只影响展示与退出码判定，检查本身仍然全跑：
    // 过滤在 service 里做会让「不关心」变成「不检查」，掩盖连带问题
    let findings: Vec<&Finding> = report
        .findings()
        .iter()
        .filter(|f| only.is_empty() || only.iter().any(|k| k == f.kind.as_str()))
        .collect();

    let errors = findings
        .iter()
        .filter(|f| f.severity() == Severity::Error)
        .count();
    let warnings = findings.len() - errors;

    if printer.is_human() {
        if findings.is_empty() {
            printer.print_section("检查通过：工作区与注册表一致");
        } else {
            for f in &findings {
                let tag = match f.severity() {
                    Severity::Error => "ERROR",
                    Severity::Warning => "WARN",
                };
                let scope = f.workspace.as_deref().map_or("(全局)", |w| w);
                let line = format!(
                    "[{tag}] {scope}: {}\n        {}\n        修复：{}",
                    crate::i18n::check(&f.kind),
                    crate::i18n::check_location(&f.kind),
                    crate::i18n::check_fix(&f.kind)
                );
                match f.severity() {
                    Severity::Error => printer.print_warning(&line),
                    Severity::Warning => printer.print_section(&line),
                }
            }
        }
    }

    let payload = DoctorJson {
        schema: 1,
        has_errors: errors > 0,
        errors,
        warnings,
        findings: findings
            .iter()
            .map(|f| DoctorFindingJson {
                level: f.severity().as_str(),
                kind: f.kind.as_str(),
                workspace: f.workspace.as_deref(),
                detail: detail_json(&f.kind),
                message: crate::i18n::check(&f.kind),
            })
            .collect(),
    };
    let summary = if findings.is_empty() {
        "检查通过".to_owned()
    } else {
        format!(
            "发现 {} 个问题（{errors} error, {warnings} warning）",
            findings.len()
        )
    };
    printer.print_outcome(&summary, &payload)?;

    if errors > 0 || (strict && warnings > 0) {
        return Err(AppDiagnostic::findings(errors).into());
    }
    Ok(())
}

/// 结构化字段：与 `Kind` 同源，kind 与 detail 不可能不一致
fn detail_json(kind: &CheckKind) -> serde_json::Value {
    match kind {
        CheckKind::WorkspaceDirMissing { path }
        | CheckKind::WorkspaceMarkerMissing { path }
        | CheckKind::DanglingLink { path }
        | CheckKind::OnDiskNotRegistered { path } => {
            serde_json::json!({ "kind": kind.as_str(), "path": path })
        }
        CheckKind::WorkspaceMarkerMismatch {
            path,
            found,
            expected,
        } => serde_json::json!({
            "kind": kind.as_str(),
            "path": path,
            "found": found,
            "expected": expected,
        }),
        CheckKind::ServersDirUnreadable { path, reason } => {
            serde_json::json!({ "kind": kind.as_str(), "path": path, "reason": reason })
        }
        CheckKind::RegisteredButMissing { name, path } => serde_json::json!({
            "kind": kind.as_str(),
            "name": name.as_str(),
            "path": path,
        }),
        CheckKind::RelDirNotDirectChild {
            name,
            rel_dir,
            workspace_path,
        } => serde_json::json!({
            "kind": kind.as_str(),
            "name": name.as_str(),
            "rel_dir": rel_dir,
            "workspace_path": workspace_path,
        }),
        CheckKind::OrphanServerRecord { name, workspace_id } => serde_json::json!({
            "kind": kind.as_str(),
            "name": name.as_str(),
            "workspace_id": workspace_id,
        }),
        // 新增类型时这里退化为最小形状；需要可寻址字段时补分支
        _ => serde_json::json!({ "kind": kind.as_str() }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_default_prefers_dirname_then_sequence() {
        let cwd = Path::new("/tmp/my-ws");
        // 显式 --dir：目录名
        assert_eq!(
            name_default(None, Some(Path::new("/srv/proj-1")), cwd, "workspace03"),
            "proj-1"
        );
        // 目录名不合法 → 序号
        assert_eq!(
            name_default(None, Some(Path::new("/srv/带空格")), cwd, "workspace03"),
            "workspace03"
        );
        // 当前目录：目录名
        assert_eq!(
            name_default(
                Some(&WorkspaceLocation::CurrentDir),
                None,
                cwd,
                "workspace03"
            ),
            "my-ws"
        );
        // 自定义目录：目录名
        assert_eq!(
            name_default(
                Some(&WorkspaceLocation::Custom(PathBuf::from("/srv/x-y"))),
                None,
                cwd,
                "workspace03"
            ),
            "x-y"
        );
        // 数据目录：名称即路径段，只能用序号
        assert_eq!(
            name_default(Some(&WorkspaceLocation::DataDir), None, cwd, "workspace03"),
            "workspace03"
        );
        // 非交互无 --dir：同样按目录名兜底（service 序号仅在无名时生效）
        assert_eq!(name_default(None, None, cwd, "workspace03"), "my-ws");
        // 根目录没有目录名 → 序号
        assert_eq!(
            name_default(None, None, Path::new("/"), "workspace03"),
            "workspace03"
        );
    }

    #[test]
    fn resolve_dir_maps_each_location() {
        let cwd = Path::new("/cwd");
        let data_root = Path::new("/data/mcsup");

        // 数据目录：路径由名称合成
        let dir = resolve_dir(
            Some(&WorkspaceLocation::DataDir),
            None,
            Some("ws1"),
            cwd,
            data_root,
        )
        .expect("数据目录");
        assert_eq!(dir, data_root.join("workspaces").join("ws1"));
        // 数据目录缺名称 → 错误（交互下不可达）
        assert!(
            resolve_dir(
                Some(&WorkspaceLocation::DataDir),
                None,
                None,
                cwd,
                data_root
            )
            .is_err(),
            "缺名称必须报错"
        );

        // 当前目录
        assert_eq!(
            resolve_dir(
                Some(&WorkspaceLocation::CurrentDir),
                None,
                None,
                cwd,
                data_root
            )
            .expect("当前目录"),
            cwd
        );

        // 自定义目录原样
        let custom = PathBuf::from("/srv/custom");
        assert_eq!(
            resolve_dir(
                Some(&WorkspaceLocation::Custom(custom.clone())),
                None,
                None,
                cwd,
                data_root
            )
            .expect("自定义"),
            custom
        );

        // --dir 优先（未进向导）
        assert_eq!(
            resolve_dir(None, Some(Path::new("/flag/dir")), None, cwd, data_root).expect("--dir"),
            PathBuf::from("/flag/dir")
        );
        // 非交互回退当前目录（保持既有机器行为）
        assert_eq!(
            resolve_dir(None, None, None, cwd, data_root).expect("cwd"),
            cwd
        );
    }
}
