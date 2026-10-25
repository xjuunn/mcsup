//! Linux 通知、截屏与文件管理器定位。
//!
//! `notify-send` 在多数桌面发行版随桌面环境安装。没有该命令时返回明确的
//! 错误，而不是静默吞掉，便于调用方判断是否需要换用其他提醒方式。

use std::path::Path;

use super::{CaptureError, Notification, NotifyError, RevealError, Urgency};

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

/// 用 `gnome-screenshot` / `grim` / `import` 中第一个可用的截屏。
///
/// 不同桌面环境的工具不同，这里按优先级探测；都不可用时返回
/// [`CaptureError::Unavailable`]，不伪造空白图。
pub(crate) fn capture_screen(path: &Path) -> Result<(), CaptureError> {
    let target = path.display().to_string();

    let candidates: &[(&str, &[&str])] = &[
        ("grim", &[]),
        ("gnome-screenshot", &["-f"]),
        ("import", &["-window", "root"]),
    ];

    for (program, prefix) in candidates {
        let mut args: Vec<String> = prefix.iter().map(|s| (*s).to_owned()).collect();
        args.push(target.clone());
        match std::process::Command::new(program)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
        {
            Ok(status) if status.success() => return Ok(()),
            // 命令存在但执行失败：换下一个，全部失败后再报错。
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(CaptureError::Platform(format!(
                    "启动 {program} 失败: {error}"
                )));
            }
        }
    }

    Err(CaptureError::Unavailable(
        "未找到可用截屏工具（grim / gnome-screenshot / import）".to_owned(),
    ))
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
