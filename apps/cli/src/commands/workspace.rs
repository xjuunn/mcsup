use crate::bootstrap;
use crate::cli::WorkspaceAction;
use crate::output::Printer;
use anyhow::{Context, Result};
use comfy_table::Table;
use inquire::Confirm;
use inquire::validator::{ErrorMessage, Validation};
use mcsup::domain::workspace::Workspace;
use mcsup::service::{InitOutcome, ServerRegistry, WorkspaceService, absolute_path};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// `workspace list` 单行（JSON 模式序列化用）
#[derive(Serialize)]
struct WorkspaceRow<'a> {
    name: &'a str,
    path: String,
    servers: usize,
    current: bool,
}

/// `workspace init` 结果（JSON 模式序列化用）
#[derive(Serialize)]
struct InitRow<'a> {
    name: &'a str,
    path: String,
    status: &'a str,
}

pub(crate) fn run(action: WorkspaceAction, printer: Printer) -> Result<()> {
    let store = bootstrap::open_store()?;
    let reg = Arc::new(ServerRegistry::new(store.clone(), store.clone()));
    let svc = WorkspaceService::new(store.clone(), store);
    match action {
        WorkspaceAction::Init { dir, name } => init(&svc, dir, name.as_deref(), printer),
        WorkspaceAction::List => list(&svc, &reg, printer),
        WorkspaceAction::Use { name } => use_ws(&svc, name.as_deref(), printer),
        WorkspaceAction::Remove { name, purge } => remove(&svc, &name, purge, printer),
        WorkspaceAction::Doctor => doctor(&reg, printer),
    }
}

