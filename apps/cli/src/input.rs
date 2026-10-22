//! 输入源适配：把原始输入折算成语义动作 / 按键，平台差异止步于本模块
//!
//! 两个来源共用 [`crate::keymap`] 的键位表：
//!
//! - `server console` 的 stdin 行模式（保留系统行编辑与 IME）：Windows conhost
//!   与 Unix canonical 模式都把控制字符直接放进整行（回显 `^A` 之类），所以
//!   快捷键只能从行内数据识别，折算见 [`split_action`]。
//! - 选择器的 raw mode 逐键事件：crossterm 在三个平台上都折算成
//!   `Ctrl+<字母>` 的按键事件，由 [`key_from_event`] 转成 [`Key`]，动作判定
//!   仍走 [`keymap::resolve`]。
//!
//! raw mode 的进入与恢复也在这里（[`RawMode`]），提示组件不直接碰终端。

use crate::keymap::{self, Action, Chord};
use anyhow::{Context, Result};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// 一次按键：raw mode 事件源折算出的语义按键
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Key {
    /// 可打印字符
    Char(char),
    /// 退格（删除光标前字符）
    Backspace,
    /// Delete（删除光标后字符）
    Delete,
    /// 光标左移
    Left,
    /// 光标右移
    Right,
    /// 光标到行首
    Home,
    /// 光标到行尾
    End,
    /// 上一个建议
    Up,
    /// 下一个建议
    Down,
    /// 上一页建议
    PageUp,
    /// 下一页建议
    PageDown,
    /// 补全当前建议
    Tab,
    /// 提交
    Enter,
    /// 取消（Esc）
    Esc,
    /// `Ctrl+C`：raw mode 下不产生终端信号，由提示按取消处理
    Interrupt,
    /// 控制键组合；是否触发动作由 [`crate::keymap`] 判定
    Chord(Chord),
}

/// raw mode 会话：进入 raw mode，`Drop` 时恢复原始终端状态
///
/// 逐键读取依赖 raw mode（关闭回显、行缓冲与终端信号），退出路径必须恢复，
/// 否则后续输入失去行编辑。`Ctrl+C` 在此期间不产生信号，由 [`Key::Interrupt`]
/// 承接。
#[derive(Debug)]
pub(crate) struct RawMode;

impl RawMode {
    /// 进入 raw mode
    ///
    /// # Errors
    ///
    /// 标准输入不是终端或系统调用失败时返回底层错误
    pub(crate) fn enter() -> Result<Self> {
        crossterm::terminal::enable_raw_mode().context("进入 raw mode 失败")?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    #[allow(clippy::print_stderr)]
    fn drop(&mut self) {
        if let Err(err) = crossterm::terminal::disable_raw_mode() {
            // 恢复失败无补救：终端保持 raw mode，但不能中断调用方的退出路径
            eprintln!("恢复终端模式失败: {err}");
        }
    }
}

/// 阻塞读取一个语义按键：忽略 resize / 鼠标 / 按键释放等非按键事件
///
/// # Errors
///
/// 终端读取失败时返回底层错误
pub(crate) fn read_key() -> Result<Key> {
    loop {
        let event = crossterm::event::read().context("读取按键失败")?;
        // 按下与长按触发；释放事件忽略（Windows 会成对上报，忽略它
        // 才能保证 Ctrl+A 一次按下只判定一次）
        if let Event::Key(key) = event
            && !matches!(key.kind, KeyEventKind::Release)
            && let Some(key) = key_from_event(key)
        {
            return Ok(key);
        }
    }
}

/// 按键事件 → 语义按键；界面用不上的按键返回 `None`（调用方继续读）
#[must_use]
pub(crate) fn key_from_event(key: KeyEvent) -> Option<Key> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        // raw mode 关掉了终端信号，Ctrl+C 只能由提示自己按取消处理
        KeyCode::Char('c' | 'C') if ctrl => Some(Key::Interrupt),
        KeyCode::Char(ch) if ctrl => Some(Key::Chord(Chord::Ctrl(ch.to_ascii_lowercase()))),
        KeyCode::Char(ch) => Some(Key::Char(ch)),
        KeyCode::Backspace => Some(Key::Backspace),
        KeyCode::Delete => Some(Key::Delete),
        KeyCode::Left => Some(Key::Left),
        KeyCode::Right => Some(Key::Right),
        KeyCode::Home => Some(Key::Home),
        KeyCode::End => Some(Key::End),
        KeyCode::Up => Some(Key::Up),
        KeyCode::Down => Some(Key::Down),
        KeyCode::PageUp => Some(Key::PageUp),
        KeyCode::PageDown => Some(Key::PageDown),
        KeyCode::Tab => Some(Key::Tab),
        KeyCode::Enter => Some(Key::Enter),
        KeyCode::Esc => Some(Key::Esc),
        _ => None,
    }
}

