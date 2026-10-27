mod backup;
mod config;
mod doctor;
mod install;
mod java;
mod lifecycle;
mod modpack;
mod origin;
mod props;
mod remote;
mod server_ops;
mod settings;
mod workspace;

use crate::cli::{BackupAction, Cli, Command, JavaAction, ServerAction};
use crate::config::ConfigFile;
use crate::diagnostic::AppDiagnostic;
use crate::locale::Lang;
use crate::output::Printer;
use anyhow::{Context, Result};
use std::path::Path;

pub(crate) async fn run(cli: Cli, cfg: &ConfigFile, lang: Lang) -> Result<()> {
    // 全局 `-y` 必须随 Printer 一起进入命令层：确认式提问（删除、首次信任
    // 指纹）统一读 `assumes_yes()`，在这里丢掉会让 `-y` 对命令完全失效。
    let printer = Printer::new(cli.output, lang).with_yes(cli.yes);
    match cli.command {
        Command::Workspace { action } => workspace::run(action, printer),
        Command::Server { action } => server(action, cfg, printer).await,
        Command::Java { action } => java(action, printer).await,
        Command::Remote { action } => remote::run(action, printer),
        Command::Doctor { only, strict } => doctor::run(cfg, &only, strict, printer).await,
        Command::Config { action } => config::run(action, printer),
        Command::Update => err("update"),
        Command::Supervise {
            dir,
            java,
            jar,
            arg_file,
            server_arg,
            ram,
        } => {
            // 隐藏入口：server start 拉起的守护进程本体，判定在 runtime。
            // `--jar` 与 `--arg-file` 二选一：前者单 JAR，后者参数文件串。
            // `--server-arg` 是两种形态共用的程序参数（`nogui`），由调用方
            // （`LocalSupervisor`）从 `LaunchSpec::program_args` 逐项传来。
            let launch = match (jar, arg_file.is_empty()) {
                (Some(jar), true) => domain_launch_jar(jar, server_arg),
                (None, false) => domain_launch_args(arg_file, server_arg),
                // 两者都给或都不给：入口参数错误，明确报出而不是猜
                _ => {
                    return Err(anyhow::anyhow!(
                        "__supervise 需要且仅需要 --jar 或至少一个 --arg-file"
                    ));
                }
            };
            let launch = mcsup::runtime::MinecraftLaunch {
                server_dir: dir,
                java_bin: java,
                launch,
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
        JavaAction::List { origin } => {
            let filter = origin::filter(origin.as_deref(), printer.lang())?;
            java::list(&filter, printer)
        }
        JavaAction::Scan { origin } => {
            if let Some(origin) = origin {
                let origin = mcsup::port::Origin::parse(&origin).map_err(|err| {
                    AppDiagnostic::usage(
                        crate::i18n::pick(
                            printer.lang(),
                            format!("--origin 取值非法: {origin}"),
                            format!("invalid --origin value: {origin}"),
                        ),
                        Some(err.to_string()),
                    )
                })?;
                if !origin.is_local() {
                    return java::scan_remote(&origin, printer);
                }
            }
            java::scan(printer)
        }
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

#[allow(clippy::too_many_lines)]
async fn server(action: ServerAction, cfg: &ConfigFile, printer: Printer) -> Result<()> {
    match action {
        ServerAction::List { origin } => {
            let filter = origin::filter(origin.as_deref(), printer.lang())?;
            server_ops::list(&filter, printer)
        }
        ServerAction::Create {
            name,
            origin,
            version,
            variant,
            workspace,
            workspace_name,
            workspace_dir,
            modpack,
        } => {
            server_ops::create(
                server_ops::CreateArgs {
                    name: name.as_deref(),
                    origin: origin.as_deref(),
                    version: version.as_deref(),
                    variant: variant.as_deref(),
                    workspace: workspace.as_deref(),
                    workspace_name: workspace_name.as_deref(),
                    workspace_dir: workspace_dir.as_deref(),
                    modpack: modpack.as_deref(),
                },
                printer,
            )
            .await
        }
        ServerAction::Install { name, version } => {
            let name = server_ops::require_server_name(name.as_deref(), "server install", printer)?;
            install::run(&name, version.as_deref(), cfg, printer).await
        }
        ServerAction::Start { name, accept_eula } => {
            let name = server_ops::require_server_name(name.as_deref(), "server start", printer)?;
            lifecycle::start(&name, accept_eula, printer).await
        }
        ServerAction::Stop { name, grace, force } => {
            let name = server_ops::require_server_name(name.as_deref(), "server stop", printer)?;
            lifecycle::stop(&name, grace, force, printer)
        }
        ServerAction::Restart {
            name,
            accept_eula,
            grace,
            force,
        } => {
            let name = server_ops::require_server_name(name.as_deref(), "server restart", printer)?;
            lifecycle::restart(&name, accept_eula, grace, force, printer).await
        }
        ServerAction::Status { name } => {
            let name = server_ops::require_server_name(name.as_deref(), "server status", printer)?;
            lifecycle::status(&name, printer)
        }
        ServerAction::Console { name } => {
            let name = server_ops::require_server_name(name.as_deref(), "server console", printer)?;
            lifecycle::console(&name, printer)
        }
        ServerAction::Send { name, command } => {
            let name = server_ops::require_server_name(name.as_deref(), "server send", printer)?;
            let command = server_ops::require_text(
                command,
                "server send 缺少要注入的命令",
                "非交互模式：mcsup server send <name> \"<命令>\"",
                printer,
            )?;
            lifecycle::send(&name, &command, printer)
        }
        ServerAction::Logs { name, tail, follow } => {
            let name = server_ops::require_server_name(name.as_deref(), "server logs", printer)?;
            lifecycle::logs(&name, tail, follow, printer)
        }
        ServerAction::Props { name, kv } => {
            let name = server_ops::require_server_name(name.as_deref(), "server props", printer)?;
            props::run(&name, &kv, printer)
        }
        ServerAction::Set { name, key, value } => {
            let name = server_ops::require_server_name(name.as_deref(), "server set", printer)?;
            settings::set(&name, key.as_deref(), value.as_deref(), printer)
        }
        ServerAction::Remove {
            name,
            purge,
            origin,
        } => {
            let name = server_ops::require_server_name(name.as_deref(), "server remove", printer)?;
            if let Some(origin) = origin {
                let origin = mcsup::port::Origin::parse(&origin).map_err(|err| {
                    AppDiagnostic::usage(
                        crate::i18n::pick(
                            printer.lang(),
                            format!("--origin 取值非法: {origin}"),
                            format!("invalid --origin value: {origin}"),
                        ),
                        Some(err.to_string()),
                    )
                })?;
                if !origin.is_local() {
                    return server_ops::remove_remote(&origin, &name, purge, printer);
                }
            }
            server_ops::remove(&name, purge, printer)
        }
        ServerAction::Link {
            ref path,
            ref name,
            ref workspace,
        } => server_ops::link(path, name.as_deref(), workspace.as_deref(), printer),
        ServerAction::Version { name } => {
            let name = server_ops::require_server_name(name.as_deref(), "server version", printer)?;
            settings::version(&name, printer)
        }
        ServerAction::Backup { action } => backup(action, printer).await,
        ServerAction::Mod { .. } => err("mod"),
    }
}

fn err(what: &str) -> Result<()> {
    Err(AppDiagnostic::not_implemented(what).into())
}

/// 单 JAR 启动规格（`__supervise --jar`）
fn domain_launch_jar(
    jar: std::path::PathBuf,
    server_args: Vec<String>,
) -> mcsup::domain::server::launch::LaunchSpec {
    mcsup::domain::server::launch::LaunchSpec::Jar { path: jar }.with_program_args(server_args)
}

/// 参数文件启动规格（`__supervise --arg-file ...`）
fn domain_launch_args(
    files: Vec<std::path::PathBuf>,
    server_args: Vec<String>,
) -> mcsup::domain::server::launch::LaunchSpec {
    mcsup::domain::server::launch::LaunchSpec::args(files).with_program_args(server_args)
}

async fn backup(action: BackupAction, printer: Printer) -> Result<()> {
    match action {
        BackupAction::Create {
            name,
            label,
            scope,
            no_reuse,
        } => {
            let name =
                server_ops::require_server_name(name.as_deref(), "server backup create", printer)?;
            backup::create(&name, label.as_deref(), scope.as_deref(), no_reuse, printer).await
        }
        BackupAction::List {
            name,
            page,
            page_size,
        } => {
            let name =
                server_ops::require_server_name(name.as_deref(), "server backup list", printer)?;
            backup::list(&name, page, page_size, printer)
        }
        BackupAction::Restore { name, id, to } => {
            let name =
                server_ops::require_server_name(name.as_deref(), "server backup restore", printer)?;
            backup::restore(&name, id.as_deref(), to.as_deref(), printer).await
        }
        BackupAction::Delete { name, id } => {
            let name =
                server_ops::require_server_name(name.as_deref(), "server backup delete", printer)?;
            backup::delete(&name, id.as_deref(), printer)
        }
        BackupAction::Verify { name, id } => {
            let name =
                server_ops::require_server_name(name.as_deref(), "server backup verify", printer)?;
            backup::verify(&name, id.as_deref(), printer)
        }
        BackupAction::Prune {
            name,
            keep,
            older_than,
            dry_run,
        } => {
            let name =
                server_ops::require_server_name(name.as_deref(), "server backup prune", printer)?;
            backup::prune(&name, keep, older_than.as_deref(), dry_run, printer)
        }
    }
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
