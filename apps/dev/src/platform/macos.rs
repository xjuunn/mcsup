//! macOS 通知：优先 `terminal-notifier`，缺失时回退 `osascript`。
//!
//! `osascript` 随系统提供；`terminal-notifier`（brew 安装）发送者名称可控，
//! 存在时优先使用。脚本内容不在 shell 里插值，只做 AppleScript 字符串转义。

use std::path::Path;

use super::{CaptureError, Notification, NotifyError, RevealError, Urgency};

const TERMINAL_NOTIFIER: &str = "terminal-notifier";

/// 该平台使用的通知通道，供 `list_tools` 报告能力。
pub(crate) const CHANNEL: &str = "macOS 通知中心（terminal-notifier，缺失时 osascript）";

pub(crate) async fn notify(notification: &Notification) -> Result<(), NotifyError> {
    if super::command_exists(TERMINAL_NOTIFIER).await {
        return super::run_command(
            TERMINAL_NOTIFIER,
            &[
                "-title".to_owned(),
                notification.title.clone(),
                "-message".to_owned(),
                notification.body.clone(),
                "-group".to_owned(),
                "mcsup".to_owned(),
            ],
            &[],
        )
        .await;
    }

    let mut script = format!(
        "display notification \"{}\" with title \"{}\"",
        escape_applescript(&notification.body),
        escape_applescript(&notification.title),
    );
    if notification.urgency == Urgency::Critical {
        script.push_str(" sound name \"Sosumi\"");
    }

    super::run_command("osascript", &["-e".to_owned(), script], &[]).await
}

fn escape_applescript(raw: &str) -> String {
    raw.replace('\\', "\\\\").replace('"', "\\\"")
}

/// 用系统自带的 `screencapture` 截取整个屏幕。
///
/// `-x` 关闭快门音：工具调用不应制造意外噪音。
pub(crate) fn capture_screen(path: &Path) -> Result<(), CaptureError> {
    let status = std::process::Command::new("screencapture")
        .arg("-x")
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|error| CaptureError::Platform(format!("启动 screencapture 失败: {error}")))?;

    if status.success() {
        return Ok(());
    }
    Err(CaptureError::Platform(format!(
        "screencapture 退出码 {}（可能缺少「屏幕录制」权限）",
        status.code().unwrap_or(-1)
    )))
}

/// 用 `open -R` 在访达中选中路径。
pub(crate) fn reveal_in_file_manager(path: &Path) -> Result<(), RevealError> {
    let mut command = std::process::Command::new("open");
    if path.is_dir() {
        command.arg(path);
    } else {
        command.arg("-R").arg(path);
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| RevealError::Platform(format!("启动 open 失败: {error}")))
}

#[cfg(test)]
mod tests {
    use super::escape_applescript;

    #[test]
    fn escapes_quotes_and_backslashes() {
        assert_eq!(escape_applescript("a\"b"), "a\\\"b");
        assert_eq!(escape_applescript("a\\b"), "a\\\\b");
    }
}
