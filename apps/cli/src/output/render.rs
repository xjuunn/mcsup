//! 输出原语：真正往 stdout / stderr 写字节的那几个函数
//!
//! 只放「怎么写」，不放「写什么」：文案与分级在 [`super::error`] 与
//! [`crate::i18n`]，本模块不认识任何业务类型。

use comfy_table::Table;
use owo_colors::{OwoColorize, Stream};

/// 人类可读字节数：`1.50 GiB` / `12.0 MiB` / `512 B`
#[allow(clippy::cast_precision_loss)]
pub(crate) fn human_bytes(n: u64) -> String {
    const KIB: u64 = 1 << 10;
    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;
    if n >= GIB {
        format!("{:.2} GiB", n as f64 / GIB as f64)
    } else if n >= MIB {
        format!("{:.2} MiB", n as f64 / MIB as f64)
    } else if n >= KIB {
        format!("{:.2} KiB", n as f64 / KIB as f64)
    } else {
        format!("{n} B")
    }
}

#[allow(clippy::print_stdout)]
pub(crate) fn print_table(table: &Table) {
    println!("{table}");
}

#[allow(clippy::print_stdout)]
pub(crate) fn print_styled(text: &str) {
    // `bold()` 无条件写 ANSI，管道里会冒出转义序列；`if_supports_color` 才按
    // 流是否终端 + 全局 override 决定
    println!(
        "{}",
        text.if_supports_color(Stream::Stdout, OwoColorize::bold)
    );
}

#[allow(clippy::print_stdout)]
pub(crate) fn print_success(text: &str) {
    println!(
        "{} {text}",
        "+".if_supports_color(Stream::Stdout, OwoColorize::green)
    );
}

#[allow(clippy::print_stdout)]
pub(crate) fn print_json_line(line: &str) {
    println!("{line}");
}

#[allow(clippy::print_stderr)]
pub(crate) fn print_error_line(text: &str) {
    eprintln!("{text}");
}
