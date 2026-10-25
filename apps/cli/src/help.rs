//! 帮助 / 版本输出的中文化
//!
//! clap 没有 i18n：`Usage:` / `Commands:` / `Options:` / `Arguments:` 这些标题、
//! 内置 `-h` / `-V` 的说明，以及自动生成的 `help` 子命令说明都是硬编码字符串，
//! 没有扩展点可改。参数自身的说明走各命令的 `arg(help = ...)`，日志级别说明走
//! [`crate::verbosity::ZhLevel`]，两者都不经过这里。
//!
//! 因此这里只对 clap 渲染出的帮助文本做一次文案替换，其余错误原样交给 clap，
//! 退出码与输出流保持不变。clap 升级改了文案时由 `tests/cli_contract.rs`
//! 的帮助用例兜底。

use crate::cli::Cli;
use clap::Parser;
use clap::error::ErrorKind;
use std::io::{IsTerminal, Write};

/// clap 硬编码文案 → 中文；长串必须排在它的前缀之前
const LOCALIZED: &[(&str, &str)] = &[
    // clap 在存在 long_help 时会换用带提示的措辞，先替换
    (
        "Print help (see more with '--help')",
        "打印帮助（用 --help 查看完整说明）",
    ),
    (
        "Print help (see a summary with '-h')",
        "打印帮助（用 -h 查看摘要）",
    ),
    (
        "Print help for the subcommand(s)",
        "打印指定子命令的帮助信息",
    ),
    (
        "Print this message or the help of the given subcommand(s)",
        "打印本帮助或指定子命令的帮助信息",
    ),
    ("Print help", "打印帮助信息"),
    ("Print version", "打印版本信息"),
    // 分组标题与用法标题
    ("Usage:", "用法:"),
    ("Commands:", "命令:"),
    ("Arguments:", "参数:"),
    ("Options:", "选项:"),
    // 参数取值注解
    ("[default: ", "[默认: "),
    ("[possible values: ", "[可选值: "),
];

/// 解析命令行：帮助 / 版本输出中文化，其余错误走 clap 默认处理
pub(crate) fn parse() -> Cli {
    match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => exit_with(&err),
    }
}

fn exit_with(err: &clap::Error) -> ! {
    // 用法错误照旧由 clap 打印到 stderr 并退 2
    if !matches!(
        err.kind(),
        ErrorKind::DisplayHelp
            | ErrorKind::DisplayVersion
            | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    ) {
        err.exit();
    }

    let text = localize(&render(err));
    // 帮助会经管道被截断（如 `mcsup --help | head`），写失败不算错误
    if err.use_stderr() {
        let mut stream = std::io::stderr();
        let _ = stream
            .write_all(text.as_bytes())
            .and_then(|()| stream.flush());
    } else {
        let mut stream = std::io::stdout();
        let _ = stream
            .write_all(text.as_bytes())
            .and_then(|()| stream.flush());
    }
    std::process::exit(err.exit_code());
}

/// 输出流是终端时保留 ANSI 样式，否则输出纯文本（对应 clap 默认的 Auto）
fn render(err: &clap::Error) -> String {
    let styled = err.render();
    let tty = if err.use_stderr() {
        std::io::stderr().is_terminal()
    } else {
        std::io::stdout().is_terminal()
    };
    if tty {
        styled.ansi().to_string()
    } else {
        styled.to_string()
    }
}

fn localize(text: &str) -> String {
    let mut out = text.to_string();
    for (from, to) in LOCALIZED {
        out = out.replace(from, to);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 帮助文本里不再残留 clap 的英文标题与内置说明
    #[test]
    fn localize_replaces_clap_copy() {
        let raw = "Usage: mcsup <COMMAND>\n\nCommands:\n  help  Print this message or the help of the given subcommand(s)\n\nOptions:\n  -h, --help  Print help\n  -V, --version  Print version\n";
        let text = localize(raw);
        for english in [
            "Usage:",
            "Commands:",
            "Options:",
            "Print help",
            "Print version",
            "Print this message",
        ] {
            assert!(!text.contains(english), "{english} 未替换:\n{text}");
        }
        assert!(text.contains("用法:"));
        assert!(text.contains("命令:"));
        assert!(text.contains("选项:"));
        assert!(text.contains("打印帮助信息"));
        assert!(text.contains("打印版本信息"));
    }

    /// `Print help (see more with '--help')` 不能被 "Print help" 截断成半句
    #[test]
    fn localize_keeps_long_help_hint() {
        let text = localize("Print help (see more with '--help')");
        assert_eq!(text, "打印帮助（用 --help 查看完整说明）");
    }
}
