//! 范围可切换的可过滤选择器
//!
//! 默认只列非 `extra` 候选，`Ctrl+A` 在「默认范围」与「全部候选」之间切换：
//! 按键读取与折算在 [`crate::input`]，动作判定在 [`crate::keymap`]，本模块
//! 只管状态与渲染。过滤、建议、补全与校验口径与 [`super::select_filter`] 一致。
//!
//! 渲染写 stderr（与 inquire 一致），stdout 只留数据；调用方必须先经
//! `printer.is_interactive()` 守卫。

use crate::input::{self, Key};
use crate::keymap::{self, Action};
use anyhow::{Context, Result};
use console::{Style, measure_text_width, truncate_str};
use crossterm::cursor::{MoveToColumn, MoveUp};
use crossterm::queue;
use crossterm::style::Print;
use crossterm::terminal::{Clear, ClearType, size as terminal_size};
use inquire::autocompletion::Replacement;
use inquire::{Autocomplete, CustomUserError, InquireError};
use std::io::{IsTerminal, Write};

use crate::locale::Lang;

use super::{FilterComplete, canonical_choice, err_empty_input, err_not_a_choice, filter_choices};

/// 建议列表每页条数（与 `select_filter` 的建议条数一致）
const PAGE_SIZE: usize = 10;

/// 输入区最少留出的列数（窄终端下先牺牲标签，也不能让输入区消失）
const MIN_INPUT_COLS: usize = 4;

/// 可过滤选择的候选：`extra` 为 `true` 的候选默认隐藏，切换范围后显示
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScopedChoice {
    /// 展示标签，同时是提交值（按全名大小写不敏感归一）
    pub(crate) label: String,
    /// 是否属于「切换范围后才显示」的扩展候选
    pub(crate) extra: bool,
}

impl ScopedChoice {
    #[must_use]
    pub(crate) fn new(label: impl Into<String>, extra: bool) -> Self {
        Self {
            label: label.into(),
            extra,
        }
    }
}

/// 范围切换的提示文案：措辞由调用方按业务给，组件只负责渲染
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScopeHint {
    /// 停在默认范围时展示：切换后能看到什么
    pub(crate) show_all: &'static str,
    /// 停在全部范围时展示：切换后会隐藏什么
    pub(crate) show_default: &'static str,
}

/// 收尾原因：只有这三种会结束交互
#[derive(Debug, Clone, PartialEq, Eq)]
enum Done {
    /// 提交（值为候选全名）
    Submitted(String),
    /// Esc 取消
    Cancelled,
    /// Ctrl+C 中断
    Interrupted,
}

/// 显示范围：默认只列非 `extra` 候选
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// 默认范围
    Default,
    /// 全部候选
    All,
}

/// 按范围筛出候选标签
fn pool_of(choices: &[ScopedChoice], scope: Scope) -> Vec<String> {
    choices
        .iter()
        .filter(|choice| scope == Scope::All || !choice.extra)
        .map(|choice| choice.label.clone())
        .collect()
}

/// 交互状态：纯数据 + 按键处理，不碰终端（读键与渲染在外层）
#[derive(Debug, Clone)]
struct Picker<'a> {
    /// 提示标题（渲染进输入行前缀）
    label: &'a str,
    /// 全部候选（范围切换时按 `extra` 重新筛）
    choices: &'a [ScopedChoice],
    /// 当前范围内的候选标签：过滤、建议、补全与校验共用这一份
    pool: Vec<String>,
    /// 当前显示范围
    scope: Scope,
    /// 范围切换提示文案
    hint: ScopeHint,
    /// 过滤关键字
    input: String,
    /// 光标：字符下标
    cursor: usize,
    /// 建议列表高亮下标（相对完整过滤结果）；`None` 表示未高亮（回车按输入文本校验）
    highlight: Option<usize>,
    /// 建议窗口起始下标：高亮超出窗口时随之滚动
    scroll: usize,
    /// 上一次提交失败的原因；输入变化即清
    error: Option<String>,
    /// 界面语言：帮助行与校验文案按它渲染
    lang: Lang,
}

impl<'a> Picker<'a> {
    fn new(lang: Lang, label: &'a str, choices: &'a [ScopedChoice], hint: ScopeHint) -> Self {
        // 没有非 extra 候选时直接开在全部，避免默认范围空列表
        let scope = if choices.iter().any(|choice| !choice.extra) {
            Scope::Default
        } else {
            Scope::All
        };
        let pool = pool_of(choices, scope);
        Self {
            label,
            choices,
            pool,
            scope,
            hint,
            input: String::new(),
            cursor: 0,
            highlight: None,
            scroll: 0,
            error: None,
            lang,
        }
    }

    /// 关键字过滤后的全部候选（不截断，滚动窗口见 [`Self::window`]）
    fn filtered(&self) -> Vec<String> {
        filter_choices(&self.pool, &self.input)
            .into_iter()
            .cloned()
            .collect()
    }

    /// 可见窗口 `[start, end)`：固定一页高，随高亮滚动
    ///
    /// `scroll` 只记录首条可见下标，这里再按当前候选数夹紧，
    /// 保证过滤结果变短时不会留下空窗。
    fn window(&self, len: usize) -> (usize, usize) {
        let max_start = len.saturating_sub(PAGE_SIZE);
        let start = self.scroll.min(max_start);
        (start, (start + PAGE_SIZE).min(len))
    }

