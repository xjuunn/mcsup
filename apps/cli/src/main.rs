mod bootstrap;
mod cli;
mod commands;
mod completions;
mod config;
mod diagnostic;
mod output;
mod progress;
mod verbosity;

use clap::Parser;
use std::{env, process::ExitCode};

#[tokio::main]
async fn main() -> ExitCode {
    let _ = miette::set_hook(Box::new(
        |_| Box::new(miette::GraphicalReportHandler::new()),
    ));
    let cli = cli::Cli::parse();
    verbosity::init(cli.verbose);
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "启动");

    let cfg = match config::Config::load(cli.config.as_deref()) {
        Ok(cfg) => cfg,
        Err(err) => {
            output::Printer::render_error(err);
            return ExitCode::from(1);
        }
    };

    match commands::run(cli, &cfg).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            let code = output::Printer::exit_code(&err);
            output::Printer::render_error(err);
            ExitCode::from(code)
        }
    }
}
