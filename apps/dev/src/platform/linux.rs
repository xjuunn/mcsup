//! Linux 通知：经 `notify-send`（libnotify）发到桌面通知服务。
//!
//! `notify-send` 在多数桌面发行版随桌面环境安装。没有该命令时返回明确的
//! 错误，而不是静默吞掉，便于调用方判断是否需要换用其他提醒方式。

use super::{Notification, NotifyError, Urgency};

/// 该平台使用的通知通道，供 `list_tools` 报告能力。
pub(crate) const CHANNEL: &str = "libnotify（notify-send）";

pub(crate) async fn notify(notification: &Notification) -> Result<(), NotifyError> {
    let urgency = match notification.urgency {
        Urgency::Low => "low",
        Urgency::Normal => "normal",
        Urgency::Critical => "critical",
    };

    super::run_command(
        "notify-send",
        &[
            "--app-name=mcsup".to_owned(),
            format!("--urgency={urgency}"),
            "--".to_owned(),
            notification.title.clone(),
            notification.body.clone(),
        ],
        &[],
    )
    .await
}
