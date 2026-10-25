//! Windows 通知：直接调用 `WinRT` `Windows.UI.Notifications`（Toast）。
//!
//! 不启动 `PowerShell`、不依赖外部命令：文案在 Rust 侧拼进系统提供的 Toast 模板，
//! 由 `WinRT` 构建 `ToastNotification` 并交给 `ToastNotificationManager` 显示。

use windows::UI::Notifications::{ToastNotification, ToastNotificationManager, ToastTemplateType};
use windows::core::HSTRING;

use super::{Notification, NotifyError};

/// 该平台使用的通知通道，供 `list_tools` 报告能力。
pub(crate) const CHANNEL: &str = "Windows Toast（Rust 直连 Windows.UI.Notifications）";

/// 短名形式的 AppUserModelID。桌面进程没有自己的 AUMID 时用它即可显示。
const APP_ID: &str = "mcsup-dev-tools";

fn fill_text(
    xml: &windows::Data::Xml::Dom::XmlDocument,
    index: u32,
    value: &str,
) -> Result<(), NotifyError> {
    let nodes = xml
        .GetElementsByTagName(&HSTRING::from("text"))
        .map_err(|error| NotifyError::Platform(format!("定位通知文本节点失败: {error}")))?;
    let node = nodes
        .GetAt(index)
        .map_err(|error| NotifyError::Platform(format!("读取通知文本节点失败: {error}")))?;
    // `CreateTextNode` 由 MSXML 负责转义，不需要在 Rust 侧手写 XML 转义。
    let text = xml
        .CreateTextNode(&HSTRING::from(value))
        .map_err(|error| NotifyError::Platform(format!("创建文本节点失败: {error}")))?;
    node.AppendChild(&text)
        .map_err(|error| NotifyError::Platform(format!("写入通知文本失败: {error}")))?;
    Ok(())
}

fn build_toast(notification: &Notification) -> Result<ToastNotification, NotifyError> {
    // ToastText02：第一行标题，第二行正文，都是纯文本节点，无法注入标记。
    let xml = ToastNotificationManager::GetTemplateContent(ToastTemplateType::ToastText02)
        .map_err(|error| NotifyError::Platform(format!("读取通知模板失败: {error}")))?;

    fill_text(&xml, 0, &notification.title)?;
    if !notification.body.is_empty() {
        fill_text(&xml, 1, &notification.body)?;
    }

    let toast = ToastNotification::CreateToastNotification(&xml)
        .map_err(|error| NotifyError::Platform(format!("构建通知失败: {error}")))?;

    Ok(toast)
}

pub(crate) async fn notify(notification: &Notification) -> Result<(), NotifyError> {
    // WinRT 是同步 COM 调用，放到阻塞线程池，避免占住 MCP 的异步执行器。
    let owned = notification.clone();
    tokio::task::spawn_blocking(move || show(&owned))
        .await
        .map_err(|error| NotifyError::Platform(format!("通知任务异常: {error}")))?
}
fn show(notification: &Notification) -> Result<(), NotifyError> {
    let toast = build_toast(notification)?;
    let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID))
        .map_err(|error| NotifyError::Platform(format!("创建通知器失败: {error}")))?;

    // 记录发送前的历史数量，用于确认系统是否真的收录。
    let before = history_len();
    notifier
        .Show(&toast)
        .map_err(|error| NotifyError::Platform(format!("显示通知失败: {error}")))?;

    verify_recorded(before)
}

/// 读通知中心历史，确认这条通知真的被系统收录。
///
/// `Show` 返回成功只代表调用没报错；被通知设置 / 专注助手拦截时，
/// 系统会接受调用但不产生记录，这里把这种情况如实报成失败。
fn verify_recorded(before: Option<u32>) -> Result<(), NotifyError> {
    let Some(before) = before else {
        // 读不到历史（老系统或权限受限）时无法判定，交给调用方按成功处理。
        return Ok(());
    };

    // 系统落库不是同步的，给一点时间；不影响正确性，只影响判定准确度。
    std::thread::sleep(std::time::Duration::from_millis(300));
    let Some(after) = history_len() else {
        return Ok(());
    };

    if after > before {
        return Ok(());
    }

    Err(NotifyError::NotDisplayed)
}

fn history_len() -> Option<u32> {
    let history = ToastNotificationManager::GetDefault()
        .ok()?
        .History()
        .ok()?;
    let items = history.GetHistory().ok()?;
    items.Size().ok()
}
