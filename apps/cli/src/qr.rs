//! 终端二维码渲染
//!
//! 安装向导的交互式 setup 字段由插件提供二维码**内容**（登录链接），宿主负责
//! 编码与排版。二维码从行首开始排版，也不依赖插件进程的 stdout 编码设置；
//! 插件只把事件写进 UI 文件，见 `commands::plugin`。
//!
//! # 为什么默认用半块字符
//!
//! 半块字符把上下两个模块压进一个字符格：终端字符格约 1 列宽、2 行高，模块
//! 在两个方向上都占一个格，仍然是正方形。它只占色块排版的四分之一面积（宽高
//! 各减半），一屏就能完整容下，用户不用滚动就能扫。
//!
//! 色块排版（每模块 2 字符格宽、1 行高）深浅完全由背景色表达，不经过字形，
//! 任何字体都无缝。代价是面积是半块排版的四倍，二维码常常高过一屏。只有已知
//! 半块字形之间有细缝的终端（macOS Terminal，缝会把二维码切成横条，手机定位
//! 不到三个定位图案）才退回色块排版。
//!
//! 纠错等级取 L：屏幕上没有污损与折叠，模块数最少，整幅也最小。

use qrcode::{Color, EcLevel, QrCode};

/// 左右静默区宽度（模块数）：优先 2，终端放不下时退到 1
const QUIET: usize = 2;
/// 上下静默区高度（模块数，半块排版下两行模块合成一行文本）
const QUIET_ROWS: usize = 2;
/// 深色模块的字符格背景
const DARK_BG: &str = "\x1b[40m";
/// 浅色模块的字符格背景
const LIGHT_BG: &str = "\x1b[47m";

/// 终端一行的可用列数
///
/// `console::Term::size_checked()` 返回的是 `(行, 列)`，顺序容易写反：取错成
/// 行数会让二维码被判成放不下，整幅都不显示。
#[must_use]
pub(crate) fn terminal_columns() -> Option<usize> {
    console::Term::stdout()
        .size_checked()
        .map(columns_from_size)
}

/// `console` 的尺寸元组是 `(行, 列)`，列数在第二位
fn columns_from_size((_rows, columns): (u16, u16)) -> usize {
    usize::from(columns)
}

/// 渲染二维码，从行首开始
///
/// `columns` 是终端一行的可用字符数（未知时传 `None`），用于挑选不会折行的
/// 排版。返回 `None` 表示内容超出 QR 规范上限，或终端窄到任何排版都会折行。
#[must_use]
pub(crate) fn render(content: &str, columns: Option<usize>) -> Option<String> {
    render_with(content, columns, terminal_has_glyph_seams())
}

/// 已知半块字形有细缝的终端：退回色块排版，宁可尺寸大也要能扫
fn terminal_has_glyph_seams() -> bool {
    use std::ffi::OsStr;
    std::env::var_os("TERM_PROGRAM").as_deref() == Some(OsStr::new("Apple_Terminal"))
}

/// 按字形是否无缝选择排版，便于测试
///
/// 半块排版最省地方，始终优先。只有 `seamed` 为真时才用色块排版；此时如果
/// 色块排版也放不下，就没有更窄的选择。
fn render_with(content: &str, columns: Option<usize>, seamed: bool) -> Option<String> {
    let code = QrCode::with_error_correction_level(content.as_bytes(), EcLevel::L).ok()?;
    let size = code.width();
    let colors = code.to_colors();
    if !seamed {
        for quiet in [QUIET, 1] {
            if fits(columns, half_blocks_width(size, quiet)) {
                return Some(render_half_blocks(&colors, size, quiet));
            }
        }
        tracing::debug!(
            size,
            ?columns,
            "QR does not fit the terminal; falling back to the link"
        );
        return None;
    }
    for quiet in [QUIET, 1] {
        if fits(columns, cells_width(size, quiet)) {
            return Some(render_cells(&colors, size, quiet));
        }
    }
    tracing::debug!(
        size,
        ?columns,
        "QR does not fit the terminal; falling back to the link"
    );
    None
}

/// 终端宽度够不够；未知宽度按够用处理
fn fits(columns: Option<usize>, width: usize) -> bool {
    columns.is_none_or(|limit| width <= limit)
}

