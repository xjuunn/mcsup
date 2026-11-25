//! 终端二维码渲染
//!
//! 安装向导的交互式 setup 字段由插件提供二维码**内容**（登录链接），宿主负责
//! 编码与排版。二维码从行首开始排版，也不依赖插件进程的 stdout 编码设置；
//! 插件只把事件写进 UI 文件，见 `commands::plugin`。
//!
//! # 为什么用背景色块而不是半块字符
//!
//! 上下半块（`▀` / `▄`）能把两行模块压进一行文本，宽度只有色块排版的一半，
//! 但字形不一定填满字符格：macOS Terminal 里相邻行的半块之间留有细缝，缝把
//! 二维码切成一条条横线，手机定位不到三个定位图案，扫不出来。
//!
//! 色块排版让每个模块占 2×1 个字符格（终端字符格约 1×2，模块因此是正方形），
//! 深浅完全由背景色表达，不经过字形，所以没有缝。代价是宽度翻倍：终端放不下
//! 时先把静默区从 2 模块收到 1 模块，仍放不下才退回半块排版（宽度只有一半，
//! 字形能无缝拼接的终端仍可扫）。
//!
//! 纠错等级取 L：屏幕上没有污损与折叠，模块数最少，整幅也最小。

use qrcode::{Color, EcLevel, QrCode};

/// 左右静默区宽度（模块数）：优先 2，终端放不下时退到 1
const QUIET: usize = 2;
/// 上下静默区高度（模块数，色块排版下一行文本就是一个模块）
const QUIET_ROWS: usize = 2;
/// 深色模块的字符格背景
const DARK_BG: &str = "\x1b[40m";
/// 浅色模块的字符格背景
const LIGHT_BG: &str = "\x1b[47m";

/// 渲染二维码，从行首开始
///
/// `columns` 是终端一行的可用字符数（未知时传 `None`），用于挑选不会折行的
/// 排版。返回 `None` 表示内容超出 QR 规范上限，或终端窄到任何排版都会折行。
#[must_use]
pub(crate) fn render(content: &str, columns: Option<usize>) -> Option<String> {
    let code = QrCode::with_error_correction_level(content.as_bytes(), EcLevel::L).ok()?;
    let size = code.width();
    let colors = code.to_colors();
    // 色块排版：每模块 2 个字符格宽
    for quiet in [QUIET, 1] {
        if fits(columns, (size + quiet * 2) * 2) {
            return Some(render_cells(&colors, size, quiet));
        }
    }
    // 半块排版只占一半宽度；字形能无缝拼接的终端仍可扫
    if fits(columns, size + QUIET * 2) {
        return Some(render_half_blocks(&colors, size, QUIET));
    }
    None
}

/// 终端宽度够不够；未知宽度按够用处理
fn fits(columns: Option<usize>, width: usize) -> bool {
    columns.is_none_or(|limit| width <= limit)
}

/// 色块排版：每模块 2 个字符格宽、1 行高，深浅由背景色填满字符格
///
/// `quiet` 只收窄左右静默区；上下固定 [`QUIET_ROWS`] 行，不占宽度。
fn render_cells(colors: &[Color], size: usize, quiet: usize) -> String {
    let line_width = (size + quiet * 2) * 2;
    let quiet_row = format!("\x1b[47m{}\x1b[0m\n", " ".repeat(line_width));
    let pad = " ".repeat(quiet * 2);
    let mut out = String::new();
    for _ in 0..QUIET_ROWS {
        out.push_str(&quiet_row);
    }
    for y in 0..size {
        out.push_str(LIGHT_BG);
        out.push_str(&pad);
        for x in 0..size {
            let dark = colors[y * size + x] == Color::Dark;
            out.push_str(if dark { DARK_BG } else { LIGHT_BG });
            out.push_str("  ");
        }
        out.push_str(LIGHT_BG);
        out.push_str(&pad);
        out.push_str("\x1b[0m\n");
    }
    for _ in 0..QUIET_ROWS {
        out.push_str(&quiet_row);
    }
    out
}

