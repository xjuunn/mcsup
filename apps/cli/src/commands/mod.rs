mod config;
mod doctor;
mod install;
mod java;
mod lifecycle;
mod props;
mod server_ops;
mod settings;
mod workspace;

use crate::cli::{Cli, Command, JavaAction, ServerAction};
use crate::config::ConfigFile;
use crate::diagnostic::AppDiagnostic;
use crate::output::Printer;
use anyhow::{Context, Result};
use std::path::Path;

pub(crate) async fn run(cli: Cli, cfg: &ConfigFile) -> Result<()> {
    let printer = Printer::new(cli.output);
    match cli.command {
        Command::Workspace { action } => workspace::run(action, printer),
        Command::Server { action } => server(action, cfg, printer).await,
        Command::Modpack { .. } => err("modpack"),
        Command::Java { action } => java(action, printer).await,
        Command::Remote { .. } => err("remote"),
        Command::Doctor {
            only,
            strict,
            offline,
        } => doctor::run(cfg, &only, strict, offline, printer).await,
        Command::Config { action } => config::run(action, printer),
        Command::Update => err("update"),
        Command::Supervise {
            dir,
            java,
            jar,
            ram,
        } => {
            // 隐藏入口：server start 拉起的守护进程本体，判定在 runtime
            let launch = mcsup::runtime::MinecraftLaunch {
                server_dir: dir,
                java_bin: java,
                jar_path: jar,
                ram_mib: ram,
            };
            let code = tokio::task::spawn_blocking(move || mcsup::runtime::run_supervisor(&launch))
                .await
                .context("运行 supervisor")?;
            if code == 0 {
                Ok(())
            } else {
                Err(anyhow::anyhow!("supervisor 异常退出（退出码 {code}）"))
            }
        }
        Command::Completions { shell } => crate::completions::generate(shell),
        Command::Version => print_version(printer),
    }
}

/// `mcsup version`：版本、构建与协议信息
///
/// 与 `mcsup --version`（只打 `mcsup <ver>`）是两条通道，后者被 CI 与脚本
/// 按行精确比对，改动这里不影响它。
fn print_version(printer: Printer) -> Result<()> {
    let payload = mcsup::protocol::cli::VersionInfoJson {
        name: "mcsup",
        version: env!("CARGO_PKG_VERSION"),
        target: target_triple(),
        profile: PROFILE,
        protocol_version: mcsup::protocol::PROTOCOL_VERSION,
    };
    if printer.is_human() {
        let mut table = comfy_table::Table::new();
        table.set_header(["项", "值"]);
        table.add_row(["程序", payload.name]);
        table.add_row(["版本", payload.version]);
        table.add_row(["目标平台", payload.target]);
        table.add_row(["构建", payload.profile]);
        table.add_row(["协议", &payload.protocol_version.to_string()]);
        crate::output::print_table(&table);
        return Ok(());
    }
    printer.print_outcome("", &payload)
}

/// 构建目标描述
///
/// 交叉编译（CI 的 `--target`）时由外部注入 `TARGET`；本地构建没有该变量，
/// 退回 `os-arch`，不冒充完整三元组，也不输出 `unknown`。
fn target_triple() -> &'static str {
    static LOCAL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    option_env!("TARGET").unwrap_or_else(|| {
        LOCAL.get_or_init(|| format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH))
    })
}

/// 编译配置
const PROFILE: &str = if cfg!(debug_assertions) {
    "debug"
} else {
    "release"
};

async fn java(action: JavaAction, printer: Printer) -> Result<()> {
    match action {
        JavaAction::List => java::list(printer),
        JavaAction::Scan => java::scan(printer),
        JavaAction::Install {
            version,
            jre,
            vendor,
        } => java::install(version.as_deref(), jre, vendor.as_deref(), printer).await,
        JavaAction::Uninstall { target, dir } => {
            java::uninstall(target.as_deref(), dir.as_deref(), printer)
        }
    }
}

async fn server(action: ServerAction, cfg: &ConfigFile, printer: Printer) -> Result<()> {
    match action {
        ServerAction::List => server_ops::list(printer),
        ServerAction::Create {
            name,
            version,
            variant,
            workspace,
        } => {
            server_ops::create(
                name.as_deref(),
                version.as_deref(),
                variant.as_deref(),
                workspace.as_deref(),
                printer,
            )
            .await
        }
        ServerAction::Install { name, version } => {
            install::run(&name, version.as_deref(), cfg, printer).await
        }
        ServerAction::Start { name, accept_eula } => {
            lifecycle::start(&name, accept_eula, printer).await
        }
        ServerAction::Stop { name, grace, force } => lifecycle::stop(&name, grace, force, printer),
        ServerAction::Restart {
            name,
            accept_eula,
            grace,
            force,
        } => lifecycle::restart(&name, accept_eula, grace, force, printer).await,
        ServerAction::Status { name } => lifecycle::status(&name, printer),
        ServerAction::Console { name } => lifecycle::console(&name, printer),
        ServerAction::Send { name, command } => lifecycle::send(&name, &command, printer),
        ServerAction::Logs { name, tail, follow } => lifecycle::logs(&name, tail, follow, printer),
        ServerAction::Props { name, kv } => props::run(&name, &kv, printer),
        ServerAction::Set { name, key, value } => {
            settings::set(&name, key.as_deref(), value.as_deref(), printer)
        }
        ServerAction::Remove { name, purge } => server_ops::remove(&name, purge, printer),
        ServerAction::Link {
            ref path,
            ref name,
            ref workspace,
        } => server_ops::link(path, name.as_deref(), workspace.as_deref(), printer),
        ServerAction::Version { name } => settings::version(&name, printer),
        ServerAction::Backup { .. } => err("backup"),
        ServerAction::Mod { .. } => err("mod"),
    }
}

fn err(what: &str) -> Result<()> {
    Err(AppDiagnostic::not_implemented(what).into())
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
