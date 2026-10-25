//! macOS 通知：优先 `terminal-notifier`，缺失时回退 `osascript`。
//!
//! `osascript` 随系统提供；`terminal-notifier`（brew 安装）发送者名称可控，
//! 存在时优先使用。脚本内容不在 shell 里插值，只做 AppleScript 字符串转义。

use super::{Notification, NotifyError, Urgency};

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

#[cfg(test)]
mod tests {
    use super::escape_applescript;

    #[test]
    fn escapes_quotes_and_backslashes() {
        assert_eq!(escape_applescript("a\"b"), "a\\\"b");
        assert_eq!(escape_applescript("a\\b"), "a\\\\b");
    }
}
