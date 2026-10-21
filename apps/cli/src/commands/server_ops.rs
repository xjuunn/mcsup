use crate::bootstrap;
use crate::output::Printer;
use anyhow::{Context, Result};
use comfy_table::Table;
use inquire::Confirm;
use mcsup::domain::server::version::{ServerVariant, VersionId};
use mcsup::domain::workspace::WorkspaceId;
use mcsup::service::{ServerRegistry, WorkspaceService};
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;

/// `server list` 单行（JSON 模式序列化用）
#[derive(Serialize)]
struct ServerRow<'a> {
    name: &'a str,
    workspace: String,
    version: &'a str,
    variant: &'a str,
    path: String,
    jar: Option<String>,
    linked: bool,
}

/// `server create` / `server link` 结果（JSON 模式序列化用）
#[derive(Serialize)]
struct CreateRow<'a> {
    name: &'a str,
    version: &'a str,
    variant: &'a str,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    linked: Option<bool>,
}

pub(crate) fn open_registry() -> Result<std::sync::Arc<ServerRegistry>> {
    bootstrap::open_registry()
}

/// 缺省工作区选择器：flag > `MCSUP_WORKSPACE` 环境变量 > 当前工作区
///
/// 环境变量在入口统一解析成显式选择器传给 service，service 只认一种口径。
#[must_use]
pub(crate) fn workspace_selector(flag: Option<&str>) -> Option<String> {
    flag.map(str::to_owned).or_else(|| {
        std::env::var("MCSUP_WORKSPACE")
            .ok()
            .filter(|s| !s.trim().is_empty())
    })
}

fn workspace_names() -> Result<HashMap<WorkspaceId, String>> {
    let store = bootstrap::open_store()?;
    let svc = WorkspaceService::new(store.clone(), store);
    Ok(svc
        .list()?
        .into_iter()
        .map(|w| (w.id, w.name.as_str().to_owned()))
        .collect())
}

pub(crate) fn list(printer: Printer) -> Result<()> {
    let reg = open_registry()?;
    let all = reg.list().context("列出服务器失败")?;
    let ws_names = workspace_names()?;
    let rows: Vec<ServerRow<'_>> = all
        .iter()
        .map(|srv| ServerRow {
            name: srv.meta.name.as_str(),
            workspace: ws_names
                .get(&srv.meta.workspace_id)
                .cloned()
                .unwrap_or_else(|| srv.meta.workspace_id.as_str().to_owned()),
            version: srv.meta.version.as_str(),
            variant: srv.meta.variant.as_str(),
            path: srv.dir.display().to_string(),
            jar: srv.meta.jar.as_ref().map(|j| j.path.display().to_string()),
            linked: srv.linked,
        })
        .collect();
    printer.print_rows(
        &rows,
        "尚无服务器，先运行 mcsup server create",
        || {
            let mut table = Table::new();
            table.set_header(["名称", "工作区", "版本", "变体", "路径"]);
            for srv in &all {
                let ws = ws_names
                    .get(&srv.meta.workspace_id)
                    .cloned()
                    .unwrap_or_else(|| srv.meta.workspace_id.as_str().to_owned());
                table.add_row([
                    srv.meta.name.as_str().to_owned(),
                    ws,
                    srv.meta.version.as_str().to_owned(),
                    srv.meta.variant.as_str().to_owned(),
                    srv.dir.display().to_string(),
                ]);
            }
            table
        },
    )
}

pub(crate) fn create(
    name: &str,
    version: &str,
    variant: Option<&str>,
    workspace: Option<&str>,
    printer: Printer,
) -> Result<()> {
    let reg = open_registry()?;
    let version_id = VersionId::new(version);
    let variant = match variant {
        Some(v) => ServerVariant::parse(v).map_err(|_| anyhow::anyhow!("未知变体: {v}"))?,
        None => ServerVariant::Vanilla,
    };
    let selector = workspace_selector(workspace);
    let srv = reg
        .create(selector.as_deref(), name, &version_id, variant)
        .with_context(|| format!("创建服务器 {name} 失败"))?;
    printer.print_outcome(
        &format!(
            "已创建: {} {} → {}",
            srv.meta.name,
            srv.meta.version,
            srv.dir.display()
        ),
        &CreateRow {
            name: srv.meta.name.as_str(),
            version: srv.meta.version.as_str(),
            variant: srv.meta.variant.as_str(),
            path: srv.dir.display().to_string(),
            linked: None,
        },
    )
}

pub(crate) fn link(
    path: &Path,
    name: Option<&str>,
    workspace: Option<&str>,
    printer: Printer,
) -> Result<()> {
    let reg = open_registry()?;
    let selector = workspace_selector(workspace);
    let out = reg
        .link(path, name, selector.as_deref())
        .with_context(|| format!("链接服务器目录 {} 失败", path.display()))?;
    printer.print_outcome(
        &format!(
            "已链接: {} → {}（目标 {}）",
            out.server.meta.name,
            out.server.dir.display(),
            out.target.display()
        ),
        &CreateRow {
            name: out.server.meta.name.as_str(),
            version: out.server.meta.version.as_str(),
            variant: out.server.meta.variant.as_str(),
            path: out.server.dir.display().to_string(),
            linked: Some(true),
        },
    )
}

pub(crate) fn remove(name: &str, purge: bool, printer: Printer) -> Result<()> {
    let reg = open_registry()?;
    // 先看一眼：链接服务器 + purge 会被 service 拒绝，这里提前给出更友好的确认
    let srv = reg
        .get(name)
        .with_context(|| format!("服务器 {name} 不存在"))?;
    let purge = if purge && srv.linked {
        printer.print_warning(&format!(
            "{name} 是链接服务器，--purge 会拒绝执行（目标在工作区外，无法安全删除）"
        ));
        true // 仍传给 service，由其拒绝并给出统一错误
    } else if purge && !printer.is_interactive() {
        true
    } else if purge {
        printer.print_warning(&format!(
            "警告: 将永久删除 {} ({}) 目录及其中所有文件，此操作无法恢复",
            name,
            srv.dir.display()
        ));
        Confirm::new(&format!("删除 {name} 的磁盘数据?"))
            .with_default(false)
            .prompt()
            .context("已取消删除")?
    } else {
        false
    };
    let out = reg.remove(name, purge)?;
    printer.print_outcome(
        &if purge {
            format!("已删除并注销: {}", out.meta.name)
        } else if out.linked {
            format!("已移除链接并注销: {}（目标目录保留）", out.meta.name)
        } else {
            format!("已注销（磁盘数据保留）: {}", out.meta.name)
        },
        &serde_json::json!({
            "name": out.meta.name.as_str(),
            "path": out.dir.display().to_string(),
            "purged": purge,
            "linked": out.linked,
        }),
    )
}
