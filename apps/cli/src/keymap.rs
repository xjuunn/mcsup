//! 快捷键语义层：按键 → 动作，与输入来源无关
//!
//! 各入口只把按键折算成 [`Chord`]，由 [`resolve`] 统一判定成 [`Action`]：
//! `server console` 的 stdin 行输入与选择器的 raw mode 按键（[`crate::input`]）
//! 都走这条判定，入口只消费动作。新增键位只改 [`BINDINGS`] 一行，入口与
//! 平台适配不再动。
//!
//! `Ctrl+C` 不在本表：它由终端信号 / 协作取消路径处理（退出码 130），
//! 在这里再绑一个动作会出现两套行为。

/// 一次按键：与输入来源无关的键表示（不泄漏 crossterm 类型）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Chord {
    /// `Ctrl+<键名>`，键名是语义字母（如 `0x04` 折算成 `'d'`）
    Ctrl(char),
}

/// 快捷键对应的业务动作
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    /// 分离控制台，服务器继续运行
    Detach,
    /// 切换选择器的显示范围（默认候选 ↔ 全部候选）
    ToggleScope,
}

/// 键位表：一行一个绑定
const BINDINGS: &[(Chord, Action)] = &[
    (Chord::Ctrl('d'), Action::Detach),      // Unix EOF（行首 Ctrl+D）
    (Chord::Ctrl('z'), Action::Detach),      // Windows EOF（Ctrl+Z 回车）
    (Chord::Ctrl('a'), Action::ToggleScope), // 切换选择范围（raw mode 选择器）
];

/// 判定按键属于哪个动作；未绑定返回 `None`（调用方按普通输入处理）
#[must_use]
pub(crate) fn resolve(chord: Chord) -> Option<Action> {
    BINDINGS
        .iter()
        .find(|(bound, _)| *bound == chord)
        .map(|(_, action)| *action)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 表驱动：绑定的键出动作，没绑的键不出动作
    #[test]
    fn bindings_resolve_to_actions() {
        assert_eq!(resolve(Chord::Ctrl('d')), Some(Action::Detach));
        assert_eq!(resolve(Chord::Ctrl('z')), Some(Action::Detach));
        assert_eq!(resolve(Chord::Ctrl('a')), Some(Action::ToggleScope));
        assert_eq!(resolve(Chord::Ctrl('c')), None);
        assert_eq!(resolve(Chord::Ctrl('x')), None);
        assert_eq!(resolve(Chord::Ctrl('\r')), None);
    }
}
