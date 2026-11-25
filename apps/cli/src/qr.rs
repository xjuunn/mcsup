//! 终端二维码渲染
//!
//! 安装向导的交互式 setup 字段由插件提供二维码**内容**（登录链接），宿主负责
//! 编码与排版。二维码从行首开始排版，也不依赖插件进程的 stdout 编码设置；
//! 插件只把事件写进 UI 文件，见 `commands::plugin`。
//!
//! 用上下半块把两行模块合成一行文本：终端字符格约 1×2（宽×高），半行正好
//! 对应一个模块，图案不会被拉长。纠错等级取 L，屏幕上没有污损与折叠，模块数
//! 最少，整幅也最小。静默区缩到 2 模块——每多一格就是一圈额外的白色边带，
//! 2 格仍能让手机稳定定位，但不会把安装向导撑成一整屏白底。

use qrcode::{Color, EcLevel, QrCode};

/// 静默区宽度（模块数）
const QUIET: usize = 2;

/// 渲染二维码，从行首开始
///
/// 返回 `None` 表示内容超出 QR 规范上限。
#[must_use]
pub(crate) fn render(content: &str) -> Option<String> {
    let code = QrCode::with_error_correction_level(content.as_bytes(), EcLevel::L).ok()?;
    let size = code.width();
    let colors = code.to_colors();
    let line_width = size + QUIET * 2;
    // 静默区与模块共用白底：整幅二维码是一块连续的浅色底板，边缘不留透明缝
    let quiet = " ".repeat(QUIET);
    let quiet_row = format!("\x1b[30;47m{}\x1b[0m\n", " ".repeat(line_width));
    let quiet_lines = QUIET.div_ceil(2);

    let mut out = String::new();
    for _ in 0..quiet_lines {
        out.push_str(&quiet_row);
    }
    for y in (0..size).step_by(2) {
        out.push_str("\x1b[30;47m");
        out.push_str(&quiet);
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
        out.push_str(&quiet);
        out.push_str("\x1b[0m\n");
    }
    for _ in 0..quiet_lines {
        out.push_str(&quiet_row);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::{QUIET, render};
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

    /// 每行都从行首开始，不插入居中缩进
    #[test]
    fn starts_every_line_at_the_left_margin() {
        let rendered = render("https://example.com/weixin").expect("render");
        for line in rendered.lines() {
            assert_eq!(
                line.chars().take_while(|ch| *ch == ' ').count(),
                0,
                "不应有前导缩进：{line:?}"
            );
        }
    }

    /// 静默区只有 2 模块，整体宽度与行数都可从模块数直接算出
    #[test]
    fn layout_is_a_two_module_quiet_zone_around_half_blocks() {
        let content = "https://example.com/weixin";
        let size = QrCode::with_error_correction_level(content.as_bytes(), EcLevel::L)
            .expect("encode")
            .width();
        let width = size + QUIET * 2;
        let rendered = render(content).expect("render");
        let lines = rendered.lines().collect::<Vec<_>>();

        assert_eq!(lines.len(), QUIET.div_ceil(2) * 2 + size.div_ceil(2));
        for line in lines {
            let text = visible(line);
            assert_eq!(text.chars().count(), width, "每行可见宽度一致：{text:?}");
            assert!(
                text.chars().all(|ch| " █▀▄".contains(ch)),
                "只应出现静默区与半块字符：{text:?}"
            );
        }
    }
}
