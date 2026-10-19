use crate::config;
use crate::output::Printer;
use anyhow::{Context, Result};
use comfy_table::Table;
use mcsup::domain::server::version::{ServerVariant, VersionId};
use mcsup::service::ServerRegistry;
use mcsup::storage::SqliteStore;
use std::sync::Arc;

/// 打开元数据库并装配 `ServerRegistry`
pub(crate) fn open_registry() -> Result<Arc<ServerRegistry>> {
    let path = config::database_path()?;
    let store = Arc::new(
        SqliteStore::open(&path).with_context(|| format!("打开元数据库 {}", path.display()))?,
    );
    Ok(Arc::new(ServerRegistry::new(store.clone(), store)))
}

pub(crate) fn list(printer: Printer) -> Result<()> {
    let reg = open_registry()?;
    let all = reg.list().context("列出服务器失败")?;
    if all.is_empty() {
        printer.print_section("尚无服务器，先运行 mcsup server create");
        return Ok(());
    }
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
    crate::output::print_table(&table);
    Ok(())
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
    printer.print_section(&format!(
        "已创建: {} {} → {}",
        srv.meta.name,
        srv.meta.version,
        srv.dir.display()
    ));
    Ok(())
}
