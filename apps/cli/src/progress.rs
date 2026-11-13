use crate::output::human_bytes;
use indicatif::{FormattedDuration, ProgressBar, ProgressState, ProgressStyle};
use mcsup::port::{CancellationToken, InstallerOutputSink, ProgressSink, TaskObserver, TaskStage};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SPINNER: &str = "{spinner:.cyan} {msg}";
const SPINNER_BYTES: &str = "{spinner:.cyan} {msg} ({bytes})";
/// 单行进度条：不带 spinner / 换行，避免与 `steady_tick` 叠加闪烁
const BAR: &str = "{msg}  [{bar:36.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, ETA {eta})";
const TICKS: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const TICK_INTERVAL: Duration = Duration::from_millis(100);

/// 速度统计窗口：只取最近这么久的字节增量
///
/// indicatif 内建估计器的时间常数是 15 秒（双重平滑后约 30 秒），下载开头
/// 要几十秒才爬到真实速度、收尾位置不动了速度还在缓慢回落，画面上的
/// `MB/s` 与进度增长对不上。3 秒滑窗既能看清瞬时变化，又不会被单个
/// chunk 的抖动带偏。
const SPEED_WINDOW: Duration = Duration::from_secs(3);
/// 采样最短跨度：不足时按零速处理，避免首个采样点把几个 chunk 估成天文数字
const SPEED_MIN_SPAN: Duration = Duration::from_millis(400);
/// 超过这么久没有新字节即视为停住：速度归零，而不是继续显示历史值
const SPEED_STALL: Duration = Duration::from_millis(1500);

/// 下载速度计：按最近 [`SPEED_WINDOW`] 的采样算平均速率
///
/// 位置可能回退（分段失败会退还该段的已写字节），回退时以新位置重新起算，
/// 否则会算出负增量或把重传的字节重复计入。
#[derive(Debug, Default)]
struct SpeedMeter {
    samples: VecDeque<(Instant, u64)>,
}

impl SpeedMeter {
    /// 以当前位置为新基线（开始下载、切定长、续传接上残片时调用）
    fn reset(&mut self, transferred: u64, now: Instant) {
        self.samples.clear();
        self.samples.push_back((now, transferred));
    }

    fn record(&mut self, transferred: u64, now: Instant) {
        if self
            .samples
            .back()
            .is_some_and(|&(_, last)| transferred < last)
        {
            self.samples.clear();
        }
        self.samples.push_back((now, transferred));
        while self.samples.len() > 2 && now.duration_since(self.samples[0].0) > SPEED_WINDOW {
            self.samples.pop_front();
        }
    }

    /// 最近窗口内的平均速率（字节/秒）
    #[allow(clippy::cast_precision_loss)]
    fn per_sec(&self, now: Instant) -> f64 {
        let (Some(&(first_time, first_pos)), Some(&(last_time, last_pos))) =
            (self.samples.front(), self.samples.back())
        else {
            return 0.0;
        };
        if now.duration_since(last_time) > SPEED_STALL {
            return 0.0;
        }
        let span = now.duration_since(first_time).max(SPEED_MIN_SPAN);
        let bytes = last_pos.saturating_sub(first_pos);
        bytes as f64 / span.as_secs_f64()
    }

    /// 按当前速率估计剩余时间；速率未知时返回 `None`（显示 `--`）
    #[allow(clippy::cast_precision_loss)]
    fn eta(&self, remaining: u64, now: Instant) -> Option<Duration> {
        if remaining == 0 {
            return Some(Duration::ZERO);
        }
        let rate = self.per_sec(now);
        (rate > 1.0).then(|| Duration::from_secs_f64(remaining as f64 / rate))
    }
}

/// 速度是 `f64`，[`human_bytes`] 只接收 `u64`；四舍五入到整字节。
///
/// `NaN` / 负数一律落到 0，对应「还没开始传」。
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn rate_bytes(rate: f64) -> u64 {
    if rate.is_finite() && rate > 0.0 {
        rate.round() as u64
    } else {
        0
    }
}

