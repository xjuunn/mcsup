use indicatif::{ProgressBar, ProgressStyle};
use mcsup::port::ProgressSink;
use std::sync::Mutex;
use std::time::Duration;

const SPINNER: &str = "{spinner:.cyan} {msg}";
const SPINNER_BYTES: &str = "{spinner:.cyan} {msg} ({bytes})";
/// 单行进度条：不带 spinner / 换行，避免与 `steady_tick` 叠加闪烁
const BAR: &str = "{msg}  [{bar:36.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, ETA {eta})";
const TICKS: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn make_spinner(enable: bool, message: String) -> ProgressBar {
    let bar = if enable {
        ProgressBar::new_spinner()
    } else {
        ProgressBar::hidden()
    };
    if let Ok(style) = ProgressStyle::with_template(SPINNER) {
        bar.set_style(style.tick_strings(TICKS));
    }
    bar.enable_steady_tick(Duration::from_millis(100));
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

    /// 清除 spinner
    pub(crate) fn finish(&self) {
        self.bar.finish_and_clear();
    }
}

impl Drop for TaskSpinner {
    fn drop(&mut self) {
        self.bar.finish_and_clear();
    }
}

/// 下载进度条：首次 `on_progress` 前保持隐藏
///
/// 避免与阶段 spinner 叠画；获知总长后切换单行 bar 并停用 `steady_tick`
/// 防止闪烁。
pub(crate) struct BarSink {
    message: String,
    enable: bool,
    inner: Mutex<Option<DownloadBar>>,
}

struct DownloadBar {
    bar: ProgressBar,
    determinate: bool,
}

impl BarSink {
    /// `enable` 为 false（JSON 模式）时始终隐藏，stdout 保持纯数据
    pub(crate) fn new(message: impl Into<String>, enable: bool) -> Self {
        Self {
            message: message.into(),
            enable,
            inner: Mutex::new(None),
        }
    }

    pub(crate) fn finish(&self) {
        if let Ok(guard) = self.inner.lock()
            && let Some(state) = guard.as_ref()
        {
            state.bar.finish_and_clear();
        }
    }
}

impl ProgressSink for BarSink {
    fn on_progress(&self, transferred: u64, total: Option<u64>) {
        if !self.enable {
            return;
        }
        let Ok(mut guard) = self.inner.lock() else {
            return;
        };
        let state = guard.get_or_insert_with(|| {
            let bar = ProgressBar::new(0);
            bar.set_message(self.message.clone());
            DownloadBar {
                bar,
                determinate: false,
            }
        });

        match total {
            Some(total) if !state.determinate => {
                // 切换单行 bar，停 steady_tick，只在位置变化时刷新
                state.bar.set_length(total);
                if let Ok(style) = ProgressStyle::with_template(BAR) {
                    state.bar.set_style(style);
                }
                state.bar.disable_steady_tick();
                state.bar.set_position(transferred);
                state.determinate = true;
            }
            None if !state.determinate => {
                // 未知总长：spinner + 已传字节
                if let Ok(style) = ProgressStyle::with_template(SPINNER_BYTES) {
                    state.bar.set_style(style.tick_strings(TICKS));
                }
                state.bar.enable_steady_tick(Duration::from_millis(100));
                state.bar.set_position(transferred);
            }
            _ => {
                state.bar.set_position(transferred);
            }
        }
    }
}
