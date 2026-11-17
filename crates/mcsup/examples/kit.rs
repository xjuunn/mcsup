//! kit 任务级接口的使用示例
//!
//! 运行需要真实的 mcsup 数据目录与网络（Mojang / Modrinth / Adoptium）：
//!
//! ```sh
//! cargo run -p mcsup --features kit --example kit
//! ```
//!
//! 示例只演示调用顺序：每个任务只写要覆盖的字段，其余默认值来自
//! [`KitConfig`] 与内置默认，不需要为每种组合构造参数。
//!
//! 任务按领域分组挂在 `kit.servers()` / `kit.backups()` / `kit.content()` /
//! `kit.java()` 上，读起来就是「哪个领域、做什么」。

#![allow(clippy::print_stdout)]

use mcsup::bootstrap::Mcsup;
use mcsup::domain::java::JavaMajor;
use mcsup::kit::{
    BackupNow, CreateServer, InstallCore, InstallJava, InstallMod, InstallModpack, Kit, KitConfig,
    ListBackups, PruneBackups, RemoveMod, RemoveServer, RestoreBackup, StartServer, StopServer,
    UpdateMods,
};
use std::error::Error;
use std::path::Path;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    // 组合根：一次装配全部默认实现（数据目录、数据库、服务与下载器）
    let mcsup = Mcsup::open(Path::new("./mcsup-data"))?;

    // 实例级默认：只改要改的项，其余保持内置默认
    let kit = mcsup.kit(KitConfig {
        accept_eula: true,   // 起服时代为同意 EULA
        ram_mib: Some(4096), // 起服堆上限
        ..KitConfig::default()
    });

    create_and_run(&kit).await?;
    backup_flow(&kit)?;
    content_flow(&kit).await?;
    modpack_flow(&kit).await?;
    java_flow(&kit).await?;

    // 删服（缺省只注销登记，purge 才删目录）
    kit.servers()
        .remove("survival", RemoveServer::default().purge(true))?;
    Ok(())
}

/// 建服 → 换核心 → 起服 → 查状态 → 重启 → 停服
async fn create_and_run(kit: &Kit<'_>) -> Result<(), Box<dyn Error>> {
    // 建目录 + 写 marker + 入库 + 装核心
    let created = kit
        .servers()
        .create(CreateServer::new("survival").version("1.21.4"))
        .await?;
    println!(
        "created {} ({}, {})",
        created.core.server.meta.name,
        created.core.server.meta.variant.as_str(),
        created.core.server.meta.version
    );

    kit.servers()
        .install_core("survival", InstallCore::default().version("1.21.5"))
        .await?;
    kit.servers().start("survival", StartServer::default())?;
    println!("status: {:?}", kit.servers().status("survival")?);
    kit.servers()
        .restart("survival", StartServer::default(), StopServer::default())?;
    kit.servers().stop("survival", StopServer::default())?;
    Ok(())
}

/// 备份全流程：建 → 列 → 校验 → 恢复 → 删 → 按保留策略清理
fn backup_flow(kit: &Kit<'_>) -> Result<(), Box<dyn Error>> {
    let snapshot = kit
        .backups()
        .create("survival", BackupNow::default().label("before-upgrade"))?;
    let page = kit
        .backups()
        .list("survival", ListBackups::default().size(20))?;
    println!("snapshots: {} / {}", page.items.len(), page.total);

    let verified = kit
        .backups()
        .verify("survival", snapshot.snapshot.id.as_str())?;
    println!(
        "verified: {} objects, {} broken",
        verified.objects,
        verified.broken.len()
    );

    kit.backups().restore(
        "survival",
        snapshot.snapshot.id.as_str(),
        RestoreBackup::default(), // 缺省原地恢复
    )?;
    kit.backups()
        .delete("survival", snapshot.snapshot.id.as_str())?;
    kit.backups()
        .prune("survival", PruneBackups::default().keep(5))?;
    Ok(())
}

/// 内容：装 / 更新 / 移除（版本与落点由 service 的计划决定）
async fn content_flow(kit: &Kit<'_>) -> Result<(), Box<dyn Error>> {
    let installed = kit
        .content()
        .install("survival", "fabric-api", InstallMod::default())
        .await?;
    println!(
        "installed {} -> {}",
        installed.plan.version_number, installed.plan.file
    );

    let updated = kit
        .content()
        .update("survival", UpdateMods::default())
        .await?;
    println!("updates planned: {}", updated.plan.items.len());
    kit.content()
        .remove("survival", "fabric-api", RemoveMod::default())?;
    Ok(())
}

/// 整合包开服：加载器与 MC 版本取自包内清单
async fn modpack_flow(kit: &Kit<'_>) -> Result<(), Box<dyn Error>> {
    let packed = kit
        .servers()
        .create(CreateServer::new("atm").modpack("all-the-mods-10"))
        .await?;
    println!(
        "modpack server {} runs {} {}",
        packed.core.server.meta.name,
        packed.core.server.meta.variant.as_str(),
        packed.core.server.meta.version
    );

    // 建服中途失败（记录已建、内容没装完）时，重跑这一步补装，不需要重建
    let reinstalled = kit
        .servers()
        .install_modpack("atm", "all-the-mods-10", InstallModpack::default())
        .await?;
    println!(
        "modpack installed: {} {}, {} server files",
        reinstalled.variant.as_str(),
        reinstalled.minecraft,
        reinstalled.applied.file_count
    );
    Ok(())
}

/// Java：装托管运行时，再列本机可用版本
async fn java_flow(kit: &Kit<'_>) -> Result<(), Box<dyn Error>> {
    let java = kit
        .java()
        .install(InstallJava::default().major(JavaMajor::parse(21)?))
        .await?;
    println!(
        "java {} at {}",
        java.runtime.version,
        java.runtime.path.display()
    );

    for runtime in kit.java().runtimes()? {
        println!(
            "java available: {} {}",
            runtime.major,
            runtime.vendor.as_str()
        );
    }
    Ok(())
}
