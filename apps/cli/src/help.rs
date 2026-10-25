//! 帮助 / 版本输出的本地化
//!
//! clap 没有内建 i18n：`Usage:` / `Commands:` / `Options:` / `Arguments:` 这些
//! 标题、内建 `-h` / `-V` 的说明、以及自动生成的 `help` 子命令说明都是硬编码
//! 英文字符串，没有扩展点可改。逐条子命令的说明走各自的 `arg(help = ...)`，
//! 日志级别说明走 [`crate::verbosity::ZhLevel`]，由作者直接写中文。
//!
//! 本模块只对 clap 渲染后的帮助文本做一次成对替换：替换表以外保持原样，
//! 退出码与输出流向都不变。clap 升级改动英文文案时，`tests/cli_contract.rs`
//! 的帮助中文化用例会先失败。

use crate::cli::Cli;
use crate::locale::Lang;
use clap::Parser;
use clap::error::ErrorKind;
use std::io::{IsTerminal, Write};

/// clap 硬编码文案 → 中文
///
/// 顺序有讲究：长句排在会匹配其前缀的短句之前。
const ZH: &[(&str, &str)] = &[
    // clap 在存在 long_help 时会换成这个提示
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
        "打印此信息或指定子命令的帮助信息",
    ),
    ("Print help", "打印帮助信息"),
    ("Print version", "打印版本信息"),
    // 各类用法分组标题
    ("Usage:", "用法:"),
    ("Commands:", "命令:"),
    ("Arguments:", "参数:"),
    ("Options:", "选项:"),
    // 可选取值注释
    ("[default: ", "[默认: "),
    ("[possible values: ", "[可选值: "),
];

/// 帮助替换表，按语言选取；英文保持 clap 原文
fn replacements(lang: Lang) -> &'static [(&'static str, &'static str)] {
    match lang {
        Lang::Zh => ZH,
        Lang::En => &[],
    }
}

/// 解析命令行；帮助 / 版本输出现已本地化，错误仍走 clap 默认
pub(crate) fn parse() -> Cli {
    match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => exit_with(&err),
    }
}

fn exit_with(err: &clap::Error) -> ! {
    // 用法错误仍交给 clap 打印到 stderr 并以 2 退出
    if !matches!(
        err.kind(),
        ErrorKind::DisplayHelp
            | ErrorKind::DisplayVersion
            | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    ) {
        err.exit();
    }

    let text = localize(&render(err), Lang::detect_help(), replacements);
    // 输出可能被管道截断（如 `mcsup --help | head`），写失败不算错误
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

/// 帮助渲染时终端是否支持 ANSI，与 clap 默认的 Auto 一致
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

fn localize(
    text: &str,
    lang: Lang,
    replacements: fn(Lang) -> &'static [(&'static str, &'static str)],
) -> String {
    let mut out = text.to_string();
    for (from, to) in replacements(lang) {
        out = out.replace(from, to);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zh(raw: &str) -> String {
        localize(raw, Lang::Zh, replacements)
    }

    /// 帮助文本里不再出现 clap 的英文标题与内建说明
    #[test]
    fn localize_replaces_clap_copy() {
        let raw = "Usage: mcsup <COMMAND>\n\nCommands:\n  help  Print this message or the help of the given subcommand(s)\n\nOptions:\n  -h, --help  Print help\n  -V, --version  Print version\n";
        let text = zh(raw);
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

    /// `Print help (see more with '--help')` 不能被 "Print help" 截断成半截
    #[test]
    fn localize_keeps_long_help_hint() {
        let text = zh("Print help (see more with '--help')");
        assert_eq!(text, "打印帮助（用 --help 查看完整说明）");
    }

    /// 英文模式原样返回，不做任何替换
    #[test]
    fn english_passes_through() {
        let raw = "Usage: mcsup <COMMAND>\n\nOptions:\n  -V, --version  Print version\n";
        assert_eq!(localize(raw, Lang::En, replacements), raw);
    }
}
