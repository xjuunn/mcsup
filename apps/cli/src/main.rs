mod bootstrap;
mod cli;
mod commands;
mod completions;
mod config;
mod diagnostic;
mod help;
mod i18n;
mod input;
mod keymap;
mod output;
mod progress;
mod prompts;
mod verbosity;

use std::{env, process::ExitCode};

#[tokio::main]
async fn main() -> ExitCode {
    install_panic_hook();
    let _ = miette::set_hook(Box::new(
        |_| Box::new(miette::GraphicalReportHandler::new()),
    ));
    let cli = help::parse();
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
            // 130 只有「用户取消」一条来路（Ctrl-C 协作取消），中间文件已由
            // 下层静默清理完：不打印错误，直接以取消码退出
            if code != 130 {
                output::Printer::render_error(err);
            }
            ExitCode::from(code)
        }
    }
}

/// release 用 `panic = "abort"` 压体积，展开被关闭后 [`input::RawMode`] 的
/// `Drop` 不会执行。恐慌退出前先恢复终端，避免把用户留在 raw mode。
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::terminal::disable_raw_mode();
        default_hook(info);
    }));
}
