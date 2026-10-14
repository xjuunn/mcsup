mod install;

use crate::cli::{Cli, Command, ServerAction};
use crate::config::Config;
use crate::diagnostic::AppDiagnostic;
use crate::output::Printer;
use anyhow::{Ok, Result};

pub(crate) async fn run(cli: Cli, cfg: &Config) -> Result<()> {
    let printer = Printer::new(cli.output);
    match cli.command {
        Command::Server { action } => server(action, cfg, printer).await,
        Command::Modpack { .. } => err("modpack"),
        Command::Java { .. } => err("java"),
        Command::Remote { .. } => err("remote"),
        Command::Init { .. } => err("init"),
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

async fn server(action: ServerAction, cfg: &Config, printer: Printer) -> Result<()> {
    match action {
        ServerAction::List => err("list"),
        ServerAction::Create { .. } => err("create"),
        ServerAction::Install { version, dir } => {
            install::run(&version, dir.as_deref(), cfg, printer).await
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
        ServerAction::Remove { .. } => err("remove"),
        ServerAction::Version { .. } => err("version"),
        ServerAction::Backup { .. } => err("backup"),
        ServerAction::Mod { .. } => err("mod"),
    }
}

fn err(what: &str) -> Result<()> {
    Err(AppDiagnostic::not_implemented(what).into())
}
