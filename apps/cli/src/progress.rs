use indicatif::{ProgressBar, ProgressStyle};
use mcsup::port::{CancellationToken, InstallerOutputSink, ProgressSink};
use mcsup::service::{InstallStage, StageSink};
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
    bar: ProgressBar,
    /// 回显状态
    state: Mutex<SinkState>,
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
        resume(&self.bar);
        apply_unknown_length_style(&self.bar);
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
            self.bar.set_message(state.message.clone());
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
/// - 与阶段机共用同一条回显线（见 [`StageSink`] 实现）
/// - `Drop` 兜底：`?` 提前返回、Ctrl-C 中断时收线并请求取消，
///   不留转不停的 spinner 挡住错误输出
pub(crate) struct InstallSession {
    spinner: TaskSpinner,
    sink: BarSink,
    cancel: CancellationToken,
    downloading: std::sync::atomic::AtomicBool,
    /// 是否处于安装器阶段：这段时间回显线收起，只透传安装器输出
    installing: std::sync::atomic::AtomicBool,
    /// 安装器输出透传：与阶段回显共用同一条线
    installer_output: std::sync::Arc<InstallerOutput>,
}

impl InstallSession {
    /// `message` 是初始阶段文案（探测中…），`enable` 传 [`crate::output::Printer::is_human`]
    pub(crate) fn new(message: impl Into<String>, enable: bool) -> Self {
        let message = message.into();
        let spinner = TaskSpinner::new(message.clone(), enable);
        let sink = BarSink::new(spinner.handle(), message);
        let cancel = CancellationToken::new();
        install_ctrl_c_handler(cancel.clone(), spinner.handle());
        let installer_output = std::sync::Arc::new(InstallerOutput::new(
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

    /// 安装器输出的实时透传句柄：交给 [`InstallerContext::output`]
    ///
    /// [`InstallerContext::output`]: mcsup::service::InstallerContext::output
    pub(crate) fn installer_output(&self) -> std::sync::Arc<InstallerOutput> {
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

impl StageSink for InstallSession {
    fn on_stage(&self, stage: InstallStage) {
        use std::sync::atomic::Ordering;
        // 取消后回显线已由 Ctrl-C 处理器收掉，任何迟到的阶段/进度都不再画
        if self.cancel.is_cancelled() {
            return;
        }
        match stage {
            // 下载：立即接管这条线，从发起请求到首字节之间也要有回显
            InstallStage::Downloading => {
                self.downloading.store(true, Ordering::Relaxed);
                self.sink.start();
            }
            // 运行安装器：收起自己的回显线，只透传安装器输出
            //
            // 安装器自己刷进度，我们既拿不到它的真实进度，叠一条 bar/spinner
            // 只会两边抢同一行。这里先让出这条线（若刚下载完还要把 bar 交还），
            // 之后的状态全由安装器自己的输出表达。
            InstallStage::Installing => {
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
        let session = InstallSession::new(InstallStage::Probing.message(), false);
        session.set_download_message("下载 Java 25 jdk");
        let line = session.handle_for_test();

        session.on_stage(InstallStage::Listing);
        assert_eq!(line.message(), InstallStage::Listing.message());

        // 交互提示独占这一行，结束后必须恢复
        session.pause_for_prompt();
        assert!(line.is_finished());
        session.resume_after_prompt();
        assert!(!line.is_finished());

        session.on_stage(InstallStage::Resolving);
        assert_eq!(line.message(), InstallStage::Resolving.message());

        // 进入下载：同一条线交给进度条，不等首字节
        session.on_stage(InstallStage::Downloading);
        assert_eq!(line.message(), "下载 Java 25 jdk");
        session.on_progress(1, Some(100));
        assert_eq!(line.length(), Some(100));

        // 解压：进度条交还 spinner，同一条线继续有回显
        session.on_stage(InstallStage::Extracting);
        assert_eq!(line.message(), InstallStage::Extracting.message());
        assert!(!line.is_finished());
    }

    /// 运行安装器期间不保留自己的回显线，也不画自己的进度
    ///
    /// 安装器自己会往终端刷进度，再叠一条 bar / spinner 就是两边抢同一行；
    /// 而且我们拿不到它的真实进度，画出来的是假进度。
    #[test]
    fn installing_stage_yields_the_line() {
        let session = InstallSession::new(InstallStage::Probing.message(), false);
        let line = session.handle_for_test();

        session.on_stage(InstallStage::DownloadingInstaller);
        assert_eq!(line.message(), InstallStage::DownloadingInstaller.message());

        // 进入安装器阶段：回显线收起
        session.on_stage(InstallStage::Installing);
        assert!(line.is_finished());

        // 安装器阶段的进度回调不再画到这条线上
        session.on_progress(64, Some(100));
        assert!(line.is_finished());

        // 安装器结束、进入登记核心：重新挂回 spinner
        session.on_stage(InstallStage::Finalizing);
        assert!(!line.is_finished());
        assert_eq!(line.message(), InstallStage::Finalizing.message());
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