    /// 让高亮保持在窗口内：越过上下边界时滚动一页
    fn ensure_visible(&mut self, len: usize) {
        let Some(index) = self.highlight else {
            return;
        };
        if index < self.scroll {
            self.scroll = index;
        } else if index >= self.scroll + PAGE_SIZE {
            self.scroll = index + 1 - PAGE_SIZE;
        }
        self.scroll = self.scroll.min(len.saturating_sub(PAGE_SIZE));
    }

    /// 切换显示范围：输入保留，候选集与高亮按新范围重算
    fn toggle_scope(&mut self) {
        self.scope = match self.scope {
            Scope::Default => Scope::All,
            Scope::All => Scope::Default,
        };
        self.pool = pool_of(self.choices, self.scope);
        self.highlight = None;
        self.scroll = 0;
        self.error = None;
    }

    /// 帮助行的范围提示：停在默认范围时讲切换后能看到什么，反之亦然
    fn scope_hint(&self) -> &'static str {
        match self.scope {
            Scope::Default => self.hint.show_all,
            Scope::All => self.hint.show_default,
        }
    }

    /// 处理一个按键：返回 `None` 表示继续交互
    ///
    /// # Errors
    ///
    /// 补全回调返回底层错误时返回
    fn handle(&mut self, key: Key) -> Result<Option<Done>> {
        match key {
            // 动作判定收敛在 keymap：只认「切换范围」，其余动作与未绑定组合吞掉
            Key::Chord(chord) => {
                if keymap::resolve(chord) == Some(Action::ToggleScope) {
                    self.toggle_scope();
                }
                Ok(None)
            }
            Key::Enter => Ok(self.submit()),
            Key::Esc => Ok(Some(Done::Cancelled)),
            Key::Interrupt => Ok(Some(Done::Interrupted)),
            other => {
                self.edit(other)?;
                Ok(None)
            }
        }
    }

    /// 编辑与导航：内容变化会重算建议并清掉高亮与错误，纯光标移动不碰高亮
    ///
    /// # Errors
    ///
    /// 补全回调返回底层错误时返回
    fn edit(&mut self, key: Key) -> Result<()> {
        let content_changed = match key {
            Key::Char(ch) => {
                let at = char_to_byte(&self.input, self.cursor);
                self.input.insert(at, ch);
                self.cursor += 1;
                true
            }
            Key::Backspace => {
                let removed = self.cursor > 0;
                if removed {
                    let end = char_to_byte(&self.input, self.cursor);
                    let start = char_to_byte(&self.input, self.cursor - 1);
                    self.input.drain(start..end);
                    self.cursor -= 1;
                }
                removed
            }
            Key::Delete => {
                let removed = self.cursor < self.input.chars().count();
                if removed {
                    let start = char_to_byte(&self.input, self.cursor);
                    let end = char_to_byte(&self.input, self.cursor + 1);
                    self.input.drain(start..end);
                }
                removed
            }
            Key::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                false
            }
            Key::Right => {
                self.cursor = (self.cursor + 1).min(self.input.chars().count());
                false
            }
            Key::Home => {
                self.cursor = 0;
                false
            }
            Key::End => {
                self.cursor = self.input.chars().count();
                false
            }
            Key::Up => {
                self.highlight_up(1);
                false
            }
            Key::Down => {
                self.highlight_down(1);
                false
            }
            Key::PageUp => {
                self.highlight_up(PAGE_SIZE);
                false
            }
            Key::PageDown => {
                self.highlight_down(PAGE_SIZE);
                false
            }
            // 补全可能改写内容，是否清高亮由它自己决定
            Key::Tab => {
                self.complete()?;
                false
            }
            // 控制流按键在 handle 里已分流，其余按键忽略
            _ => false,
        };
        if content_changed {
            self.content_changed();
        }
        Ok(())
    }

    /// 内容变化：建议随输入重算，高亮、滚动与上一次错误作废
    fn content_changed(&mut self) {
        self.highlight = None;
        self.scroll = 0;
        self.error = None;
    }

    /// 高亮上移：越到顶回到「未高亮」（回车改按输入文本校验）
    fn highlight_up(&mut self, qty: usize) {
        let len = self.filtered().len();
        if len == 0 {
            self.highlight = None;
            return;
        }
        self.highlight = match self.highlight {
            None => None,
            Some(index) => {
                if qty > index {
                    None
                } else {
                    Some(index - qty)
                }
            }
        };
        self.ensure_visible(len);
    }

    /// 高亮下移：从无到有取首条，到底停在末条；越过窗口即滚动
    fn highlight_down(&mut self, qty: usize) {
        let len = self.filtered().len();
        self.highlight = if len == 0 {
            None
        } else if qty == 0 {
            self.highlight
        } else {
            Some(
                match self.highlight {
                    None => qty - 1,
                    Some(index) => index + qty,
                }
                .min(len - 1),
            )
        };
        self.ensure_visible(len);
    }

    /// Tab 补全：高亮项优先，其次输入已是全名、单一匹配（口径同 `select_filter`）
    ///
    /// # Errors
    ///
    /// 补全回调返回底层错误时返回
    fn complete(&mut self) -> Result<()> {
        let highlighted = self
            .highlight
            .and_then(|index| self.filtered().get(index).cloned());
        let mut filter = FilterComplete {
            choices: self.pool.clone(),
        };
        let completed = Autocomplete::get_completion(&mut filter, &self.input, highlighted)
            .map_err(|err: CustomUserError| anyhow::anyhow!("补全选项失败: {err}"))?;
        if let Replacement::Some(value) = completed {
            self.input = value;
            self.cursor = self.input.chars().count();
            self.content_changed();
        }
        Ok(())
    }

    /// 回车：有高亮直接采纳；否则要求输入是候选全名（口径同 `select_filter`）
    fn submit(&mut self) -> Option<Done> {
        if let Some(index) = self.highlight {
            let filtered = self.filtered();
            if let Some(picked) = filtered.get(index) {
                return Some(Done::Submitted(picked.clone()));
            }
            // 候选变化让高亮失效：退回按输入文本校验
            self.highlight = None;
        }
        let input = self.input.trim();
        let matched = if input.is_empty() {
            None
        } else {
            canonical_choice(input, &self.pool).cloned()
        };
        if let Some(submitted) = matched {
            Some(Done::Submitted(submitted))
        } else {
            let message = if input.is_empty() {
                err_empty_input(self.lang)
            } else {
                err_not_a_choice(self.lang)
            };
            self.error = Some(message);
            None
        }
    }

    /// 一帧画面：行块与终端光标落点
    fn screen(&self) -> Screen {
        let max = max_line_cols();
        let mut lines = Vec::new();
        if let Some(error) = &self.error {
            lines.push(styled(error, &Style::new().for_stderr().red(), max));
        }
        let (input_line, column) = self.input_line(max);
        let row = lines.len();
        lines.push(input_line);
        let filtered = self.filtered();
        let (start, end) = self.window(filtered.len());
        for (index, suggestion) in filtered.iter().enumerate().take(end).skip(start) {
            lines.push(self.suggestion_line(suggestion, index, max));
        }
        let help = match self.lang {
            Lang::Zh => format!(
                "输入关键字过滤；↑↓ 选择，Tab 补全，回车确认，Esc 取消（Ctrl+A {}）",
                self.scope_hint()
            ),
            Lang::En => format!(
                "type to filter; ↑↓ move, Tab completes, Enter confirms, Esc cancels (Ctrl+A {})",
                self.scope_hint()
            ),
        };
        lines.push(styled(&help, &Style::new().for_stderr().dim(), max));
        Screen {
            lines,
            cursor: (row, column),
        }
    }

    /// 输入行：`标签 > 关键字`，并给出终端光标落点列
    ///
    /// 输入按尾部显示：关键字在尾部输入，超宽截头部不截尾部。
    fn input_line(&self, max: usize) -> (String, usize) {
        let mut prefix = format!("{} > ", self.label);
        let mut prefix_width = measure_text_width(&prefix);
        if prefix_width + MIN_INPUT_COLS > max {
            // 窄终端：先缩短标签，保住输入区
            "> ".clone_into(&mut prefix);
            prefix_width = 2;
        }
        let budget = max.saturating_sub(prefix_width);
        if budget == 0 {
            // 放不下输入区：只留标签，光标贴行尾
            return (styled(&prefix, &Style::new().for_stderr().cyan(), max), max);
        }
        let (visible, cut) = tail_fit(&self.input, budget);
        let cursor_byte = char_to_byte(&self.input, self.cursor).max(cut);
        let shown = measure_text_width(&self.input[cut..cursor_byte]);
        let column = prefix_width + usize::from(cut > 0) + shown;
        let line = format!(
            "{}{visible}",
            Style::new().for_stderr().cyan().apply_to(&prefix)
        );
        (line, column)
    }

    /// 建议行：高亮项带 `>` 标记并着色
    fn suggestion_line(&self, suggestion: &str, index: usize, max: usize) -> String {
        let highlighted = self.highlight == Some(index);
        let marker = if highlighted { ">" } else { " " };
        let text = format!("{marker} {suggestion}");
        if highlighted {
            styled(&text, &Style::new().for_stderr().cyan().bold(), max)
        } else {
            styled(&text, &Style::new().for_stderr(), max)
        }
    }
}