/// 色块排版宽度：每模块 2 个字符格
fn cells_width(size: usize, quiet: usize) -> usize {
    (size + quiet * 2) * 2
}

/// 半块排版宽度：每模块 1 个字符格
fn half_blocks_width(size: usize, quiet: usize) -> usize {
    size + quiet * 2
}

/// 色块排版：每模块 2 个字符格宽、1 行高，深浅由背景色填满字符格
///
/// `quiet` 只收窄左右静默区；上下固定 [`QUIET_ROWS`] 行，不占宽度。
fn render_cells(colors: &[Color], size: usize, quiet: usize) -> String {
    let line_width = cells_width(size, quiet);
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

/// 半块排版：两行模块合成一行文本，宽高都只有色块排版的一半
fn render_half_blocks(colors: &[Color], size: usize, quiet: usize) -> String {
    let line_width = half_blocks_width(size, quiet);
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
    use super::{QUIET_ROWS, render_with};
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

    fn weixin_link() -> &'static str {
        "https://liteapp.weixin.qq.com/q/7GiQu1?qrcode=4ffab43bb683afbf4818b1c3365685ff&bot_type=3"
    }

    /// 每行都从行首开始（以 CSI 序列开头），不插入居中缩进
    #[test]
    fn starts_every_line_at_the_left_margin() {
        let rendered = render_with("https://example.com/weixin", Some(200), false).expect("render");
        for line in rendered.lines() {
            assert!(line.starts_with('\x1b'), "应从行首的样式序列开始：{line:?}");
        }
    }

    /// 默认用半块排版：宽度只有模块数，高度只有模块数的一半
    #[test]
    fn default_layout_is_half_blocks() {
        let content = weixin_link();
        let size = code_size(content);
        let rendered = render_with(content, Some(200), false).expect("render");
        let lines = rendered.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), size.div_ceil(2) + QUIET_ROWS);
        for line in lines {
            let text = visible(line);
            assert_eq!(text.chars().count(), size + 4, "宽度等于模块数加静默区");
            assert!(
                text.chars().all(|ch| " █▀▄".contains(ch)),
                "只应出现静默区与半块字符：{text:?}"
            );
        }
    }

    /// 已知字形有缝的终端退回色块排版：每行两个静默行 + 模块行
    #[test]
    fn seamed_terminal_uses_solid_cells() {
        let content = weixin_link();
        let size = code_size(content);
        let rendered = render_with(content, Some(200), true).expect("render");
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

    /// 半块排版放不下时先收窄静默区到 1 模块
    #[test]
    fn narrow_terminal_shrinks_the_quiet_zone() {
        let content = weixin_link();
        let size = code_size(content);
        let columns = size + 2;
        let rendered = render_with(content, Some(columns), false).expect("render");
        for line in rendered.lines() {
            assert_eq!(visible(line).chars().count(), columns, "正好铺满一行");
        }
    }

    /// 色块排版同样在放不下时收窄静默区
    #[test]
    fn seamed_terminal_shrinks_the_quiet_zone() {
        let content = weixin_link();
        let size = code_size(content);
        let columns = (size + 2) * 2;
        let rendered = render_with(content, Some(columns), true).expect("render");
        for line in rendered.lines() {
            assert_eq!(visible(line).chars().count(), columns, "正好铺满一行");
        }
    }

    /// 比最窄排版还窄：返回 None，由调用方给出链接
    #[test]
    fn terminal_too_narrow_yields_none() {
        let content = "https://example.com/weixin";
        let size = code_size(content);
        // 半块排版最窄时每模块 1 列、静默区 1 模块
        assert!(render_with(content, Some(size + 1), false).is_none());
        // 色块排版最窄时每模块 2 列、静默区 1 模块
        assert!(render_with(content, Some((size + 2) * 2 - 1), true).is_none());
        // 未知宽度按够用处理
        assert!(render_with(content, None, false).is_some());
        assert!(render_with(content, None, true).is_some());
    }

    /// `console` 的尺寸元组是 `(行, 列)`：80×24 的终端一行 80 列
    #[test]
    fn terminal_size_tuple_is_rows_then_columns() {
        assert_eq!(super::columns_from_size((24, 80)), 80);
        assert_eq!(super::columns_from_size((40, 132)), 132);
    }
}
