//! Linux 通知与文件管理器定位。
//!
//! `notify-send` 在多数桌面发行版随桌面环境安装。没有该命令时返回明确的
//! 错误，而不是静默吞掉，便于调用方判断是否需要换用其他提醒方式。

use std::path::Path;

use super::{Notification, NotifyError, RevealError, Urgency};

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

/// 用 `xdg-open` 打开路径所在目录。
pub(crate) fn reveal_in_file_manager(path: &Path) -> Result<(), RevealError> {
    let target = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent().unwrap_or(path).to_path_buf()
    };
    std::process::Command::new("xdg-open")
        .arg(target)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| RevealError::NotAvailable(format!("xdg-open 不可用: {error}")))
}