/// 一帧画面：行块与终端光标落点（`(行号, 列号)`，均相对块顶）
#[derive(Debug)]
struct Screen {
    lines: Vec<String>,
    cursor: (usize, usize),
}

/// 提示块：记住光标相对块顶的行号，重绘前回退到块顶再清屏
#[derive(Debug, Default)]
struct Frame {
    /// 终端光标相对块顶的行号（0 = 块顶）
    row: usize,
}

impl Frame {
    /// 重绘整块并把光标落到编辑位置；`lines` 为空即清屏
    ///
    /// 每行都被截到「终端宽度 - 1」列，保证一行只占一个终端行，
    /// 回退行数才不会被折行算错。
    ///
    /// # Errors
    ///
    /// 终端写入失败时返回底层错误
    fn draw(&mut self, lines: &[String], cursor: (usize, usize)) -> Result<()> {
        let mut out = std::io::stderr().lock();
        self.draw_to(&mut out, lines, cursor)
    }

    /// [`Self::draw`] 的内核；抽成泛型以便用内存缓冲断言字节序列
    ///
    /// # Errors
    ///
    /// 写入失败时返回底层错误
    fn draw_to<W: Write>(
        &mut self,
        out: &mut W,
        lines: &[String],
        cursor: (usize, usize),
    ) -> Result<()> {
        if self.row > 0 {
            queue!(out, MoveUp(as_u16(self.row))).context("重绘提示失败")?;
        }
        queue!(out, MoveToColumn(0), Clear(ClearType::FromCursorDown)).context("重绘提示失败")?;
        for (index, line) in lines.iter().enumerate() {
            if index > 0 {
                // 用 CR+LF 下移而不是 `MoveToNextLine`：后者是 CNL，只在光标
                // 范围内移动，到底行会被夹住不回卷，块尾几行会互相覆盖；LF 在
                // 底行触发滚屏，块贴近终端底部时才能整体显示。
                queue!(out, Print("\r\n")).context("重绘提示失败")?;
            }
            queue!(out, Print(line)).context("重绘提示失败")?;
        }
        let (target_row, column) = cursor;
        // 打印完最后一行时光标在相对行 `lines.len() - 1`，回退到 `target_row`
        // 只需上移二者之差；多退一行会落到块顶上方，逐帧把整块往上顶。
        let up = lines_up(lines.len(), target_row);
        if up > 0 {
            queue!(out, MoveUp(as_u16(up))).context("重绘提示失败")?;
        }
        queue!(out, MoveToColumn(as_u16(column))).context("重绘提示失败")?;
        out.flush().context("重绘提示失败")?;
        self.row = target_row;
        Ok(())
    }
}

