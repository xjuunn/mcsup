//! Windows 通知、文件管理器定位与事件日志。
//!
//! 通知直接调用 `WinRT` `Windows.UI.Notifications`（Toast），不启动
//! `PowerShell`：文案在 Rust 侧拼进系统提供的 Toast 模板，由 `WinRT` 构建
//! `ToastNotification` 并交给 `ToastNotificationManager` 显示。
//!
//! 事件日志与资源管理器定位同样不起脚本：事件日志经 `wevtutil`
//! （系统自带、无脚本解释）读取。

use std::path::Path;

use windows::UI::Notifications::{ToastNotification, ToastNotificationManager, ToastTemplateType};
use windows::core::HSTRING;

use super::{Notification, NotifyError, RevealError};
use crate::tools::ToolError;
use crate::tools::logs::LogEntry;

/// 该平台使用的通知通道，供 `list_tools` 报告能力。
pub(crate) const CHANNEL: &str = "Windows Toast（Rust 直连 Windows.UI.Notifications）";

/// 短名形式的 AppUserModelID。桌面进程没有自己的 AUMID 时用它即可显示。
const APP_ID: &str = "mcsup-dev-tools";

/// `wevtutil` 单次调用的超时。
const WEVTUTIL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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

    ToastNotification::CreateToastNotification(&xml)
        .map_err(|error| NotifyError::Platform(format!("构建通知失败: {error}")))
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

/// 在资源管理器中选中指定路径。
pub(crate) fn reveal_in_file_manager(path: &Path) -> Result<(), RevealError> {
    // explorer.exe 是系统自带的文件管理器，不经脚本解释参数。
    let arg = if path.is_dir() {
        path.display().to_string()
    } else {
        format!("/select,{}", path.display())
    };
    std::process::Command::new("explorer.exe")
        .arg(arg)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| RevealError::Platform(format!("启动 explorer.exe 失败: {error}")))
}

/// 列出所有事件日志通道名，按字典序排序。
///
/// 经 `wevtutil el`：系统自带的命令行工具，参数是固定字面量，不接受用户输入，
/// 无脚本解释面。`forbid(unsafe_code)` 下这是唯一可用的通道枚举方式。
pub(crate) async fn list_event_logs() -> Result<Vec<String>, ToolError> {
    let output =
        crate::tools::workspace::run("wevtutil", &["el".to_owned()], None, WEVTUTIL_TIMEOUT)
            .await?;

    if !output.success {
        return Err(ToolError::Failed(format!(
            "wevtutil el 失败：{}",
            output.stderr.trim()
        )));
    }

    let mut names: Vec<String> = output
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    names.sort();
    Ok(names)
}

/// 读取指定事件日志最近 `minutes` 分钟内的条目，最新的在前。
///
/// 经 `wevtutil qe <channel> /f:xml` 取原始 XML，在 Rust 侧解析字段：
/// 不走 `/f:text`，因为后者的消息文本依赖消息 DLL，缺 DLL 时只剩占位符。
///
/// `channel` 来自调用方，必须防止它被当作 `wevtutil` 的选项——以 `-` 或 `/`
/// 开头的值直接拒绝。
pub(crate) async fn read_event_log(
    log_name: &str,
    minutes: u32,
) -> Result<Vec<LogEntry>, ToolError> {
    if log_name.starts_with('-') || log_name.starts_with('/') {
        return Err(ToolError::InvalidInput(format!(
            "log_name 不能以 - 或 / 开头：{log_name:?}"
        )));
    }

    let ms = u64::from(minutes) * 60 * 1000;
    let query = format!("*[System[TimeCreated[timediff(@SystemTime) <= {ms}]]]");
    let args = vec![
        "qe".to_owned(),
        log_name.to_owned(),
        format!("/q:{query}"),
        "/f:xml".to_owned(),
        "/rd:true".to_owned(),
    ];

    let output = crate::tools::workspace::run("wevtutil", &args, None, WEVTUTIL_TIMEOUT).await?;
    if !output.success {
        let stderr = output.stderr.trim();
        // 日志名错误时 wevtutil 返回非零，这里归为「找不到」而不是内部错误。
        return Err(ToolError::NotFound(format!(
            "读取事件日志 {log_name} 失败（日志名是否正确？）：{stderr}"
        )));
    }

    Ok(parse_events_xml(&output.stdout))
}

/// 解析 `wevtutil qe /f:xml` 的输出：多个 `<Event>` 紧邻拼接。
fn parse_events_xml(raw: &str) -> Vec<LogEntry> {
    let mut entries = Vec::new();
    let mut rest = raw;
    while let Some(start) = rest.find("<Event ") {
        let slice = &rest[start..];
        let Some(end) = slice.find("</Event>") else {
            break;
        };
        let block = &slice[..end + "</Event>".len()];
        entries.push(parse_event_xml(block));
        rest = &slice[end + "</Event>".len()..];
    }
    entries
}