/// 半块排版：两行模块合成一行文本，宽度是色块排版的一半
fn render_half_blocks(colors: &[Color], size: usize, quiet: usize) -> String {
    let line_width = size + quiet * 2;
    let pad = " ".repeat(quiet);
    let quiet_row = format!("\x1b[30;47m{}\x1b[0m\n", " ".repeat(line_width));
    let quiet_lines = QUIET_ROWS.div_ceil(2);
    let mut out = String::new();
    for _ in 0..quiet_lines {
        out.push_str(&quiet_row);
    }
    for y in (0..size).step_by(2) {
        out.push_str("\x1b[30;47m");
        out.push_str(&pad);
        for x in 0..size {
            let top = colors[y * size + x] == Color::Dark;
            let bottom = y + 1 < size && colors[(y + 1) * size + x] == Color::Dark;
            out.push(match (top, bottom) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        out.push_str(&pad);
        out.push_str("\x1b[0m\n");
    }
    for _ in 0..quiet_lines {
        out.push_str(&quiet_row);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{QUIET_ROWS, render};
    use qrcode::{EcLevel, QrCode};

    /// 去掉 CSI 颜色序列，只留终端里实际占位的字符
    fn visible(line: &str) -> String {
        let mut out = String::new();
        let mut chars = line.chars();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(ch);
            }
        }
        out
    }

    fn code_size(content: &str) -> usize {
        QrCode::with_error_correction_level(content.as_bytes(), EcLevel::L)
            .expect("encode")
            .width()
    }

    /// 每行都从行首开始（以 CSI 序列开头），不插入居中缩进
    #[test]
    fn starts_every_line_at_the_left_margin() {
        let rendered = render("https://example.com/weixin", Some(200)).expect("render");
        for line in rendered.lines() {
            assert!(line.starts_with('\x1b'), "应从行首的样式序列开始：{line:?}");
        }
    }

    /// 宽终端用色块排版：每行两个静默行 + 模块行，宽度是模块数的两倍
    #[test]
    fn wide_terminal_uses_solid_cells() {
        let content = "https://liteapp.weixin.qq.com/q/7GiQu1?qrcode=4ffab43bb683afbf4818b1c3365685ff&bot_type=3";
        let size = code_size(content);
        let rendered = render(content, Some(200)).expect("render");
        let lines = rendered.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), size + QUIET_ROWS * 2);
        for line in lines {
            let text = visible(line);
            // 色块排版的可见字符只有背景色上的空格
            assert_eq!(text.chars().count(), (size + 4) * 2, "宽度按两倍模块算");
            assert!(
                text.chars().all(|ch| ch == ' '),
                "色块排版只有空格：{text:?}"
            );
        }
    }

    /// 80 列终端放不下 2 模块静默区时收到 1 模块，仍然用色块排版
    #[test]
    fn narrow_terminal_shrinks_the_quiet_zone() {
        let content = "https://liteapp.weixin.qq.com/q/7GiQu1?qrcode=4ffab43bb683afbf4818b1c3365685ff&bot_type=3";
        let size = code_size(content);
        // 一行能放 1 模块静默区但放不下 2 模块
        let columns = (size + 2) * 2;
        let rendered = render(content, Some(columns)).expect("render");
        for line in rendered.lines() {
            assert_eq!(visible(line).chars().count(), columns, "正好铺满一行");
        }
        assert!(rendered.contains('\x1b'), "仍是色块排版");
    }

    /// 连 1 模块静默区都放不下时退回半块排版，宽度减半
    #[test]
    fn very_narrow_terminal_falls_back_to_half_blocks() {
        let content = "https://liteapp.weixin.qq.com/q/7GiQu1?qrcode=4ffab43bb683afbf4818b1c3365685ff&bot_type=3";
        let size = code_size(content);
        let columns = size + 4;
        let rendered = render(content, Some(columns)).expect("render");
        let lines = rendered.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), size.div_ceil(2) + QUIET_ROWS);
        for line in lines {
            let text = visible(line);
            assert_eq!(text.chars().count(), columns, "半块排版宽度减半");
            assert!(
                text.chars().all(|ch| " █▀▄".contains(ch)),
                "只应出现静默区与半块字符：{text:?}"
            );
        }
    }

    /// 比半块排版还窄：返回 None，由调用方给出链接
    #[test]
    fn terminal_too_narrow_yields_none() {
        let content = "https://example.com/weixin";
        let size = code_size(content);
        assert!(render(content, Some(size + 3)).is_none());
        // 未知宽度按够用处理
        assert!(render(content, None).is_some());
    }
}