/// 可过滤 + 范围切换的选择：默认只列非 `extra` 候选，`Ctrl+A` 切换范围
///
/// 单候选免提问直接返回；提交值是候选全名（大小写不敏感归一）。
///
/// # Errors
///
/// 无可选项、非交互终端、按键读取 / 终端写入失败，或用户 Esc取消 /
/// Ctrl+C 中断时返回错误
pub(crate) fn select_filter_scoped(
    lang: Lang,
    label: &str,
    choices: &[ScopedChoice],
    hint: ScopeHint,
) -> Result<String> {
    match choices {
        [] => {
            return Err(anyhow::anyhow!(match lang {
                Lang::Zh => "没有可选项",
                Lang::En => "no options available",
            }));
        }
        [only] => return Ok(only.label.clone()),
        _ => {}
    }
    if !std::io::stdin().is_terminal() {
        return Err(InquireError::NotTTY.into());
    }
    let mut picker = Picker::new(lang, label, choices, hint);
    let mut frame = Frame::default();
    let done = {
        // raw mode 覆盖整段交互，作用域结束（含错误路径）即恢复终端
        let _raw = input::RawMode::enter()?;
        loop {
            let screen = picker.screen();
            frame.draw(&screen.lines, screen.cursor)?;
            if let Some(done) = picker.handle(input::read_key()?)? {
                break done;
            }
        }
    };
    // 终端已恢复：清掉整块，再写收尾行
    frame.clear()?;
    match done {
        Done::Submitted(value) => {
            print_answer(label, &value);
            Ok(value)
        }
        Done::Cancelled => Err(InquireError::OperationCanceled).context("读取选择"),
        Done::Interrupted => Err(InquireError::OperationInterrupted).context("读取选择"),
    }
}

impl Frame {
    /// 清掉整块，光标停在块顶
    ///
    /// # Errors
    ///
    /// 终端写入失败时返回底层错误
    fn clear(&mut self) -> Result<()> {
        self.draw(&[], (0, 0))
    }
}

/// 收尾行：把已选值留在屏上（写 stderr，stdout 只留数据）
#[allow(clippy::print_stderr)]
fn print_answer(label: &str, picked: &str) {
    let prefix = Style::new()
        .for_stderr()
        .cyan()
        .apply_to(format!("{label} > "));
    eprintln!("{prefix}{picked}");
}

/// 单行可用列数：留 1 列规避「写满整行进入待折行」的终端状态
fn max_line_cols() -> usize {
    let (columns, _) = terminal_size().unwrap_or((80, 24));
    usize::from(columns).saturating_sub(1).max(1)
}

/// 按列数截断纯文本再套样式：ANSI 只包在截断结果外，不会被截半
fn styled(text: &str, style: &Style, max: usize) -> String {
    style.apply_to(truncate_str(text, max, "…")).to_string()
}

/// 输入按尾部显示：超宽时截头部并加省略号
///
/// 返回 `(显示文本, 被截掉的字节数)`；字节数用于把光标换算回可见区。
fn tail_fit(text: &str, budget: usize) -> (String, usize) {
    if measure_text_width(text) <= budget {
        return (text.to_owned(), 0);
    }
    let target = budget.saturating_sub(1); // 留 1 列给省略号
    let mut tail = String::new();
    let mut width = 0;
    let mut cut = text.len();
    for (index, ch) in text.char_indices().rev() {
        let ch_width = measure_text_width(&ch.to_string());
        if width + ch_width > target {
            cut = index + ch.len_utf8();
            break;
        }
        width += ch_width;
        tail.insert(0, ch);
        cut = index;
    }
    (format!("…{tail}"), cut)
}