/// 从一个 `<Event>` 块里取出 time / `event_id` / level / provider / message。
fn parse_event_xml(xml: &str) -> LogEntry {
    let (system, body) = xml
        .find("<System")
        .and_then(|start| {
            let system = &xml[start..];
            let end = system.find("</System>")?;
            Some((&system[..end], &system[end..]))
        })
        .unwrap_or((xml, xml));

    let time = extract_element(system, "TimeCreated")
        .and_then(|tag| extract_attr(&tag, "SystemTime"))
        .unwrap_or_default();
    let event_id = extract_element(system, "EventID")
        .and_then(|raw| raw.trim().parse().ok())
        .unwrap_or(0);
    let provider = extract_element(system, "Provider")
        .and_then(|tag| extract_attr(&tag, "Name"))
        .unwrap_or_default();
    let level = extract_element(system, "Level")
        .map(|raw| level_name(raw.trim()))
        .unwrap_or_default();

    let message = collect_event_data(body);

    LogEntry {
        time,
        event_id,
        level,
        provider,
        message,
    }
}

/// 把 `<EventData>` 里的若干 `<Data>` 拼成一行。
fn collect_event_data(xml: &str) -> String {
    let mut parts = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<Data") {
        let after = &rest[start..];
        let Some(open_end) = after.find('>') else {
            break;
        };
        // 自闭合的 <Data /> 没有内容。
        if open_end > 0 && after[..open_end].ends_with('/') {
            rest = &after[open_end + 1..];
            continue;
        }
        let body = &after[open_end + 1..];
        let Some(close) = body.find("</Data>") else {
            break;
        };
        let value = unescape_xml(&body[..close]);
        if !value.trim().is_empty() {
            parts.push(value);
        }
        rest = &body[close + "</Data>".len()..];
    }
    parts.join(" ")
}

/// 取标签内容；自闭合标签返回整个标签（便于再取属性）。
fn extract_element(xml: &str, name: &str) -> Option<String> {
    let open = format!("<{name}");
    let start = xml.find(&open)?;
    let tag = &xml[start..];
    let open_end = tag.find('>')?;
    // 自闭合标签：内容为空，返回标签本身供属性提取。
    if tag[..open_end].ends_with('/') {
        return Some(tag[..open_end].to_owned());
    }
    let rest = &tag[open_end + 1..];
    let close_tag = format!("</{name}>");
    let close = rest.find(&close_tag)?;
    Some(unescape_xml(&rest[..close]))
}

fn extract_attr(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let end = tag[start..].find('"')? + start;
    Some(unescape_xml(&tag[start..end]))
}

fn level_name(raw: &str) -> String {
    match raw {
        "1" => "critical",
        "2" => "error",
        "3" => "warning",
        "4" => "information",
        "5" => "verbose",
        _ => "unknown",
    }
    .to_owned()
}

fn unescape_xml(raw: &str) -> String {
    raw.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SINGLE: &str = r#"<Event xmlns="http://schemas.microsoft.com/win/2004/08/events/event">
<System>
  <Provider Name="Microsoft-Windows-PushNotification-Platform" Guid="{x}" />
  <EventID>3153</EventID>
  <Level>4</Level>
  <TimeCreated SystemTime="2026-10-25T06:27:30.1234567Z" />
</System>
<EventData>
  <Data Name="AppUserModelId">mcsup-dev-tools</Data>
  <Data Name="TrackingId">5902</Data>
</EventData>
</Event>"#;

    #[test]
    fn parses_single_event_fields() {
        let entry = parse_event_xml(SINGLE);
        assert_eq!(entry.event_id, 3153);
        assert_eq!(entry.level, "information");
        assert_eq!(
            entry.provider,
            "Microsoft-Windows-PushNotification-Platform"
        );
        assert_eq!(entry.time, "2026-10-25T06:27:30.1234567Z");
        assert!(entry.message.contains("mcsup-dev-tools"));
        assert!(entry.message.contains("5902"));
    }

    #[test]
    fn splits_concatenated_events() {
        let raw = format!("{SINGLE}\n{SINGLE}");
        assert_eq!(parse_events_xml(&raw).len(), 2);
    }

    #[test]
    fn maps_level_numbers() {
        assert_eq!(level_name("2"), "error");
        assert_eq!(level_name("3"), "warning");
        assert_eq!(level_name("9"), "unknown");
    }

    #[test]
    fn unescapes_xml_entities() {
        assert_eq!(unescape_xml("a&lt;b&gt;&amp;c"), "a<b>&c");
    }

    #[test]
    fn rejects_option_like_log_name() {
        let err = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(read_event_log("-config", 1))
            .expect_err("应以 - 开头的日志名被拒绝");
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }
}