/// 让进度条按 [`human_bytes`] 的 10 进制口径显示字节与速度，并用
/// [`SpeedMeter`] 取代 indicatif 的滞后估计器。
///
/// indicatif 内建的 `{bytes}` / `{total_bytes}` / `{bytes_per_sec}` 是 1024 进制的
/// KiB/MiB/GiB；`{decimal_*}` 又是小写 `kB`。两者都与本项目其它输出
/// （`human_bytes` 的 `KB` / `MB` / `GB`）不一致，所以统一覆盖成同一个函数。
fn decimal_units(style: ProgressStyle, speed: Arc<Mutex<SpeedMeter>>) -> ProgressStyle {
    let rate_meter = Arc::clone(&speed);
    style
        .with_key(
            "bytes",
            |state: &ProgressState, w: &mut dyn std::fmt::Write| {
                let _ = write!(w, "{}", human_bytes(state.pos()));
            },
        )
        .with_key(
            "total_bytes",
            |state: &ProgressState, w: &mut dyn std::fmt::Write| {
                let _ = write!(w, "{}", human_bytes(state.len().unwrap_or(state.pos())));
            },
        )
        .with_key(
            "bytes_per_sec",
            move |_state: &ProgressState, w: &mut dyn std::fmt::Write| {
                let rate = rate_meter
                    .lock()
                    .map_or(0.0, |meter| meter.per_sec(Instant::now()));
                let _ = write!(w, "{}/s", human_bytes(rate_bytes(rate)));
            },
        )
        .with_key(
            "eta",
            move |state: &ProgressState, w: &mut dyn std::fmt::Write| {
                let remaining = state
                    .len()
                    .unwrap_or(state.pos())
                    .saturating_sub(state.pos());
                let now = Instant::now();
                let text = speed
                    .lock()
                    .ok()
                    .and_then(|meter| meter.eta(remaining, now))
                    .map_or_else(|| "--".to_owned(), |eta| FormattedDuration(eta).to_string());
                let _ = write!(w, "{text}");
            },
        )
}

/// 阶段 spinner：单字符 + 文案
fn apply_spinner_style(bar: &ProgressBar) {
    if let Ok(style) = ProgressStyle::with_template(SPINNER) {
        bar.set_style(style.tick_strings(TICKS));
    }
}

/// 总长未知的下载回显：spinner + 已传字节
fn apply_unknown_length_style(bar: &ProgressBar, speed: Arc<Mutex<SpeedMeter>>) {
    if let Ok(style) = ProgressStyle::with_template(SPINNER_BYTES) {
        bar.set_style(decimal_units(style, speed).tick_strings(TICKS));
    }
}

