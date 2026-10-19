use crate::cli::WorkspaceAction;
use crate::config;
use crate::output::Printer;
use anyhow::{Context, Result};
use comfy_table::Table;
use mcsup::service::{InitOutcome, WorkspaceService};
use mcsup::storage::SqliteStore;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) fn run(action: WorkspaceAction, printer: Printer) -> Result<()> {
    let store = open_store()?;
    let svc = WorkspaceService::new(store);
    match action {
        WorkspaceAction::Init { dir, name } => init(&svc, dir, name.as_deref(), printer),
        WorkspaceAction::List => list(&svc, printer),
        WorkspaceAction::Use { name } => use_ws(&svc, &name, printer),
        WorkspaceAction::Remove { name, purge } => remove(&svc, &name, purge, printer),
        WorkspaceAction::Info => info(&svc, printer),
    }
}

fn open_store() -> Result<Arc<SqliteStore>> {
    let path = config::database_path()?;
    let store =
        SqliteStore::open(&path).with_context(|| format!("打开元数据库 {}", path.display()))?;
    Ok(Arc::new(store))
}

fn init(
    svc: &WorkspaceService,
    dir: Option<PathBuf>,
    name: Option<&str>,
    printer: Printer,
) -> Result<()> {
    let interactive = printer.is_human() && std::io::stdin().is_terminal();

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
        let abs = absolute_display(&dir);
        let name_display = name.unwrap_or("(自动序号)");
        let confirmed =
            inquire::Confirm::new(&format!("在 {abs} 创建并注册工作区 {name_display}?"))
                .with_default(true)
                .prompt()
                .context("确认创建")?;
        if !confirmed {
            printer.print_section("已取消");
            return Ok(());
        }
    }

    match svc.init(&dir, name)? {
        InitOutcome::Created(ws) => {
            printer.print_section(&format!(
                "已创建并注册: {} ({})",
                ws.name,
                ws.path.display()
            ));
        }
        InitOutcome::Recovered(ws) => {
            printer.print_section(&format!(
                "已补注册（上次中断）: {} ({})",
                ws.name,
                ws.path.display()
            ));
        }
        InitOutcome::AlreadyRegistered(ws) => {
            printer.print_section(&format!(
                "已注册过: {} ({})，可用 mcsup workspace use {}",
                ws.name,
                ws.path.display(),
                ws.name
            ));
        }
    }
    Ok(())
}

fn list(svc: &WorkspaceService, printer: Printer) -> Result<()> {
    let all = svc.list()?;
    if all.is_empty() {
        printer.print_section("尚无工作目录，先运行 mcsup workspace init");
        return Ok(());
    }
    let mut table = Table::new();
    table.set_header(["名称", "路径", "服务器数", "默认"]);
    for ws in &all {
        table.add_row([
            ws.name.as_str().to_owned(),
            ws.path.display().to_string(),
            count_servers(&ws.path).to_string(),
            if ws.is_default { "*" } else { "" }.to_owned(),
        ]);
    }
    crate::output::print_table(&table);
    Ok(())
}

fn count_servers(path: &Path) -> usize {
    std::fs::read_dir(path.join("servers")).map_or(0, |it| {
        it.filter(|e| e.as_ref().is_ok_and(|e| e.path().is_dir()))
            .count()
    })
}

fn use_ws(svc: &WorkspaceService, name: &str, printer: Printer) -> Result<()> {
    let ws = svc.set_default(name)?;
    printer.print_section(&format!(
        "默认工作目录已切换为 {} ({})",
        ws.name,
        ws.path.display()
    ));
    Ok(())
}

fn remove(svc: &WorkspaceService, name: &str, purge: bool, printer: Printer) -> Result<()> {
    let ws = svc.remove(name, purge)?;
    if purge {
        printer.print_section(&format!("已注销并删除磁盘数据: {}", ws.name));
    } else {
        printer.print_section(&format!("已注销（磁盘数据保留）: {}", ws.name));
    }
    Ok(())
}

fn info(svc: &WorkspaceService, printer: Printer) -> Result<()> {
    let data = config::platform_data_dir()?;
    printer.print_section(&format!("平台数据目录: {}", data.display()));
    match svc.default_workspace() {
        Ok(ws) => printer.print_section(&format!(
            "默认工作目录: {} ({})",
            ws.name,
            ws.path.display()
        )),
        Err(_) => printer.print_section("默认工作目录: 尚无，先运行 mcsup workspace init"),
    }
    Ok(())
}

fn absolute_display(dir: &Path) -> String {
    let abs = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(dir)
    };
    abs.display().to_string()
}
