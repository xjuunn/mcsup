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

#![allow(clippy::print_stdout)]

use mcsup::bootstrap::Mcsup;
use mcsup::domain::java::JavaMajor;
use mcsup::kit::{
    BackupNow, CreateServer, InstallCore, InstallJava, InstallMod, InstallModpack, KitConfig,
    ListBackups, PruneBackups, RemoveMod, RemoveServer, RestoreBackup, StartServer, StopServer,
    UpdateMods,
};
use std::path::Path;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 组合根：一次装配全部默认实现（数据目录、数据库、服务与下载器）
    let mcsup = Mcsup::open(Path::new("./mcsup-data"))?;

    // 实例级默认：只改要改的项，其余保持内置默认
    let kit = mcsup.kit(KitConfig {
        accept_eula: true,   // 起服时代为同意 EULA
        ram_mib: Some(4096), // 起服堆上限
        ..KitConfig::default()
    });

    // 1. 普通服：建目录 + 写 marker + 入库 + 装核心
    let created = kit
        .create_server(CreateServer::new("survival").version("1.21.4"))
        .await?;
    println!(
        "created {} ({}, {})",
        created.core.server.meta.name,
        created.core.server.meta.variant.as_str(),
        created.core.server.meta.version
    );

    // 2. 换核心 / 起服 / 查状态 / 重启 / 停服
    kit.install_core("survival", InstallCore::default().version("1.21.5"))
        .await?;
    kit.start_server("survival", StartServer::default())?;
    println!("status: {:?}", kit.server_status("survival")?);
    kit.restart_server("survival", StartServer::default(), StopServer::default())?;
    kit.stop_server("survival", StopServer::default())?;

    // 3. 备份全流程：建 → 列 → 校验 → 恢复 → 删 → 按保留策略清理
    let snapshot = kit.backup_now("survival", BackupNow::default().label("before-upgrade"))?;
    let page = kit.list_backups("survival", ListBackups::default().size(20))?;
    println!("snapshots: {} / {}", page.items.len(), page.total);
    let verified = kit.verify_backup("survival", snapshot.snapshot.id.as_str())?;
    println!(
        "verified: {} objects, {} broken",
        verified.objects,
        verified.broken.len()
    );
    kit.restore_backup(
        "survival",
        snapshot.snapshot.id.as_str(),
        RestoreBackup::default(), // 缺省原地恢复
    )?;
    kit.delete_backup("survival", snapshot.snapshot.id.as_str())?;
    kit.prune_backups("survival", PruneBackups::default().keep(5))?;

    // 4. 内容：装 / 更新 / 移除（版本与落点由 service 的计划决定）
    let installed = kit
        .install_mod("survival", "fabric-api", InstallMod::default())
        .await?;
    println!(
        "installed {} -> {}",
        installed.plan.version_number, installed.plan.file
    );
    let updated = kit.update_mods("survival", UpdateMods::default()).await?;
    println!("updates planned: {}", updated.plan.items.len());
    kit.remove_mod("survival", "fabric-api", RemoveMod::default())?;

    // 5. 整合包开服：加载器与 MC 版本取自包内清单
    let packed = kit
        .create_server(CreateServer::new("atm").modpack("all-the-mods-10"))
        .await?;
    println!(
        "modpack server {} runs {} {}",
        packed.core.server.meta.name,
        packed.core.server.meta.variant.as_str(),
        packed.core.server.meta.version
    );
    // 建服中途失败（记录已建、内容没装完）时，重跑这一步补装，不需要重建
    let reinstalled = kit
        .install_modpack("atm", "all-the-mods-10", InstallModpack::default())
        .await?;
    println!(
        "modpack installed: {} {}, {} server files",
        reinstalled.variant.as_str(),
        reinstalled.minecraft,
        reinstalled.applied.file_count
    );

    // 6. Java：装托管运行时，再列本机可用版本
    let java = kit
        .install_java(InstallJava::default().major(JavaMajor::parse(21)?))
        .await?;
    println!(
        "java {} at {}",
        java.runtime.version,
        java.runtime.path.display()
    );
    for runtime in kit.java_runtimes()? {
        println!(
            "java available: {} {}",
            runtime.major,
            runtime.vendor.as_str()
        );
    }

    // 7. 删服（缺省只注销登记，purge 才删目录）
    kit.remove_server("survival", RemoveServer::default().purge(true))?;

    Ok(())
}