/// 找出行内第一个已绑定的快捷键，返回它之前要注入服务端的文本
///
/// 未绑定的控制字符按普通文本处理并继续向后扫描：`Ctrl+B` 这类尚未配置
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
        // Ctrl+A 绑定了切换范围，同样从行内折算出来
        assert_eq!(split_action("\u{1}\r\n"), ("", Some(Action::ToggleScope)));
        assert_eq!(split_action("stop\r\n"), ("stop\r\n", None));
        assert_eq!(split_action("stop"), ("stop", None));
    }

    /// 未绑定的控制字符不吞行：整行照常作为文本，且不抢走后续绑定键
    #[test]
    fn unbound_control_chars_stay_text() {
        assert_eq!(
            split_action("a\u{2}b\u{4}"),
            ("a\u{2}b", Some(Action::Detach))
        );
        assert_eq!(split_action("a\u{2}b"), ("a\u{2}b", None));
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

    /// 事件源折算：可打印字符、光标键与功能键各归各位
    #[test]
    fn event_keys_map_to_semantic_keys() {
        use crossterm::event::KeyModifiers;

        let press = |code, modifiers| KeyEvent::new(code, modifiers);
        assert_eq!(
            key_from_event(press(KeyCode::Char('a'), KeyModifiers::NONE)),
            Some(Key::Char('a'))
        );
        assert_eq!(
            key_from_event(press(KeyCode::Char('中'), KeyModifiers::SHIFT)),
            Some(Key::Char('中'))
        );
        assert_eq!(
            key_from_event(press(KeyCode::Backspace, KeyModifiers::NONE)),
            Some(Key::Backspace)
        );
        assert_eq!(
            key_from_event(press(KeyCode::Up, KeyModifiers::NONE)),
            Some(Key::Up)
        );
        assert_eq!(
            key_from_event(press(KeyCode::Esc, KeyModifiers::NONE)),
            Some(Key::Esc)
        );
        assert_eq!(
            key_from_event(press(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Key::Enter)
        );
        assert_eq!(
            key_from_event(press(KeyCode::Tab, KeyModifiers::SHIFT)),
            Some(Key::Tab)
        );
        // 用不上的按键不折算（F 键、鼠标、粘贴等由调用方继续读）
        assert_eq!(
            key_from_event(press(KeyCode::F(5), KeyModifiers::NONE)),
            None
        );
    }

    /// 控制键折算：Ctrl+字母 → 键位表用的 Chord（字母统一小写），Ctrl+C 单独
    /// 走取消
    #[test]
    fn control_keys_become_chords_or_interrupt() {
        use crossterm::event::KeyModifiers;

        let press = |code, modifiers| KeyEvent::new(code, modifiers);
        assert_eq!(
            key_from_event(press(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            Some(Key::Chord(Chord::Ctrl('a')))
        );
        assert_eq!(
            key_from_event(press(KeyCode::Char('A'), KeyModifiers::CONTROL)),
            Some(Key::Chord(Chord::Ctrl('a')))
        );
        assert_eq!(
            key_from_event(press(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            Some(Key::Chord(Chord::Ctrl('d')))
        );
        assert_eq!(
            key_from_event(press(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Key::Interrupt)
        );
        assert_eq!(
            key_from_event(press(KeyCode::Char('c'), KeyModifiers::NONE)),
            Some(Key::Char('c'))
        );
    }
}
