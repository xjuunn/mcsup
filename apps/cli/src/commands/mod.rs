mod install;
mod java;
mod server_ops;
mod workspace;

use crate::cli::{Cli, Command, JavaAction, ServerAction};
use crate::config::Config;
use crate::diagnostic::AppDiagnostic;
use crate::output::Printer;
use anyhow::{Ok, Result};

pub(crate) async fn run(cli: Cli, cfg: &Config) -> Result<()> {
    let printer = Printer::new(cli.output);
    match cli.command {
        Command::Workspace { action } => workspace::run(action, printer),
        Command::Server { action } => server(action, cfg, printer).await,
        Command::Modpack { .. } => err("modpack"),
        Command::Java { action } => java(action, printer).await,
        Command::Remote { .. } => err("remote"),
        Command::Doctor => err("doctor"),
        Command::Config { .. } => {
            printer.print_section(&format!("配置: {}", cfg.path.display()));
            Ok(())
        }
        Command::Update => err("update"),
        Command::Completions { shell } => crate::completions::generate(shell),
        Command::Version => {
            printer.print_section(&format!("mcsup {}", env!("CARGO_PKG_VERSION")));
            Ok(())
        }
    }
}

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

async fn server(action: ServerAction, cfg: &Config, printer: Printer) -> Result<()> {
    match action {
        ServerAction::List => server_ops::list(printer),
        ServerAction::Create {
            name,
            version,
            variant,
            workspace,
        } => server_ops::create(
            &name,
            &version,
            variant.as_deref(),
            workspace.as_deref(),
            printer,
        ),
        ServerAction::Install { name, version } => {
            install::run(&name, version.as_deref(), cfg, printer).await
        }
        ServerAction::Start { .. } => err("start"),
        ServerAction::Stop { .. } => err("stop"),
        ServerAction::Restart { .. } => err("restart"),
        ServerAction::Status { .. } => err("status"),
        ServerAction::Console { .. } => err("console"),
        ServerAction::Send { .. } => err("send"),
        ServerAction::Logs { .. } => err("logs"),
        ServerAction::Props { .. } => err("props"),
        ServerAction::Set { .. } => err("set"),
        ServerAction::Remove { name, purge } => server_ops::remove(&name, purge, printer),
        ServerAction::Link {
            ref path,
            ref name,
            ref workspace,
        } => server_ops::link(path, name.as_deref(), workspace.as_deref(), printer),
        ServerAction::Version { .. } => err("version"),
        ServerAction::Backup { .. } => err("backup"),
        ServerAction::Mod { .. } => err("mod"),
    }
}

fn err(what: &str) -> Result<()> {
    Err(AppDiagnostic::not_implemented(what).into())
}
