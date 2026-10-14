use indicatif::{ProgressBar, ProgressStyle};
use port::ProgressSink;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const SPINNER: &str = "{spinner:.cyan} {msg} ({bytes})";
const BAR: &str = "{msg}\n{spinner:.cyan} [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, ETA {eta})";

/// 把下载进度渲染到 stderr 的 indicatif 适配器
pub(crate) struct BarSink {
    bar: ProgressBar,
    determinate: AtomicBool,
}

impl BarSink {
    /// `enable` 为 false (JSON模式)时进度条隐藏，stdout保持纯数据
    pub(crate) fn new(message: String, enable: bool) -> Self {
        let bar = if enable {
            ProgressBar::new_spinner()
        } else {
            ProgressBar::hidden()
        };
        if let Ok(style) = ProgressStyle::with_template(SPINNER) {
            bar.set_style(style.tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]));
        }
        bar.enable_steady_tick(Duration::from_millis(100));
        bar.set_message(message);
        Self {
            bar,
            determinate: AtomicBool::new(false),
        }
    }

    pub(crate) fn finish(&self) {
        self.bar.finish_and_clear();
    }
}

impl ProgressSink for BarSink {
    fn on_progress(&self, transferred: u64, total: Option<u64>) {
        if let Some(total) = total {
            self.bar.set_length(total);
            if !self.determinate.swap(true, Ordering::Relaxed)
                && let Ok(style) = ProgressStyle::with_template(BAR)
            {
                self.bar.set_style(style);
            }
        }
        self.bar.set_position(transferred);
    }
}
