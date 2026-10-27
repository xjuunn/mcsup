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
mod locale;
mod meta;
mod output;
mod progress;
mod prompts;
mod ssh;
mod verbosity;

use crate::config::ConfigFile;
use std::io::IsTerminal;
use std::{env, process::ExitCode};

#[tokio::main]
async fn main() -> ExitCode {
    install_panic_hook();
    let _ = miette::set_hook(Box::new(
        |_| Box::new(miette::GraphicalReportHandler::new()),
    ));
    let cli = help::parse();
    // 语言先于一切输出确定：帮助之外的提示都经 Printer 出去
    let lang = locale::detect(locale::LangChoice::parse(&cli.lang));
    let printer = output::Printer::new(cli.output, lang).with_yes(cli.yes);
    init_color(cli.output);
    verbosity::init(cli.verbose);
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "启动");

    let cfg = ConfigFile::locate(cli.config.as_deref());

    match commands::run(cli, &cfg, lang).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            let code = output::Printer::exit_code(&err);
            // 130 只有「用户取消」一条来路（Ctrl-C 协作取消），中间文件已由
            // 下层静默清理完：不打印错误，直接以取消码退出
            if code != 130 {
                printer.render_error(err);
            }
            ExitCode::from(code)
        }
    }
}

/// 着色开关：只在 stdout 是终端时启用
///
/// `owo_colors` 默认按全局条件判定，管道 / 重定向下仍会写 ANSI 转义，
/// 冒到 `config show | grep` 这类用法里。`--output json` 本就只剩一行数据，
/// 不参与着色。
fn init_color(output: output::OutputFormat) {
    let enabled = output == output::OutputFormat::Human
        && std::io::stdout().is_terminal()
        && env::var_os("NO_COLOR").is_none();
    owo_colors::set_override(enabled);
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