/// 已知总长的单行进度条
fn apply_bar_style(bar: &ProgressBar, speed: Arc<Mutex<SpeedMeter>>) {
    if let Ok(style) = ProgressStyle::with_template(BAR) {
        bar.set_style(decimal_units(style, speed));
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
    /// Ctrl-C 取消令牌：长任务把它传给 service，取消才有实际效果
    cancel: CancellationToken,
}

impl TaskSpinner {
    /// `enable` 为 false（JSON 模式）时隐藏，stdout 保持纯数据
    ///
    /// 同时装上 Ctrl-C 处理器：长任务（备份扫描、恢复）要能协作取消，
    /// 否则 Ctrl-C 只杀入口，后台线程继续读盘直到结束。
    pub(crate) fn new(message: impl Into<String>, enable: bool) -> Self {
        let bar = make_spinner(enable, message.into());
        let cancel = CancellationToken::new();
        install_ctrl_c_handler(cancel.clone(), bar.clone());
        Self { bar, cancel }
    }

    /// Ctrl-C 取消令牌：传给 service 的长任务
    pub(crate) fn cancel(&self) -> &CancellationToken {
        &self.cancel
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
    bar: ProgressBar,
    /// 回显状态
    state: Mutex<SinkState>,
    /// 下载速率：模板里的 `bytes_per_sec` / `eta` 都从它取
    speed: Arc<Mutex<SpeedMeter>>,
}

#[derive(Default)]
struct SinkState {
    /// 下载阶段文案（可在下载开始前更新）
    message: String,
    /// 是否已在这条线上回显下载
    started: bool,
    /// 是否已知总长并切成定长进度条
    determinate: bool,
}

impl BarSink {
    /// `bar` 来自 [`TaskSpinner::handle`]：与阶段 spinner 同一条线
    pub(crate) fn new(bar: ProgressBar, message: impl Into<String>) -> Self {
        Self {
            bar,
            state: Mutex::new(SinkState {
                message: message.into(),
                ..SinkState::default()
            }),
            speed: Arc::new(Mutex::new(SpeedMeter::default())),
        }
    }

    /// 下载开始前换文案（如从 spinner 初始文案换成「下载 Java 21 jdk」）
    pub(crate) fn set_message(&self, message: impl Into<String>) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.message = message.into();
    }

    /// 进入下载阶段：立刻在这条线上显示「下载中」，不等首字节
    pub(crate) fn start(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.started = true;
        state.determinate = false;
        if let Ok(mut speed) = self.speed.lock() {
            speed.reset(self.bar.position(), Instant::now());
        }
        resume(&self.bar);
        apply_unknown_length_style(&self.bar, Arc::clone(&self.speed));
        self.bar.set_message(state.message.clone());
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

    /// 当前速率（测试用）
    #[cfg(test)]
    fn per_sec(&self) -> f64 {
        self.speed
            .lock()
            .map_or(0.0, |meter| meter.per_sec(Instant::now()))
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
            if let Ok(mut speed) = self.speed.lock() {
                speed.reset(transferred, Instant::now());
            }
            resume(&self.bar);
            apply_unknown_length_style(&self.bar, Arc::clone(&self.speed));
            self.bar.set_message(state.message.clone());
        }
        match total {
            Some(total) if !state.determinate => {
                // 切换单行 bar，停 steady_tick，只在位置变化时刷新
                self.bar.set_length(total);
                apply_bar_style(&self.bar, Arc::clone(&self.speed));
                self.bar.disable_steady_tick();
                // 到此为止的字节可能全是续传时磁盘上已有的部分。先以当前位置
                // 为新基线再落位：否则首个采样会把已有字节算成本轮吞吐，
                // 画面瞬间飙到几十 MiB/s 再回落到真实值。
                if let Ok(mut speed) = self.speed.lock() {
                    speed.reset(transferred, Instant::now());
                }
                self.bar.set_position(transferred);
                self.bar.reset_eta();
                state.determinate = true;
            }
            _ => {
                if let Ok(mut speed) = self.speed.lock() {
                    speed.record(transferred, Instant::now());
                }
                self.bar.set_position(transferred);
            }
        }
    }
}

/// 单行刷新扫描进度：`\r` 覆盖当前行，非 TTY 时退化为逐行
///
/// 扫描回调逐个到达，行文案由调用方给；本类型只管一行刷新与收尾换行
pub(crate) struct ScanProgress {
    /// human 模式才回显；JSON 模式全程静默，stdout 保持纯数据
    enabled: bool,
    interactive: bool,
    last_len: usize,
    /// 界面语言：进度行文案跟随
    lang: crate::locale::Lang,
}

impl ScanProgress {
    /// `enable` 通常传 `Printer::is_human`；单行刷新还额外要求 stderr 是 TTY
    pub(crate) fn new(enable: bool, lang: crate::locale::Lang) -> Self {
        use std::io::IsTerminal;
        Self {
            enabled: enable,
            interactive: enable && std::io::stderr().is_terminal(),
            last_len: 0,
            lang,
        }
    }

    /// 上报一条扫描到的运行时
    pub(crate) fn on_found(&mut self, path: &std::path::Path, version: &str) {
        use std::io::Write;
        if !self.enabled {
            return;
        }
        #[allow(clippy::print_stderr)]
        {
            let line = crate::i18n::pick(
                self.lang,
                format!("  扫描到: {} ({})", path.display(), version),
                format!("  found: {} ({})", path.display(), version),
            );
            if self.interactive {
                // 清掉上一行残留（路径长短不一时用空格补齐）
                let pad = self.last_len.saturating_sub(line.len());
                eprint!("\r{line}{}", " ".repeat(pad));
                let _ = std::io::stderr().flush();
                self.last_len = line.len();
            } else {
                eprintln!("{line}");
            }
        }
    }

    /// 结束：交互行补一个换行，避免后续输出接在进度行末尾
    pub(crate) fn finish(&mut self) {
        if self.enabled && self.interactive && self.last_len > 0 {
            #[allow(clippy::print_stderr)]
            {
                eprintln!();
            }
            self.last_len = 0;
        }
    }
}

/// 安装器输出的实时透传：把安装器日志逐行打印出来
///
/// 安装器联网拉取 libraries 可达数分钟，期间只有阶段文案的话用户会以为卡死。
///
/// 进入安装器阶段后**不再保留自己的回显线**：安装器本身就在往终端刷进度
/// （`\r` 原地刷新），再叠一层 spinner / 进度条会与它抢同一块区域，画面必然
/// 错乱，而且我们拿不到安装器内部的真实进度，画出来的也只是假进度。
/// 此时只把它的输出原样逐行打印，进展由安装器自己交代。
///
/// `enable` 为 false（JSON 模式）时静默丢弃，stdout 保持纯数据。
pub(crate) struct InstallerOutput {
    bar: ProgressBar,
    cancel: CancellationToken,
    enable: bool,
    /// 安装器阶段是否已收起回显线；收起后直接写行，不经过 bar
    detached: std::sync::atomic::AtomicBool,
}

impl InstallerOutput {
    fn new(bar: ProgressBar, cancel: CancellationToken, enable: bool) -> Self {
        Self {
            bar,
            cancel,
            enable,
            detached: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// 收起回显线：此后的安装器输出直接逐行打印
    ///
    /// 调用方负责先 `finish` 掉 spinner，否则最后一行会接在 spinner 后面。
    fn detach(&self) {
        self.detached
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

impl InstallerOutputSink for InstallerOutput {
    fn on_output(&self, line: &str) {
        // 取消后回显线已收掉，迟到的输出不再打印
        if !self.enable || self.cancel.is_cancelled() {
            return;
        }
        let line = line.trim_end();
        if line.is_empty() {
            return;
        }
        if self.detached.load(std::sync::atomic::Ordering::Relaxed) {
            #[allow(clippy::print_stdout)]
            {
                println!("{line}");
            }
        } else {
            self.bar.println(line);
        }
    }
}

/// 一次安装的回显与取消装配：spinner + 下载进度条 + 取消令牌
///
/// 三者同生同灭，装错一次就整段安装只有半截回显或转不停的 spinner：
/// - 与阶段机共用同一条回显线（见 [`TaskObserver`] 实现）
/// - `Drop` 兜底：`?` 提前返回、Ctrl-C 中断时收线并请求取消，
///   不留转不停的 spinner 挡住错误输出
///
/// 本地安装与远端安装传的是**同一个**实例：远端经 `Response::Task` 帧回传的
/// 阶段 / 进度 / 输出，走的也是这里的渲染，因此两边画面完全一致。
pub(crate) struct InstallSession {
    spinner: TaskSpinner,
    sink: BarSink,
    cancel: CancellationToken,
    downloading: std::sync::atomic::AtomicBool,
    /// 是否处于安装器阶段：这段时间回显线收起，只透传安装器输出
    installing: std::sync::atomic::AtomicBool,
    /// 安装器输出透传：与阶段回显共用同一条线
    installer_output: Arc<InstallerOutput>,
}

impl InstallSession {
    /// `message` 是初始阶段文案（探测中…），`enable` 传 [`crate::output::Printer::is_human`]
    pub(crate) fn new(message: impl Into<String>, enable: bool) -> Self {
        let message = message.into();
        let spinner = TaskSpinner::new(message.clone(), enable);
        let sink = BarSink::new(spinner.handle(), message);
        let cancel = CancellationToken::new();
        install_ctrl_c_handler(cancel.clone(), spinner.handle());
        let installer_output = Arc::new(InstallerOutput::new(
            spinner.handle(),
            cancel.clone(),
            enable,
        ));
        Self {
            spinner,
            sink,
            cancel,
            downloading: std::sync::atomic::AtomicBool::new(false),
            installing: std::sync::atomic::AtomicBool::new(false),
            installer_output,
        }
    }

    /// 安装器输出的实时透传句柄（测试与内部断言用）
    #[cfg(test)]
    pub(crate) fn installer_output(&self) -> Arc<InstallerOutput> {
        self.installer_output.clone()
    }

    /// 下载阶段文案：须在进入下载前设置（下载开始后会被进度条占用）
    pub(crate) fn set_download_message(&self, message: impl Into<String>) {
        self.sink.set_message(message);
    }

    /// 交互提示独占这一行前调用：让出回显行
    pub(crate) fn pause_for_prompt(&self) {
        self.spinner.finish();
    }

    /// 提示结束、回到阶段回显时调用：恢复这条线
    pub(crate) fn resume_after_prompt(&self) {
        self.spinner.restart();
    }

    /// 正常收尾：清除回显行（Drop 会再兜底一次，重复调用无害）
    pub(crate) fn finish(&self) {
        self.spinner.finish();
    }

    /// 取消令牌：传给 service 的安装调用
    pub(crate) fn cancel(&self) -> &CancellationToken {
        &self.cancel
    }

    /// 回显线句柄（测试用：断言阶段/进度都画在同一条线上）
    #[cfg(test)]
    fn handle_for_test(&self) -> ProgressBar {
        self.spinner.handle()
    }
}

impl TaskObserver for InstallSession {
    fn on_stage(&self, stage: TaskStage) {
        use std::sync::atomic::Ordering;
        // 取消后回显线已由 Ctrl-C 处理器收掉，任何迟到的阶段/进度都不再画
        if self.cancel.is_cancelled() {
            return;
        }
        match stage {
            // 下载：立即接管这条线，从发起请求到首字节之间也要有回显
            TaskStage::Downloading => {
                self.downloading.store(true, Ordering::Relaxed);
                self.sink.start();
            }
            // 安装器本体同样是一次下载；文案用阶段自带的「下载安装器…」，
            // 不沿用调用方为「核心 JAR」设的那句（两者描述的不是同一个文件）
            TaskStage::DownloadingInstaller => {
                self.downloading.store(true, Ordering::Relaxed);
                self.sink.set_message(stage.message());
                self.sink.start();
            }
            // 运行安装器：收起自己的回显线，只透传安装器输出
            //
            // 安装器自己刷进度，我们既拿不到它的真实进度，叠一条 bar/spinner
            // 只会两边抢同一行。这里先让出这条线（若刚下载完还要把 bar 交还），
            // 之后的状态全由安装器自己的输出表达。
            TaskStage::Installing => {
                if self.downloading.swap(false, Ordering::Relaxed) {
                    self.sink.finish();
                }
                self.installing.store(true, Ordering::Relaxed);
                self.spinner.finish();
                self.installer_output.detach();
            }
            // 下载之后的阶段（解压等）：进度条交还 spinner，继续画同一条线
            stage if self.downloading.load(Ordering::Relaxed) => {
                self.downloading.store(false, Ordering::Relaxed);
                self.sink.finish();
                self.spinner.set_message(stage.message());
            }
            // 安装器之后的阶段：重新挂回 spinner，继续在这一行回显
            stage if self.installing.swap(false, Ordering::Relaxed) => {
                self.spinner.restart();
                self.spinner.set_message(stage.message());
            }
            stage => self.spinner.set_message(stage.message()),
        }
    }
}

impl InstallerOutputSink for InstallSession {
    fn on_output(&self, line: &str) {
        // 安装器阶段之外的输出同样逐行透传：远端与本地走的是同一条渲染路径
        self.installer_output.on_output(line);
    }
}

impl ProgressSink for InstallSession {
    fn on_progress(&self, transferred: u64, total: Option<u64>) {
        use std::sync::atomic::Ordering;
        // 取消后不再回显（同 on_stage：线已收掉，迟到的进度画不上去）
        if self.cancel.is_cancelled() {
            return;
        }
        // 安装器阶段不画自己的进度：那条线已收起，真实进度由安装器输出表达
        if self.installing.load(Ordering::Relaxed) {
            return;
        }
        self.sink.on_progress(transferred, total);
    }
}

impl Drop for InstallSession {
    fn drop(&mut self) {
        self.cancel.cancel();
        // TaskSpinner::drop 清除回显行
    }
}

/// Ctrl-C 转为协作取消：第一次按下取消令牌并立即收掉回显线（下载循环在下个
/// 轮询粒度内退出、中间文件就地回收），第二次按下强制退出（此时清理可能被
/// 占用卡住，交给用户决断）
///
/// 不装处理器时 Ctrl-C 走进程默认终止，析构与清理路径全都不跑，`.tmp`
/// 半成品直接残留在磁盘上。非异步上下文（单测）下静默跳过。
fn install_ctrl_c_handler(cancel: CancellationToken, bar: ProgressBar) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    handle.spawn(async move {
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
        tracing::debug!("ctrl-c received, cancelling install");
        cancel.cancel();
        // 界面立刻停住：不等下载循环与清理走完
        bar.finish_and_clear();
        if tokio::signal::ctrl_c().await.is_ok() {
            // 第二次：不再等清理，立即退出（130 = 128 + SIGINT）
            std::process::exit(130);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 进度条的字节与速度都走 [`human_bytes`] 的 10 进制口径，不出现 MiB
    ///
    /// 用最小 `TermLike` 捕获渲染文本：indicatif 的 `InMemoryTerm` 要开
    /// `in_memory` feature（会引入 `vt100`），为一个断言不值得加依赖。
    #[test]
    fn bar_renders_decimal_units() {
        use indicatif::{ProgressDrawTarget, TermLike};
        use std::io;
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Debug)]
        struct Capture(Arc<Mutex<String>>);

        impl TermLike for Capture {
            fn width(&self) -> u16 {
                240
            }
            fn move_cursor_up(&self, _: usize) -> io::Result<()> {
                Ok(())
            }
            fn move_cursor_down(&self, _: usize) -> io::Result<()> {
                Ok(())
            }
            fn move_cursor_right(&self, _: usize) -> io::Result<()> {
                Ok(())
            }
            fn move_cursor_left(&self, _: usize) -> io::Result<()> {
                Ok(())
            }
            fn write_line(&self, s: &str) -> io::Result<()> {
                self.0.lock().expect("lock").push_str(s);
                Ok(())
            }
            fn write_str(&self, s: &str) -> io::Result<()> {
                self.0.lock().expect("lock").push_str(s);
                Ok(())
            }
            fn clear_line(&self) -> io::Result<()> {
                self.0.lock().expect("lock").clear();
                Ok(())
            }
            fn flush(&self) -> io::Result<()> {
                Ok(())
            }
        }

        let sink = Arc::new(Mutex::new(String::new()));
        let bar = ProgressBar::with_draw_target(
            Some(27_137_000),
            ProgressDrawTarget::term_like(Box::new(Capture(Arc::clone(&sink)))),
        );
        apply_bar_style(&bar, Arc::new(Mutex::new(SpeedMeter::default())));
        bar.set_message("取回整合包");
        bar.set_position(25_207_000);
        bar.tick();

        let rendered = sink.lock().expect("lock").clone();
        assert!(rendered.contains("25.21 MB"), "{rendered}");
        assert!(rendered.contains("27.14 MB"), "{rendered}");
        assert!(
            !rendered.contains("MiB"),
            "进度条不应再出现 MiB：{rendered}"
        );
        assert!(rendered.contains("/s"), "速度应带 /s 后缀：{rendered}");
        assert!(
            rendered.contains("ETA --"),
            "没有采样时 ETA 应显示 `--` 而不是编造数字：{rendered}"
        );
    }

    /// 速度计只反映最近窗口：3 秒内传了多少就报多少
    ///
    /// indicatif 内建估计器是 15 秒半衰期的双重平滑，开头几十秒显示的速度
    /// 与进度增长对不上；这里锁住「滑窗」这个语义。
    #[test]
    fn speed_meter_reports_recent_window() {
        let start = Instant::now();
        let mut meter = SpeedMeter::default();
        meter.reset(0, start);
        // 1 秒传 10 MB
        for step in 1..=10 {
            meter.record(step * 1_000_000, start + Duration::from_millis(100 * step));
        }
        let rate = meter.per_sec(start + Duration::from_secs(1));
        assert!(
            (9.0..11.0).contains(&(rate / 1_000_000.0)),
            "1 秒传 10 MB 应报约 10 MB/s，实际 {rate} B/s"
        );
    }

    /// 窗口外的旧采样必须丢弃：速度跟着当前吞吐走，而不是整个下载的平均值
    #[test]
    fn speed_meter_forgets_old_samples() {
        let start = Instant::now();
        let mut meter = SpeedMeter::default();
        meter.reset(0, start);
        // 每秒 1 MB，连续 10 秒
        for step in 1..=10 {
            meter.record(step * 1_000_000, start + Duration::from_secs(step));
        }
        assert!(
            meter.samples.len() <= 5,
            "只该留下 3 秒窗口内的采样，实际 {}",
            meter.samples.len()
        );
        let rate = meter.per_sec(start + Duration::from_secs(10));
        assert!(
            (0.9..=1.1).contains(&(rate / 1_000_000.0)),
            "稳定 1 MB/s 应报约 1 MB/s，实际 {rate} B/s"
        );
        // 停住超过阈值后速度归零，不再显示历史值
        assert!(
            meter.per_sec(start + Duration::from_secs(12)) < f64::EPSILON,
            "停住超过阈值后速度必须归零"
        );
    }

    /// 位置回退（分段退款 / 重下）时以新位置重新起算，不得出现负增量
    #[test]
    fn speed_meter_handles_rollback() {
        let start = Instant::now();
        let mut meter = SpeedMeter::default();
        meter.reset(0, start);
        meter.record(5_000_000, start + Duration::from_secs(1));
        meter.record(2_000_000, start + Duration::from_secs(2));
        let rate = meter.per_sec(start + Duration::from_secs(2));
        assert!(rate >= 0.0, "回退后速率不得为负：{rate}");
        assert_eq!(meter.samples.len(), 1, "回退应丢弃旧基线，只留当前点");
    }

    /// 已传满时 ETA 为 0，速率未知时不给数字（显示 `--`）
    #[test]
    fn speed_meter_eta() {
        let start = Instant::now();
        let mut meter = SpeedMeter::default();
        assert_eq!(meter.eta(0, start), Some(Duration::ZERO));
        assert_eq!(meter.eta(1000, start), None, "没有采样时不应编造 ETA");
        meter.reset(0, start);
        meter.record(10_000_000, start + Duration::from_secs(1));
        let eta = meter
            .eta(30_000_000, start + Duration::from_secs(1))
            .expect("有速率就该给 ETA");
        assert!(
            (2..=4).contains(&eta.as_secs()),
            "剩余 30 MB、10 MB/s 应约 3 秒：{eta:?}"
        );
    }

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

    /// 安装全程只用一条回显线：阶段 spinner → 下载进度条 → 阶段 spinner。
    ///
    /// 交互提示 `finish` 之后必须 `restart`，否则 indicatif 的 bar 处于终态，
    /// 后续阶段全部画不出来 —— 那几秒空白就是这么来的。
    #[test]
    fn install_session_keeps_one_live_line() {
        let session = InstallSession::new(TaskStage::Probing.message(), false);
        session.set_download_message("下载 Java 25 jdk");
        let line = session.handle_for_test();

        session.on_stage(TaskStage::Listing);
        assert_eq!(line.message(), TaskStage::Listing.message());

        // 交互提示独占这一行，结束后必须恢复
        session.pause_for_prompt();
        assert!(line.is_finished());
        session.resume_after_prompt();
        assert!(!line.is_finished());

        session.on_stage(TaskStage::Resolving);
        assert_eq!(line.message(), TaskStage::Resolving.message());

        // 进入下载：同一条线交给进度条，不等首字节
        session.on_stage(TaskStage::Downloading);
        assert_eq!(line.message(), "下载 Java 25 jdk");
        session.on_progress(1, Some(100));
        assert_eq!(line.length(), Some(100));

        // 解压：进度条交还 spinner，同一条线继续有回显
        session.on_stage(TaskStage::Extracting);
        assert_eq!(line.message(), TaskStage::Extracting.message());
        assert!(!line.is_finished());
    }

    /// 运行安装器期间不保留自己的回显线，也不画自己的进度
    ///
    /// 安装器自己会往终端刷进度，再叠一条 bar / spinner 就是两边抢同一行；
    /// 而且我们拿不到它的真实进度，画出来的是假进度。
    #[test]
    fn installing_stage_yields_the_line() {
        let session = InstallSession::new(TaskStage::Probing.message(), false);
        let line = session.handle_for_test();

        session.on_stage(TaskStage::DownloadingInstaller);
        assert_eq!(line.message(), TaskStage::DownloadingInstaller.message());

        // 进入安装器阶段：回显线收起
        session.on_stage(TaskStage::Installing);
        assert!(line.is_finished());

        // 安装器阶段的进度回调不再画到这条线上
        session.on_progress(64, Some(100));
        assert!(line.is_finished());

        // 安装器结束、进入登记核心：重新挂回 spinner
        session.on_stage(TaskStage::Finalizing);
        assert!(!line.is_finished());
        assert_eq!(line.message(), TaskStage::Finalizing.message());
    }

    /// 中途 drop（错误提前返回 / Ctrl-C）也要收线并请求取消
    #[test]
    fn install_session_drop_cancels() {
        let session = InstallSession::new("step", false);
        let handle = session.handle_for_test();
        let cancel = session.cancel().clone();
        drop(session);
        assert!(cancel.is_cancelled());
        assert!(handle.is_finished());
    }

    /// 续传时不得把磁盘上已有的字节算成本轮吞吐
    ///
    /// Ctrl-C 后重下会从残片继续：首个 `on_progress` 就到 `transferred = 已有字节`。
    /// indicatif 的估计器把这一跳当成「刚刚瞬间传完」，于是画面上出现几十 MiB/s
    /// 的速度与 `ETA 0s`，再随平滑窗口收敛掉回真实值 —— 用户看到的是
    /// 「显示很快但实际没在下」。切定长时重置估计器即以此为基线。
    #[test]
    fn resume_bytes_are_not_counted_as_throughput() {
        const RESUMED: u64 = 250 * 1024 * 1024;
        const TOTAL: u64 = 320 * 1024 * 1024;

        let spinner = TaskSpinner::new("取回整合包", false);
        let handle = spinner.handle();
        let sink = BarSink::new(handle.clone(), "取回整合包");

        sink.start();
        sink.on_progress(RESUMED, Some(TOTAL));

        // 位置照常前进（进度条要显示已完成的部分）
        assert_eq!(handle.position(), RESUMED);
        assert_eq!(handle.length(), Some(TOTAL));
        // 但吞吐从零起算：一个字节都还没传，速度必须是 0 而不是 RESUMED/秒
        assert!(
            sink.per_sec() < 1.0,
            "续传的已存在字节被算进了吞吐: {} B/s",
            sink.per_sec()
        );
    }

    /// 续传之后的真实增量仍然要统计
    #[test]
    fn resumed_transfer_still_reports_new_bytes() {
        const RESUMED: u64 = 250 * 1024 * 1024;
        const TOTAL: u64 = 320 * 1024 * 1024;
        const STEP: u64 = 64 * 1024;

        let spinner = TaskSpinner::new("取回整合包", false);
        let handle = spinner.handle();
        let sink = BarSink::new(handle.clone(), "取回整合包");

        sink.start();
        sink.on_progress(RESUMED, Some(TOTAL));
        std::thread::sleep(Duration::from_millis(300));
        sink.on_progress(RESUMED + STEP, Some(TOTAL));

        assert_eq!(handle.position(), RESUMED + STEP);
        assert!(
            sink.per_sec() > 0.0,
            "新传的字节没有被计入吞吐: {} B/s",
            sink.per_sec()
        );
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

    /// 安装器输出透传：session 提供同一令牌的句柄；取消 / 静默模式下不打印、不 panic
    #[test]
    fn installer_output_skips_when_cancelled_or_disabled() {
        let session = InstallSession::new("step", false);
        let output = session.installer_output();
        let cancel = session.cancel().clone();

        // 静默模式（JSON）：直接丢弃
        output.on_output("下载 libraries");

        // Ctrl-C 之后回显线已收掉，迟到的安装器输出不再打印
        cancel.cancel();
        output.on_output("after cancel");
    }
}
