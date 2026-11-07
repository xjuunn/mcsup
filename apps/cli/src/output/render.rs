//! 输出原语：真正往 stdout / stderr 写字节的那几个函数
//!
//! 只放「怎么写」，不放「写什么」：文案与分级在 [`super::error`] 与
//! [`crate::i18n`]，本模块不认识任何业务类型。

use comfy_table::Table;
use console::measure_text_width;
use owo_colors::{OwoColorize, Stream};

/// 标签列与值列之间的固定间隔
pub(crate) const LABEL_GAP: usize = 2;

/// 「标签列 + 值列」对齐的一段文本
///
/// 中文标签是双宽字符，补空格按显示宽度而不是字符数算，否则「ID」和
/// 「游戏版本」后面的值会错开。不画表格边框，也不加冒号：这种块总是紧接在
/// 版本选择 / 确认提问之前出现，边框只会挤占宽度。
pub(crate) fn render_fields(rows: &[(&str, String)]) -> String {
    let width = rows
        .iter()
        .map(|(label, _)| measure_text_width(label))
        .max()
        .unwrap_or(0);
    rows.iter()
        .map(|(label, value)| {
            let pad = width
                .saturating_sub(measure_text_width(label))
                .saturating_add(LABEL_GAP);
            format!("{label}{}{value}", " ".repeat(pad))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

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
