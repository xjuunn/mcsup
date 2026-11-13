//! 输出原语：真正往 stdout / stderr 写字节的那几个函数
//!
//! 只放「怎么写」，不放「写什么」：文案与分级在 [`super::error`] 与
//! [`crate::i18n`]，本模块不认识任何业务类型。

use comfy_table::{ContentArrangement, Table, TableStyle};
use console::measure_text_width;
use owo_colors::{OwoColorize, Stream};

/// 标签列与值列之间的固定间隔
pub(crate) const LABEL_GAP: usize = 2;

/// 紧凑表格样式：不画外框、列分隔与横线
///
/// 表格的职责是对齐列，不是画格子。`+----+` 与 `|` 既占宽度，又会随中文
/// 双宽字符对不齐——框线按字符数排、内容按显示宽度排，两者一旦不一致，线就
/// 会跑到表格外面去。列多的时候这种错位比没有框更难看。
///
/// 表头下也不留横线：动态排版下它按终端宽度铺满，比内容长出好几截，反而
/// 是最显眼的错位。表头靠首行位置区分就够了。
const COMPACT: TableStyle = TableStyle::new();

/// 建一张紧凑的无边框表格
pub(crate) fn compact_table() -> Table {
    let mut table = Table::new();
    table.load_style(COMPACT);
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table
}

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

/// 无修饰打印一行：不加粗、不变色。用于列表项这类次级信息，
/// 与 [`print_styled`]（标题）形成对比。
#[allow(clippy::print_stdout)]
pub(crate) fn print_line(text: &str) {
    println!("{text}");
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
