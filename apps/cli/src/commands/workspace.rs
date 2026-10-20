use crate::bootstrap;
use crate::cli::WorkspaceAction;
use crate::config;
use crate::output::Printer;
use anyhow::{Context, Result};
use comfy_table::Table;
use inquire::Confirm;
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
    default: bool,
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
    let svc = WorkspaceService::new(store);
    match action {
        WorkspaceAction::Init { dir, name } => init(&svc, dir, name.as_deref(), printer),
        WorkspaceAction::List => list(&svc, &reg, printer),
        WorkspaceAction::Use { name } => use_ws(&svc, &name, printer),
        WorkspaceAction::Remove { name, purge } => remove(&svc, &name, purge, printer),
        WorkspaceAction::Info => info(&svc, printer),
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
                default: ws.is_default,
            })
        })
        .collect::<Result<_>>()?;
    printer.print_rows(
        &rows,
        "尚无工作目录，先运行 mcsup workspace init",
        || {
            let mut table = Table::new();
            table.set_header(["名称", "路径", "服务器数", "默认"]);
            for (ws, row) in all.iter().zip(&rows) {
                table.add_row([
                    ws.name.as_str().to_owned(),
                    row.path.clone(),
                    row.servers.to_string(),
                    if row.default { "*" } else { "" }.to_owned(),
                ]);
            }
            table
        },
    )
}

fn use_ws(svc: &WorkspaceService, name: &str, printer: Printer) -> Result<()> {
    let ws = svc.set_default(name)?;
    printer.print_outcome(
        &format!("默认工作目录已切换为 {} ({})", ws.name, ws.path.display()),
        &serde_json::json!({
            "name": ws.name.as_str(),
            "path": ws.path.display().to_string(),
        }),
    )
}

fn remove(svc: &WorkspaceService, name: &str, purge: bool, printer: Printer) -> Result<()> {
    // 先解析：名字不对时不必问，顺带把路径给用户看（删除不可恢复）
    let ws = svc.resolve(name)?;
    // 删了不可恢复：可交互时问一句，默认不删；`--purge` 是显式指令，不再问
    let purge = if purge || !printer.is_interactive() {
        purge
    } else {
        Confirm::new(&format!(
            "删除 {} ({}) 的磁盘数据?",
            ws.name.as_str(),
            ws.path.display()
        ))
        .with_default(false)
        .prompt()
        .context("已取消注销")?
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
    )
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

fn info(svc: &WorkspaceService, printer: Printer) -> Result<()> {
    let data = config::platform_data_dir()?;
    let default = svc.default_workspace().ok();
    let payload = serde_json::json!({
        "data_dir": data.display().to_string(),
        "default": default.as_ref().map(|ws| serde_json::json!({
            "name": ws.name.as_str(),
            "path": ws.path.display().to_string(),
        })),
    });
    let text = match &default {
        Some(ws) => format!("默认工作目录: {} ({})", ws.name, ws.path.display()),
        None => "默认工作目录: 尚无，先运行 mcsup workspace init".to_owned(),
    };
    printer.print_section(&format!("平台数据目录: {}", data.display()));
    printer.print_outcome(&text, &payload)
}
