//! 平台实现注册表。
//!
//! 工具只调用这里的函数，不直接处理平台差异；各平台文件按目标系统条件编译，
//! 交叉编译时不会引入其他平台的依赖。

#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "windows")]
pub(crate) use windows::{
    CHANNEL, capture_screen, list_event_logs, notify, read_event_log, reveal_in_file_manager,
};

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
pub(crate) use macos::{CHANNEL, capture_screen, notify, reveal_in_file_manager};

#[cfg(all(unix, not(target_os = "macos")))]
mod linux;

#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) use linux::{CHANNEL, capture_screen, notify, reveal_in_file_manager};

/// 发送一条系统通知的错误。
#[derive(Debug, thiserror::Error)]
pub(crate) enum NotifyError {
    /// 平台 API 直接返回的错误（不经过外部进程）。
    #[error("{0}")]
    Platform(String),
    /// 通知已提交，但系统没有收录：通常是被通知设置或专注助手拦截。
    #[error("通知已提交但未进入通知中心，请检查系统通知设置")]
    NotDisplayed,
    /// 外部通知命令启动失败（macOS / Linux 回退实现）。
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[error("通知命令 {program} 启动失败: {source}")]
    Spawn {
        program: &'static str,
        source: std::io::Error,
    },
    /// 外部通知命令返回失败。
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[error("通知命令 {program} 退出码 {code}：{stderr}")]
    Failed {
        program: &'static str,
        code: i32,
        stderr: String,
    },
    /// 外部通知命令超时。
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[error("通知命令 {program} 超时")]
    Timeout { program: &'static str },
}

/// 外部通知命令超过该时长视为失败，避免调用方无限等待。
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) const NOTIFY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 一次系统通知的内容。
#[derive(Debug, Clone)]
pub(crate) struct Notification {
    pub(crate) title: String,
    pub(crate) body: String,
    /// `low` / `normal` / `critical`（默认 `normal`），由各平台映射成自己的分级。
    #[cfg_attr(
        target_os = "windows",
        allow(
            dead_code,
            reason = "Windows Toast 的分级只影响音效与停留策略，尚未接入"
        )
    )]
    pub(crate) urgency: Urgency,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Urgency {
    Low,
    Normal,
    Critical,
}

impl Urgency {
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw {
            "low" => Some(Self::Low),
            "normal" => Some(Self::Normal),
            "critical" => Some(Self::Critical),
            _ => None,
        }
    }
}

/// 截屏失败的原因。
#[derive(Debug, thiserror::Error)]
pub(crate) enum CaptureError {
    /// Windows 上截屏不可用，永远只构造 `Unavailable`。
    #[cfg_attr(
        target_os = "windows",
        allow(dead_code, reason = "Windows 无可用截屏通道")
    )]
    #[error("{0}")]
    Platform(String),
    #[error("截屏命令未安装或不可用：{0}")]
    Unavailable(String),
}

/// 在文件管理器中定位路径失败的原因。
#[derive(Debug, thiserror::Error)]
pub(crate) enum RevealError {
    #[error("{0}")]
    Platform(String),
    /// Linux 上 `xdg-open` 可能整体不可用；Windows / macOS 只有 `Platform`。
    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "仅 Linux 区分命令缺失")
    )]
    #[error("无法在文件管理器中定位：{0}")]
    NotAvailable(String),
}

/// 执行通知命令并等待结果。子进程由 tokio 托管，超时后 kill。
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) async fn run_command(
    program: &'static str,
    args: &[String],
    envs: &[(&str, &str)],
) -> Result<(), NotifyError> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .envs(envs.iter().copied())
        .stdin(std::process::Stdio::null());

    let output = tokio::time::timeout(NOTIFY_TIMEOUT, command.output())
        .await
        .map_err(|_| NotifyError::Timeout { program })?
        .map_err(|source| NotifyError::Spawn { program, source })?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(NotifyError::Failed {
        program,
        code: output.status.code().unwrap_or(-1),
        stderr,
    })
}

/// 探测 PATH 上是否存在可执行命令（仅 macOS 回退时用）。
///
/// 先直接执行 `program --version`，不依赖 shell 的 `command -v`。
#[cfg(target_os = "macos")]
pub(crate) fn command_exists(program: &str) -> bool {
    std::process::Command::new(program)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}
