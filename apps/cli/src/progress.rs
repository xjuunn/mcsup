use indicatif::{ProgressBar, ProgressStyle};
use mcsup::port::ProgressSink;
use std::sync::Mutex;
use std::time::Duration;

const SPINNER: &str = "{spinner:.cyan} {msg}";
const SPINNER_BYTES: &str = "{spinner:.cyan} {msg} ({bytes})";
/// 单行进度条：不带 spinner / 换行，避免与 `steady_tick` 叠加闪烁
const BAR: &str = "{msg}  [{bar:36.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, ETA {eta})";
const TICKS: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const TICK_INTERVAL: Duration = Duration::from_millis(100);

/// 阶段 spinner：单字符 + 文案
fn apply_spinner_style(bar: &ProgressBar) {
    if let Ok(style) = ProgressStyle::with_template(SPINNER) {
        bar.set_style(style.tick_strings(TICKS));
    }
}

/// 总长未知的下载回显：spinner + 已传字节
fn apply_unknown_length_style(bar: &ProgressBar) {
    if let Ok(style) = ProgressStyle::with_template(SPINNER_BYTES) {
        bar.set_style(style.tick_strings(TICKS));
    }
}

/// 已知总长的单行进度条
fn apply_bar_style(bar: &ProgressBar) {
    if let Ok(style) = ProgressStyle::with_template(BAR) {
        bar.set_style(style);
    }
}

/// 从 `finish` 终态恢复回显
///
/// `finish_and_clear` 之后 bar 的状态是 `DoneHidden`：不再绘制、steady tick
/// 线程退出，之后 `set_style` / `set_message` 都画不出任何东西。
/// 只要还要在这条线上继续回显，就必须 `reset` 并重新挂 tick。
fn resume(bar: &ProgressBar) {
    if bar.is_finished() {
        bar.reset();
    }
    bar.enable_steady_tick(TICK_INTERVAL);
}

fn make_spinner(enable: bool, message: String) -> ProgressBar {
    let bar = if enable {
        ProgressBar::new_spinner()
    } else {
        ProgressBar::hidden()
    };
    apply_spinner_style(&bar);
    bar.enable_steady_tick(TICK_INTERVAL);
    bar.set_message(message);
    bar
}

/// 耗时任务的单行 spinner：可中途换消息，结束时清除
///
/// CLI 中任何可能超过数百毫秒的步骤都应挂一个，
/// 避免用户以为卡死。
pub(crate) struct TaskSpinner {
    bar: ProgressBar,
}

impl TaskSpinner {
    /// `enable` 为 false（JSON 模式）时隐藏，stdout 保持纯数据
    pub(crate) fn new(message: impl Into<String>, enable: bool) -> Self {
        Self {
            bar: make_spinner(enable, message.into()),
        }
    }

    /// 换当前步骤文案
    pub(crate) fn set_message(&self, message: impl Into<String>) {
        self.bar.set_message(message.into());
    }

    /// 清除 spinner；此后这条线不再回显，继续用要先 [`restart`](Self::restart)
    pub(crate) fn finish(&self) {
        self.bar.finish_and_clear();
    }

    /// 从 `finish` 终态恢复回显
    ///
    /// 交互提示要独占这一行（`finish` 让 steady tick 退出，提示期间不会被
    /// spinner 打断），提示结束后的阶段还得接着画，否则整段都是空白。
    pub(crate) fn restart(&self) {
        apply_spinner_style(&self.bar);
        resume(&self.bar);
    }

    /// 把这条回显行交给下载进度条复用
    ///
    /// 同一条线只能有一个 bar 在画：换手时改样式而不是新建对象，
    /// 两个 bar 各自画同一行会互相清除。
    pub(crate) fn handle(&self) -> ProgressBar {
        self.bar.clone()
    }
}

impl Drop for TaskSpinner {
    fn drop(&mut self) {
        self.bar.finish_and_clear();
    }
}

/// 下载进度：复用阶段 spinner 的那一条回显线
///
/// 阶段切到下载时调用 [`start`](Self::start) 立即接管这一行 —— 从发起 HTTP 请求
/// 到首字节之间没有任何进度回调，等首个 `on_progress` 才显示会空白数秒。
/// 没有阶段回调的调用方则由首个 `on_progress` 自动接手。
///
/// JSON 模式下 spinner本身就是 hidden bar，复用它自然不产生输出。
pub(crate) struct BarSink {
    message: String,
    bar: ProgressBar,
    /// 回显状态
    state: Mutex<SinkState>,
}

#[derive(Default)]
struct SinkState {
    /// 是否已在这条线上回显下载
    started: bool,
    /// 是否已知总长并切成定长进度条
    determinate: bool,
}

impl BarSink {
    /// `bar` 来自 [`TaskSpinner::handle`]：与阶段 spinner 同一条线
    pub(crate) fn new(bar: ProgressBar, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            bar,
            state: Mutex::new(SinkState::default()),
        }
    }

    /// 进入下载阶段：立刻在这条线上显示「下载中」，不等首字节
    pub(crate) fn start(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.started = true;
        state.determinate = false;
        resume(&self.bar);
        apply_unknown_length_style(&self.bar);
        self.bar.set_message(self.message.clone());
    }

    /// 下载阶段结束：把这条线交还阶段 spinner（不清屏，避免出现空白）
    pub(crate) fn finish(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.started = false;
        state.determinate = false;
        resume(&self.bar);
        apply_spinner_style(&self.bar);
    }
}

impl ProgressSink for BarSink {
    fn on_progress(&self, transferred: u64, total: Option<u64>) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if !state.started {
            // 没有阶段回调的调用方：首个进度在这里接手回显
            state.started = true;
            resume(&self.bar);
            apply_unknown_length_style(&self.bar);
            self.bar.set_message(self.message.clone());
        }
        match total {
            Some(total) if !state.determinate => {
                // 切换单行 bar，停 steady_tick，只在位置变化时刷新
                self.bar.set_length(total);
                apply_bar_style(&self.bar);
                self.bar.disable_steady_tick();
                self.bar.set_position(transferred);
                state.determinate = true;
            }
            // 已是定长进度条，或总长未知（保持 spinner + 已传字节）
            _ => self.bar.set_position(transferred),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `finish` 后 bar 进入终态：不再绘制、steady tick 退出。
    /// 不 `restart` 就继续 `set_message`，整段阶段回显都是空白。
    #[test]
    fn restart_resumes_finished_spinner() {
        let spinner = TaskSpinner::new("step", false);
        let handle = spinner.handle();
        assert!(!handle.is_finished());
        spinner.finish();
        assert!(handle.is_finished());
        spinner.restart();
        assert!(!handle.is_finished());
    }

    /// 下载进度与阶段 spinner共用同一条线，且 finish 之后仍能接手
    #[test]
    fn sink_reuses_spinner_line() {
        let spinner = TaskSpinner::new("解析", false);
        let handle = spinner.handle();
        let sink = BarSink::new(handle.clone(), "下载 Java 25");

        spinner.finish();
        sink.start();
        assert!(!handle.is_finished());
        assert_eq!(handle.message(), "下载 Java 25");

        sink.on_progress(10, None);
        assert_eq!(handle.position(), 10);
        sink.on_progress(10, Some(100));
        assert_eq!(handle.length(), Some(100));

        sink.finish();
        assert!(!handle.is_finished());
    }
}
