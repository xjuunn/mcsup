//! 终端二维码渲染
//!
//! 安装向导的交互式 setup 字段由插件提供二维码**内容**（登录链接），宿主负责
//! 编码与排版。这样二维码始终按当前终端宽度居中，也不依赖插件进程的 stdout
//! 编码设置；插件只把事件写进 UI 文件，见 `commands::plugin`。
//!
//! 用上下半块把两行模块合成一行文本，并固定黑字白底：深色终端主题下也能被
//! 手机识别。静默区按规范保留 4 个模块。

use qrcode::{Color, QrCode};

/// 静默区宽度（模块数）
const QUIET: usize = 4;

/// 渲染二维码并加前导缩进，使它在 `columns` 宽的终端里居中
///
/// 返回 `None` 表示内容超出 QR 规范上限；终端太窄时不做缩进，整幅照常输出，
/// 让用户至少看得到完整图像。
#[must_use]
pub(crate) fn render_centered(content: &str, columns: usize) -> Option<String> {
    let code = QrCode::new(content.as_bytes()).ok()?;
    let size = code.width();
    let colors = code.to_colors();
    let line_width = size + QUIET * 2;
    let indent = " ".repeat(columns.saturating_sub(line_width) / 2);
    let quiet_row = format!("{indent}\x1b[30;47m{}\x1b[0m", " ".repeat(line_width));
    let quiet_lines = QUIET.div_ceil(2);

    let mut out = String::new();
    for _ in 0..quiet_lines {
        out.push_str(&quiet_row);
        out.push('\n');
    }
    for y in (0..size).step_by(2) {
        out.push_str(&indent);
        out.push_str("\x1b[30;47m");
        out.push_str(&" ".repeat(QUIET));
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
        out.push_str(&" ".repeat(QUIET));
        out.push_str("\x1b[0m\n");
    }
    for _ in 0..quiet_lines {
        out.push_str(&quiet_row);
        out.push('\n');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::render_centered;

    /// 二维码每行都是等宽文本，居中靠统一前导空格实现
    #[test]
    fn centers_every_line_by_the_same_offset() {
        let rendered = render_centered("https://example.com/weixin", 120).expect("render");
        let offsets = rendered
            .lines()
            .map(|line| line.chars().take_while(|ch| *ch == ' ').count())
            .collect::<Vec<_>>();
        assert!(offsets[0] > 0, "宽终端应产生居中缩进");
        assert!(
            offsets.windows(2).all(|pair| pair[0] == pair[1]),
            "每行缩进必须一致：{offsets:?}"
        );
    }

    /// 终端比二维码窄时不能丢掉图案本身
    #[test]
    fn keeps_content_when_terminal_is_narrow() {
        let rendered = render_centered("https://example.com/weixin", 10).expect("render");
        assert!(rendered.lines().all(|line| !line.starts_with(' ')));
        assert!(rendered.contains('█') || rendered.contains('▀') || rendered.contains('▄'));
    }
}
