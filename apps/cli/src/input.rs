//! 输入源适配：把原始输入折算成语义动作，平台差异止步于本模块
//!
//! 当前来源是 `server console` 的 stdin 行模式（保留系统行编辑与 IME）：
//! Windows conhost 与 Unix canonical 模式都把控制字符直接放进整行（回显
//! `^A` 之类），所以快捷键只能从行内数据识别，控制字符 → 键名的折算
//! 在这里完成。
//!
//! raw mode + crossterm 事件源属于 TUI 阶段：届时在本模块加事件 →
//! [`Chord`] 的转换，[`crate::keymap`] 的键位表与判定不变。

use crate::keymap::{self, Action, Chord};

/// 找出行内第一个已绑定的快捷键，返回它之前要注入服务端的文本
///
/// 未绑定的控制字符按普通文本处理并继续向后扫描：`Ctrl+A` 这类尚未配置
/// 的按键不会吞掉整行，也不会抢走行里随后出现的 `Ctrl+D`。
pub(crate) fn split_action(line: &str) -> (&str, Option<Action>) {
    let mut from = 0;
    while let Some((offset, ch)) = line[from..].char_indices().find(|(_, c)| c.is_control()) {
        let index = from + offset;
        from = index + ch.len_utf8();
        let action = control_letter(ch)
            .map(Chord::Ctrl)
            .and_then(keymap::resolve);
        if let Some(action) = action {
            return (&line[..index], Some(action));
        }
    }
    (line, None)
}

/// 控制字符 → 语义键名：`0x01 → 'a'`、`0x04 → 'd'`、`0x1A → 'z'`
///
/// CR / LF / Tab 不折算：它们在事件源口径里是 Enter / Tab 键而不是
/// `Ctrl+M` / `Ctrl+I`，折算过去会让键位表和 TUI 阶段的口径打架；
/// 这些字符继续按文本走。
fn control_letter(c: char) -> Option<char> {
    let code = c as u32;
    if !(0x01..=0x1A).contains(&code) || matches!(c, '\r' | '\n' | '\t') {
        return None;
    }
    char::from_u32(u32::from(b'a') + code - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 行内第一个绑定键折算成动作，并保留它之前的注入文本
    #[test]
    fn splits_bound_control_key_and_keeps_prefix() {
        assert_eq!(split_action("\u{4}\r\n"), ("", Some(Action::Detach)));
        assert_eq!(
            split_action("stop\r\n\u{4}"),
            ("stop\r\n", Some(Action::Detach))
        );
        assert_eq!(
            split_action("stop\u{4}more\n"),
            ("stop", Some(Action::Detach))
        );
        assert_eq!(split_action("\u{1a}\r\n"), ("", Some(Action::Detach)));
        assert_eq!(split_action("stop\r\n"), ("stop\r\n", None));
        assert_eq!(split_action("stop"), ("stop", None));
    }

    /// 未绑定的控制字符不吞行：整行照常作为文本，且不抢走后续绑定键
    #[test]
    fn unbound_control_chars_stay_text() {
        assert_eq!(
            split_action("a\u{1}b\u{4}"),
            ("a\u{1}b", Some(Action::Detach))
        );
        assert_eq!(split_action("a\u{1}b"), ("a\u{1}b", None));
        assert_eq!(split_action("cmd\t\r\n"), ("cmd\t\r\n", None));
    }

    /// 控制字符 → 键名的折算口径，且 CR / LF / Tab 不折算
    #[test]
    fn control_chars_map_to_key_names() {
        assert_eq!(control_letter('\u{1}'), Some('a'));
        assert_eq!(control_letter('\u{4}'), Some('d'));
        assert_eq!(control_letter('\u{1a}'), Some('z'));
        assert_eq!(control_letter('\r'), None);
        assert_eq!(control_letter('\n'), None);
        assert_eq!(control_letter('\t'), None);
        assert_eq!(control_letter('x'), None);
        assert_eq!(control_letter('\u{7}'), Some('g'));
    }
}