/// 字符下标 → 字节下标（下标越界返回字符串长度）
fn char_to_byte(text: &str, index: usize) -> usize {
    text.char_indices()
        .nth(index)
        .map_or(text.len(), |(byte, _)| byte)
}

/// 行列数 → crossterm 参数（终端尺寸远小于 `u16` 上限，越界按上限截断）
fn as_u16(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

/// 打印完 `line_count` 行、光标停在最后一行后，回到相对行 `target_row` 需上移的行数
///
/// 关键在「最后一行」是相对行 `line_count - 1`：回退到 `target_row` 是
/// `(line_count - 1) - target_row`。若按 `line_count - target_row` 会多退一行，
/// 光标落到块顶上方，逐帧把整块往上顶、覆盖历史。
fn lines_up(line_count: usize, target_row: usize) -> usize {
    line_count.saturating_sub(1).saturating_sub(target_row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::Chord;

    fn choices(specs: &[(&str, bool)]) -> Vec<ScopedChoice> {
        specs
            .iter()
            .map(|(label, extra)| ScopedChoice::new(*label, *extra))
            .collect()
    }

    fn hint() -> ScopeHint {
        ScopeHint {
            show_all: "显示所有版本",
            show_default: "只显示正式版",
        }
    }

    fn new_picker<'a>(label: &'a str, choices: &'a [ScopedChoice]) -> Picker<'a> {
        Picker::new(Lang::Zh, label, choices, hint())
    }

    fn type_chars(picker: &mut Picker<'_>, text: &str) {
        for ch in text.chars() {
            let done = picker.handle(Key::Char(ch)).expect("输入不应失败");
            assert!(done.is_none(), "字符键不应结束交互");
        }
    }

    /// 默认范围只列非 extra 候选；`Ctrl+A` 来回切换且保留输入
    #[test]
    fn ctrl_a_toggles_scope_through_keymap() {
        let all = choices(&[
            ("1.21.1", false),
            ("24w45a（快照）", true),
            ("1.0（旧版）", true),
        ]);
        let mut picker = new_picker("选择版本", &all);
        assert_eq!(picker.pool, vec!["1.21.1"]);
        assert_eq!(picker.scope, Scope::Default);

        type_chars(&mut picker, "1.");
        let done = picker
            .handle(Key::Chord(Chord::Ctrl('a')))
            .expect("切换不应失败");
        assert_eq!(done, None, "切换不结束交互");
        assert_eq!(picker.scope, Scope::All);
        assert_eq!(
            picker.pool,
            vec!["1.21.1", "24w45a（快照）", "1.0（旧版）"],
            "切到全部后补进扩展候选"
        );
        assert_eq!(picker.input, "1.", "切换保留已输入的关键字");
        assert_eq!(picker.filtered().len(), 2, "建议按新候选集重算");

        picker
            .handle(Key::Chord(Chord::Ctrl('a')))
            .expect("切换不应失败");
        assert_eq!(picker.scope, Scope::Default);
        assert_eq!(picker.pool, vec!["1.21.1"]);
    }

    /// 未绑定与无关动作不改变范围，输入也不被吞掉语义
    #[test]
    fn other_chords_do_nothing() {
        let all = choices(&[("1.21.1", false), ("24w45a（快照）", true)]);
        let mut picker = new_picker("选择版本", &all);
        for chord in [
            Chord::Ctrl('x'), // 未绑定
            Chord::Ctrl('d'), // 绑定了，但是控制台的分离动作
        ] {
            let done = picker.handle(Key::Chord(chord)).expect("按键不应失败");
            assert_eq!(done, None);
            assert_eq!(picker.scope, Scope::Default, "{chord:?} 不该切换范围");
        }
    }

    /// 没有非 extra 候选时直接开在全部，避免默认范围空列表
    #[test]
    fn all_extra_starts_in_all_scope() {
        let only_extra = choices(&[("24w45a", true), ("1.0（旧版）", true)]);
        let picker = new_picker("选择版本", &only_extra);
        assert_eq!(picker.scope, Scope::All);
        assert_eq!(picker.pool, vec!["24w45a", "1.0（旧版）"]);
    }

    /// 回车口径：高亮直接采纳；无高亮必须是候选全名
    #[test]
    fn enter_submits_highlight_or_exact_label() {
        let all = choices(&[("1.21.1", false), ("1.21.2", false)]);
        let mut picker = new_picker("选择版本", &all);

        type_chars(&mut picker, "1.21");
        assert_eq!(
            picker.handle(Key::Enter).expect("回车不应失败"),
            None,
            "部分匹配不是合法选择"
        );
        assert_eq!(
            picker.error.as_deref(),
            Some(err_not_a_choice(Lang::Zh).as_str())
        );

        picker.handle(Key::Down).expect("按键不应失败");
        assert_eq!(picker.highlight, Some(0));
        assert_eq!(
            picker.handle(Key::Enter).expect("回车不应失败"),
            Some(Done::Submitted("1.21.1".to_owned())),
            "高亮项优先于输入文本"
        );

        let mut picker = new_picker("选择版本", &all);
        assert_eq!(
            picker.handle(Key::Enter).expect("回车不应失败"),
            None,
            "空输入不提交"
        );
        assert_eq!(
            picker.error.as_deref(),
            Some(err_empty_input(Lang::Zh).as_str())
        );

        type_chars(&mut picker, "1.21.2");
        assert_eq!(
            picker.handle(Key::Enter).expect("回车不应失败"),
            Some(Done::Submitted("1.21.2".to_owned())),
            "输入全名可直接提交"
        );
    }

    /// 范围只影响可见候选：默认范围里输扩展候选的全名也不认
    #[test]
    fn hidden_candidates_are_not_selectable_until_toggled() {
        let all = choices(&[("1.21.1", false), ("24w45a（快照）", true)]);
        let mut picker = new_picker("选择版本", &all);
        type_chars(&mut picker, "24w45a（快照）");
        assert_eq!(picker.handle(Key::Enter).expect("回车不应失败"), None);
        assert_eq!(
            picker.error.as_deref(),
            Some(err_not_a_choice(Lang::Zh).as_str())
        );

        picker
            .handle(Key::Chord(Chord::Ctrl('a')))
            .expect("切换不应失败");
        assert_eq!(
            picker.handle(Key::Enter).expect("回车不应失败"),
            Some(Done::Submitted("24w45a（快照）".to_owned()))
        );
    }

    /// Esc / Ctrl+C 结束交互，不改状态
    #[test]
    fn esc_and_interrupt_end_interaction() {
        let all = choices(&[("1.21.1", false), ("1.21.2", false)]);
        let mut picker = new_picker("选择版本", &all);
        assert_eq!(
            picker.handle(Key::Esc).expect("取消不应失败"),
            Some(Done::Cancelled)
        );
        assert_eq!(
            picker.handle(Key::Interrupt).expect("中断不应失败"),
            Some(Done::Interrupted)
        );
    }

    /// 光标编辑：插入 / 删除 / 移动都按字符走，边界不越界
    #[test]
    fn editing_moves_cursor_by_char() {
        let all = choices(&[("1.21.1", false), ("1.21.2", false)]);
        let mut picker = new_picker("选择版本", &all);
        type_chars(&mut picker, "1.2中");
        assert_eq!(picker.input, "1.2中");
        assert_eq!(picker.cursor, 4);

        picker.handle(Key::Left).expect("按键不应失败");
        picker.handle(Key::Left).expect("按键不应失败");
        assert_eq!(picker.cursor, 2);
        picker.handle(Key::Char('x')).expect("输入不应失败");
        assert_eq!(picker.input, "1.x2中", "在光标处插入");

        picker.handle(Key::Delete).expect("按键不应失败");
        assert_eq!(picker.input, "1.x中", "删除光标后字符");
        picker.handle(Key::Home).expect("按键不应失败");
        assert_eq!(picker.cursor, 0);
        picker.handle(Key::Backspace).expect("按键不应失败");
        assert_eq!(picker.cursor, 0, "行首退格不越界");
        picker.handle(Key::End).expect("按键不应失败");
        assert_eq!(picker.cursor, picker.input.chars().count());
        picker.handle(Key::Delete).expect("按键不应失败");
        assert_eq!(
            picker.input.chars().count(),
            4,
            "行尾 Delete 不越界（无字符可删）"
        );
        picker.handle(Key::Right).expect("按键不应失败");
        assert_eq!(picker.cursor, 4, "行尾右移不越界");
    }

    /// 高亮移动口径同 inquire：下移从首条起，上移越顶回到未高亮
    #[test]
    fn highlight_moves_like_inquire() {
        let all = choices(&[("a-1", false), ("b-2", false)]);
        let mut picker = new_picker("选择", &all);

        picker.handle(Key::Up).expect("按键不应失败");
        assert_eq!(picker.highlight, None, "无高亮时上移保持无高亮");

        picker.handle(Key::Down).expect("按键不应失败");
        assert_eq!(picker.highlight, Some(0));
        picker.handle(Key::Down).expect("按键不应失败");
        assert_eq!(picker.highlight, Some(1));
        picker.handle(Key::Down).expect("按键不应失败");
        assert_eq!(picker.highlight, Some(1), "到底停在末条");

        picker.handle(Key::Up).expect("按键不应失败");
        assert_eq!(picker.highlight, Some(0));
        picker.handle(Key::Up).expect("按键不应失败");
        assert_eq!(picker.highlight, None, "越顶回到未高亮");

        picker.handle(Key::PageDown).expect("按键不应失败");
        assert_eq!(picker.highlight, Some(1), "整页下移夹到末条");
        picker.handle(Key::PageUp).expect("按键不应失败");
        assert_eq!(picker.highlight, None, "整页上移越顶回到未高亮");
    }

    /// 超过一页时向下选择会滚动窗口，一直可以走到末条
    #[test]
    fn list_scrolls_past_first_page() {
        let labels: Vec<String> = (0..15).map(|i| format!("v{i:02}")).collect();
        let specs: Vec<(&str, bool)> = labels.iter().map(|label| (label.as_str(), false)).collect();
        let all = choices(&specs);
        let mut picker = new_picker("选择", &all);
        let len = picker.filtered().len();
        assert_eq!(len, 15);

        // 走到第一页末条：窗口不动，高亮停在页内最后一条
        for _ in 0..PAGE_SIZE {
            picker.handle(Key::Down).expect("按键不应失败");
        }
        assert_eq!(picker.highlight, Some(PAGE_SIZE - 1));
        assert_eq!(picker.window(len), (0, PAGE_SIZE));
        assert_eq!(picker.scroll, 0);

        // 再下移一条：高亮越过窗口，窗口随之滚动一格
        picker.handle(Key::Down).expect("按键不应失败");
        assert_eq!(picker.highlight, Some(PAGE_SIZE));
        let (start, end) = picker.window(len);
        assert_eq!(start, 1, "窗口跟随高亮滚动");
        assert_eq!(end, PAGE_SIZE + 1);
        assert!(start <= PAGE_SIZE && PAGE_SIZE < end, "高亮仍在窗口内");

        // 一直下移：停在末条，窗口滚到最后一页
        for _ in 0..20 {
            picker.handle(Key::Down).expect("按键不应失败");
        }
        assert_eq!(picker.highlight, Some(len - 1), "到底停在末条");
        assert_eq!(picker.window(len), (len - PAGE_SIZE, len), "滚到最后一页");

        // 再上移几条：窗口回滚，高亮保持在窗口内
        for _ in 0..3 {
            picker.handle(Key::Up).expect("按键不应失败");
        }
        let (start, end) = picker.window(len);
        let highlighted = picker.highlight.expect("应仍有高亮");
        assert!(
            start <= highlighted && highlighted < end,
            "上移后高亮在窗口内"
        );
    }

    /// 画面只渲染窗口内的一页：候选再多，行数也恒定
    #[test]
    fn screen_renders_only_the_window() {
        let labels: Vec<String> = (0..15).map(|i| format!("v{i:02}")).collect();
        let specs: Vec<(&str, bool)> = labels.iter().map(|label| (label.as_str(), false)).collect();
        let all = choices(&specs);
        let mut picker = new_picker("选择", &all);

        let initial = picker.screen();
        assert_eq!(
            initial.lines.len(),
            1 + PAGE_SIZE + 1,
            "输入行 + 一页建议 + 帮助行"
        );

        for _ in 0..13 {
            picker.handle(Key::Down).expect("按键不应失败");
        }
        let scrolled = picker.screen();
        assert_eq!(
            scrolled.lines.len(),
            initial.lines.len(),
            "滚动后仍是固定页高"
        );
        assert_eq!(scrolled.cursor.0, 0, "光标仍在输入行");

        // 高亮项必须落在渲染出的窗口里
        let (start, end) = picker.window(picker.filtered().len());
        let visible = &picker.filtered()[start..end];
        let highlighted = &picker.filtered()[picker.highlight.expect("应已高亮")];
        assert!(visible.contains(highlighted), "高亮项在可见窗口内");
        assert!(
            scrolled.lines.iter().any(|line| line.contains(highlighted)),
            "高亮项确实被渲染"
        );
    }

    /// 内容变化清高亮与错误；纯光标移动不动高亮
    #[test]
    fn content_change_resets_highlight_and_error() {
        let all = choices(&[("a-1", false), ("b-2", false)]);
        // 构造失败态：输入部分匹配但不是全名，回车报错
        let mut picker = new_picker("选择", &all);
        type_chars(&mut picker, "a");
        picker.handle(Key::Enter).expect("回车不应失败");
        assert!(picker.error.is_some());

        picker.handle(Key::Down).expect("按键不应失败");
        picker.handle(Key::Left).expect("按键不应失败");
        assert!(picker.error.is_some(), "纯移动不清错误");
        assert_eq!(picker.highlight, Some(0), "纯移动不清高亮");

        picker.handle(Key::End).expect("按键不应失败");
        picker.handle(Key::Backspace).expect("退格不应失败");
        assert!(picker.error.is_none(), "内容变化清错误");
        assert_eq!(picker.highlight, None, "内容变化清高亮");
    }

    /// Tab 补全：单一匹配补全成全名
    #[test]
    fn tab_completes_single_match() {
        let all = choices(&[("1.21.1", false), ("1.21.2", false)]);
        let mut picker = new_picker("选择版本", &all);
        type_chars(&mut picker, "1.21.1");
        picker.handle(Key::Tab).expect("补全不应失败");
        assert_eq!(picker.input, "1.21.1");
        assert_eq!(picker.cursor, 6);

        let mut picker = new_picker("选择版本", &all);
        type_chars(&mut picker, "1.21.2");
        picker.handle(Key::Backspace).expect("退格不应失败");
        assert_eq!(picker.input, "1.21.");
        picker.handle(Key::End).expect("按键不应失败");
        picker.handle(Key::Char('2')).expect("输入不应失败");
        assert_eq!(picker.input, "1.21.2");
        picker.handle(Key::Left).expect("按键不应失败");
        picker.handle(Key::Left).expect("按键不应失败");
        picker.handle(Key::Left).expect("按键不应失败");
        picker.handle(Key::Backspace).expect("退格不应失败");
        assert_eq!(picker.input, "1.1.2", "删除光标前字符");
    }

    /// 画面不超出行宽（折行会把重绘行数算错），光标落在输入行
    #[test]
    fn screen_fits_width_and_places_cursor() {
        let all = choices(&[("1.21.1", false), ("24w45a（快照）", true)]);
        let mut picker = new_picker("选择版本", &all);
        let max = max_line_cols();

        let screen = picker.screen();
        // 输入行 + 1 条建议（默认范围） + 帮助行
        assert_eq!(screen.lines.len(), 3);
        assert_eq!(screen.cursor.0, 0, "无错误时光标在第 0 行");
        for line in &screen.lines {
            assert!(measure_text_width(line) <= max, "行超宽会折行破坏重绘");
        }

        type_chars(&mut picker, &"x".repeat(500));
        let screen = picker.screen();
        for line in &screen.lines {
            assert!(measure_text_width(line) <= max, "超长输入也必须截到行宽内");
        }

        let mut failing = new_picker("选择版本", &all);
        type_chars(&mut failing, "zz");
        failing.handle(Key::Enter).expect("回车不应失败");
        let screen = failing.screen();
        assert_eq!(screen.cursor.0, 1, "错误行占第 0 行，输入行下移到第 1 行");
        assert!(screen.lines[0].contains(&err_not_a_choice(Lang::Zh)));
    }

    /// 光标落点：随输入增长右移，且不超出该行可用列
    #[test]
    fn cursor_column_tracks_input() {
        let all = choices(&[("1.21.1", false), ("1.21.2", false)]);
        let mut picker = new_picker("选择版本", &all);
        let max = max_line_cols();
        let (_, empty_column) = picker.input_line(max);
        type_chars(&mut picker, "1.21");
        let (_, column) = picker.input_line(max);
        assert!(column > empty_column, "输入增长光标右移");
        assert!(column <= max, "光标不越出行宽");
    }

    /// 尾部保留：超预算截头部，省略号占 1 列
    #[test]
    fn tail_fit_truncates_head_only() {
        let (visible, cut) = tail_fit("abcdefghij", 4);
        assert_eq!(visible, "…hij");
        assert_eq!(cut, 7, "保留尾部 hij，截掉 abcdefg");
        assert_eq!(&"abcdefghij"[cut..], "hij");

        let (plain, cut) = tail_fit("abc", 10);
        assert_eq!((plain.as_str(), cut), ("abc", 0), "不超宽原样返回");
    }

    /// 字符下标 → 字节下标（多字节字符不切半个）
    #[test]
    fn char_to_byte_counts_chars() {
        assert_eq!(char_to_byte("abc", 0), 0);
        assert_eq!(char_to_byte("abc", 2), 2);
        assert_eq!(char_to_byte("abc", 9), 3);
        assert_eq!(char_to_byte("中中", 1), 3);
    }

    /// 回到块顶/光标行的上移行数：不能多退一行（会覆盖块顶之上的历史）
    #[test]
    fn lines_up_never_overshoots_block_top() {
        // 12 行、光标在末行：无需上移
        assert_eq!(lines_up(12, 11), 0);
        // 光标在块顶（相对 0）：上移 11 行，正好回到顶行
        assert_eq!(lines_up(12, 0), 11);
        // 光标在中间：差值为 12 - 1 - 5
        assert_eq!(lines_up(12, 5), 6);
        // 空块（清屏路径）：不动
        assert_eq!(lines_up(0, 0), 0);
        // 防御：target_row 越界时不回绕
        assert_eq!(lines_up(3, 99), 0);
    }

    /// 空候选报错，单候选免提问
    #[test]
    fn empty_and_single_choice_shortcut() {
        assert!(select_filter_scoped(Lang::Zh, "选择版本", &[], hint()).is_err());
        let single = choices(&[("1.21.1", false)]);
        assert_eq!(
            select_filter_scoped(Lang::Zh, "选择版本", &single, hint()).expect("单候选直接返回"),
            "1.21.1"
        );
    }

    /// 逐行下移必须用 CR+LF（LF 在底行滚屏），不能用 CNL：
    /// 用 CNL 时块贴近终端底部会被夹在底行，首帧只露出最后一行
    #[test]
    fn frame_advances_lines_with_crlf_not_cnl() {
        let mut frame = Frame::default();
        let mut buf: Vec<u8> = Vec::new();
        frame
            .draw_to(&mut buf, &["a".to_owned(), "b".to_owned()], (0, 0))
            .expect("写入内存缓冲");
        let out = String::from_utf8(buf).expect("ASCII 输出");
        assert!(out.contains("\r\n"), "下移用 CR+LF，LF 底行才滚屏");
        assert!(!out.contains("\x1b[1E"), "不能用 CNL（MoveToNextLine）");
        // 两行、光标回块顶：上移 1 行
        assert!(out.contains("\x1b[1A"), "回退到相对行 0");
    }

    /// 清屏（空块）不产生换行，只上移 + 清到行尾
    #[test]
    fn frame_clear_emits_no_newline() {
        let mut frame = Frame::default();
        let mut buf: Vec<u8> = Vec::new();
        frame.draw_to(&mut buf, &[], (0, 0)).expect("写入内存缓冲");
        let out = String::from_utf8(buf).expect("ASCII 输出");
        assert!(!out.contains('\n'), "清屏不需要下移");
        assert!(!out.contains('\r'), "清屏不需要回车换行");
        assert!(out.contains("\x1b[J"), "清到光标以下");
    }
}
