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

/// 人类可读字节数：`1.50 GB` / `12.0 MB` / `512 B`
///
/// 按国际单位制 10 进制换算（`1 KB = 1000 B`），不是 1024 进制的 KiB/MiB/GiB。
/// 与硬盘厂商、下载工具的口径一致：`1 MiB` 的字节数在这里显示为 `1.05 MB`。
#[allow(clippy::cast_precision_loss)]
pub(crate) fn human_bytes(n: u64) -> String {
    const KB: u64 = 1_000;
    const MB: u64 = 1_000 * 1_000;
    const GB: u64 = 1_000 * 1_000 * 1_000;
    if n >= GB {
        format!("{:.2} GB", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{:.2} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.2} KB", n as f64 / KB as f64)
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

#[cfg(test)]
mod tests {
    use super::human_bytes;

    /// 按 10 进制换算，不使用 KiB/MiB/GiB 的 1024 进制
    #[test]
    fn human_bytes_uses_decimal_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1_000), "1.00 KB");
        assert_eq!(human_bytes(1_500), "1.50 KB");
        assert_eq!(human_bytes(1_000_000), "1.00 MB");
        assert_eq!(human_bytes(641_250_000), "641.25 MB");
        assert_eq!(human_bytes(1_000_000_000), "1.00 GB");
    }

    /// 旧口径下 `641.25 MiB` 的字节数，按真 SI 应显示为 `672.40 MB`
    ///
    /// 641.25 MiB = 641.25 × 1024² = 672,399,360 字节；换算到 10 进制即 672.40 MB。
    #[test]
    fn human_bytes_converts_binary_sizes_to_decimal() {
        assert_eq!(human_bytes(672_399_360), "672.40 MB");
        assert_eq!(human_bytes(1_048_576), "1.05 MB", "1 MiB = 1.048576 MB");
        assert_eq!(human_bytes(1_073_741_824), "1.07 GB", "1 GiB = 1.073 GB");
    }
}