fn init(
    svc: &WorkspaceService,
    dir: Option<PathBuf>,
    name: Option<&str>,
    printer: Printer,
) -> Result<()> {
    let interactive = printer.is_interactive();

    let dir = match dir {
        Some(dir) => dir,
        None if interactive => {
            let input: String = inquire::Text::new("工作目录")
                .with_default(".")
                .prompt()
                .context("读取工作目录输入")?;
            PathBuf::from(input)
        }
        None => std::env::current_dir().context("获取当前工作目录失败")?,
    };

    // 名称：--name 优先；交互下询问，默认 next_default_name()；非交互无 --name 时由 service 生成
    let name = match name {
        Some(name) => Some(name.to_owned()),
        None if interactive => {
            let default_name = svc.next_default_name()?.as_str().to_owned();
            let input: String = inquire::Text::new("工作区名称")
                .with_default(&default_name)
                .prompt()
                .context("读取工作区名称输入")?;
            let input = input.trim();
            if input.is_empty() {
                Some(default_name)
            } else {
                Some(input.to_owned())
            }
        }
        None => None,
    };
    let name = name.as_deref();

    if interactive {
        let abs = absolute_path(&dir);
        let name_display = name.unwrap_or("(自动序号)");
        let confirmed = Confirm::new(&format!(
            "在 {} 创建并注册工作区 {name_display}?",
            abs.display()
        ))
        .with_default(true)
        .prompt()
        .context("确认创建")?;
        if !confirmed {
            printer.print_section("已取消");
            return Ok(());
        }
    }

    match svc.init(&dir, name)? {
        InitOutcome::Created(ws) => printer.print_outcome(
            &format!("已创建并注册: {} ({})", ws.name, ws.path.display()),
            &InitRow {
                name: ws.name.as_str(),
                path: ws.path.display().to_string(),
                status: "created",
            },
        )?,
        InitOutcome::Recovered(ws) => printer.print_outcome(
            &format!("已补注册（上次中断）: {} ({})", ws.name, ws.path.display()),
            &InitRow {
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
            &InitRow {
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
        &serde_json::json!({
            "name": ws.name.as_str(),
            "path": ws.path.display().to_string(),
        }),
    )
}

/// 无参 `workspace use`：交互选择器
fn pick_workspace(svc: &WorkspaceService, printer: Printer) -> Result<String> {
    let all = svc.list()?;
    if all.is_empty() {
        anyhow::bail!("尚无工作目录，先运行 mcsup workspace init");
    }
    if !printer.is_interactive() {
        let names: Vec<&str> = all.iter().map(|w| w.name.as_str()).collect();
        anyhow::bail!(
            "非交互模式必须指定工作区名称（如 `mcsup workspace use {}`）",
            names[0]
        );
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
    // 删了不可恢复：可交互时问一句，默认不删；`--purge` 是显式指令，不再问
    let purge = if purge || !printer.is_interactive() {
        purge
    } else {
        let confirmed = Confirm::new(&format!(
            "删除 {} ({}) 的磁盘数据?",
            ws.name.as_str(),
            ws.path.display()
        ))
        .with_default(false)
        .prompt()
        .context("已取消注销")?;
        if confirmed {
            // 同意删盘后再上一道闸：红色警示 + 手输工作区名称，防误删存档与备份
            confirm_purge_by_name(&ws, printer)?;
        }
        confirmed
    };
    if purge {
        // 自身工作目录在目标里时我们自己也持有句柄，先退到目标之外；
        // 终端的工作目录不由我们释放，那种情况只能提示用户 cd 出去
        chdir_out_of(&ws.path);
    }
    let ws = svc.remove(name, purge)?;
    printer.print_outcome(
        &if purge {
            format!("已注销并删除磁盘数据: {}", ws.name)
        } else {
            format!("已注销（磁盘数据保留）: {}", ws.name)
        },
        &serde_json::json!({
            "name": ws.name.as_str(),
            "path": ws.path.display().to_string(),
            "purged": purge,
        }),
    )?;
    // 删除的是当前工作区时告知移交结果，避免用户找不到「当前」去哪了
    if ws.is_current
        && let Ok(cur) = svc.current_workspace()
    {
        printer.print_section(&format!("当前工作区已移交给 {}", cur.name));
    }
    Ok(())
}

/// 删盘前的第二道确认：红色警示 + 输入工作区名称（交互下同意删盘后调用）
fn confirm_purge_by_name(ws: &Workspace, printer: Printer) -> Result<()> {
    printer.print_warning(&format!(
        "警告: 将永久删除 {} ({}) 中的所有存档、备份等文件，此操作无法恢复",
        ws.name.as_str(),
        ws.path.display()
    ));
    let expected = ws.name.as_str();
    let _: String = inquire::Text::new(&format!("输入工作区名称 {expected} 以确认删除"))
        .with_validator(move |input: &str| {
            Ok(if input.trim() == expected {
                Validation::Valid
            } else {
                Validation::Invalid(ErrorMessage::Custom(
                    "名称不匹配，请重新输入（Ctrl+C 取消）".to_owned(),
                ))
            })
        })
        .prompt()
        .context("已取消删除")?;
    Ok(())
}

/// 目标目录包含当前工作目录时，先退出来
///
/// Windows 上进程的工作目录本身持有句柄，不退出来 `remove_dir_all` 必然失败
/// （终端自己的工作目录无法由我们释放，只能让用户 cd 出去）。
fn chdir_out_of(target: &Path) {
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    if !cwd.starts_with(target) {
        return;
    }
    let outside = target.parent().filter(|p| !p.as_os_str().is_empty());
    let _ = std::env::set_current_dir(outside.unwrap_or(Path::new(".")));
}

fn doctor(reg: &ServerRegistry, printer: Printer) -> Result<()> {
    use mcsup::service::DoctorLevel;

    let findings = reg.doctor()?;
    if findings.is_empty() {
        printer.print_outcome(
            "检查通过：工作区与注册表一致",
            &serde_json::json!({ "findings": [] }),
        )?;
        return Ok(());
    }
    if printer.is_human() {
        for f in &findings {
            let scope = f.workspace.as_deref().unwrap_or("(全局)");
            let level = match f.level {
                DoctorLevel::Error => "ERROR",
                DoctorLevel::Warning => "WARN",
            };
            let line = format!("[{level}] {scope}: {}", f.message);
            match f.level {
                DoctorLevel::Error => printer.print_warning(&line),
                DoctorLevel::Warning => printer.print_section(&line),
            }
        }
    }
    printer.print_outcome(
        &format!("发现 {} 个问题", findings.len()),
        &serde_json::json!({
            "findings": findings.iter().map(|f| serde_json::json!({
                "level": match f.level {
                    DoctorLevel::Error => "error",
                    DoctorLevel::Warning => "warning",
                },
                "workspace": f.workspace,
                "message": f.message,
            })).collect::<Vec<_>>(),
        }),
    )?;
    if findings.iter().any(|f| f.level == DoctorLevel::Error) {
        anyhow::bail!("发现错误级问题，请按提示处理");
    }
    Ok(())
}
