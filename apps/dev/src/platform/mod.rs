//! 平台实现注册表。
//!
//! 工具只调用这里的函数，不直接处理平台差异；各平台文件按目标系统条件编译，
//! 交叉编译时不会引入其他平台的依赖。

#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "windows")]
pub(crate) use windows::{CHANNEL, notify};

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
pub(crate) use macos::{CHANNEL, notify};

#[cfg(all(unix, not(target_os = "macos")))]
mod linux;

#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) use linux::{CHANNEL, notify};

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
    /// Windows 的 `WinRT` Toast 没有分级的原生入口，该字段在 Windows 上不参与渲染。
    #[cfg_attr(
        target_os = "windows",
        allow(dead_code, reason = "Windows 无对应分级入口")
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
