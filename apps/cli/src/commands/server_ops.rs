use crate::bootstrap;
use crate::output::Printer;
use anyhow::{Context, Result};
use comfy_table::Table;
use mcsup::domain::server::version::{ServerVariant, VersionId};
use mcsup::service::ServerRegistry;
use serde::Serialize;

/// `server list` 单行（JSON 模式序列化用）
#[derive(Serialize)]
struct ServerRow<'a> {
    name: &'a str,
    version: &'a str,
    variant: &'a str,
    path: String,
    jar: Option<String>,
}

/// `server create` 结果（JSON 模式序列化用）
#[derive(Serialize)]
struct CreateRow<'a> {
    name: &'a str,
    version: &'a str,
    variant: &'a str,
    path: String,
}

pub(crate) fn open_registry() -> Result<std::sync::Arc<ServerRegistry>> {
    bootstrap::open_registry()
}

pub(crate) fn list(printer: Printer) -> Result<()> {
    let reg = open_registry()?;
    let all = reg.list().context("列出服务器失败")?;
    let rows: Vec<ServerRow<'_>> = all
        .iter()
        .map(|srv| ServerRow {
            name: srv.meta.name.as_str(),
            version: srv.meta.version.as_str(),
            variant: srv.meta.variant.as_str(),
            path: srv.dir.display().to_string(),
            jar: srv.meta.jar.as_ref().map(|j| j.path.display().to_string()),
        })
        .collect();
    printer.print_rows(
        &rows,
        "尚无服务器，先运行 mcsup server create",
        || {
            let mut table = Table::new();
            table.set_header(["名称", "版本", "变体", "路径"]);
            for srv in &all {
                table.add_row([
                    srv.meta.name.as_str().to_owned(),
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
    let srv = reg
        .create(workspace, name, &version_id, variant)
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
        },
    )
}
