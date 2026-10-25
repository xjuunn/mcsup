//! `read_system_log` 工具：读取本机系统事件日志。
//!
//! 排查平台相关问题时用（例如通知没弹、进程被拦截）。只在 Windows 上可用，
//! 其他平台返回明确的不支持说明，而不是空结果。
//!
//! 直接调用 Win32 事件日志 API，不起 `PowerShell`：低频操作也保持一致，
//! 且不引入脚本注入面。

use rmcp::handler::server::wrapper::Parameters;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::ToolError;

/// 单条消息上限。
const MAX_MESSAGE_CHARS: usize = 1500;
/// 单次返回条数上限。
const MAX_ENTRIES: usize = 200;

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct ReadSystemLogRequest {
    /// 日志名，例如 `Microsoft-Windows-PushNotification-Platform/Operational`。
    /// 省略则列出可用日志名。
    #[serde(default)]
    pub(crate) log_name: Option<String>,
    /// 只要这些事件 id；省略要全部。
    #[serde(default)]
    pub(crate) event_ids: Option<Vec<u32>>,
    /// 只取最近多少分钟内的条目，默认 30。
    #[serde(default)]
    pub(crate) minutes: Option<u32>,
    /// 最多返回多少条，默认 20，最大 200。
    #[serde(default)]
    pub(crate) limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct LogEntry {
    /// ISO 8601 本地时间。
    pub(crate) time: String,
    pub(crate) event_id: u32,
    pub(crate) level: String,
    pub(crate) provider: String,
    pub(crate) message: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct ReadSystemLogResponse {
    log_name: Option<String>,
    /// 可用日志名（仅在不指定 `log_name` 时返回）。
    #[serde(skip_serializing_if = "Option::is_none")]
    available: Option<Vec<String>>,
    entries: Vec<LogEntry>,
    /// 是否因上限被截断。
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
}

pub(crate) async fn read_system_log(
    Parameters(request): Parameters<ReadSystemLogRequest>,
) -> Result<String, String> {
    read_system_log_inner(request).await.map_err(|e| e.render())
}

async fn read_system_log_inner(request: ReadSystemLogRequest) -> Result<String, ToolError> {
    #[cfg(not(target_os = "windows"))]
    {
        let _ = request;
        Err(ToolError::Unsupported(
            "read_system_log 目前只支持 Windows 事件日志".to_owned(),
        ))
    }

    #[cfg(target_os = "windows")]
    {
        let limit = request.limit.unwrap_or(20).clamp(1, MAX_ENTRIES);
        // 上限一周：再长的范围用事件日志工具更合适，工具应保持快。
        let minutes = request.minutes.unwrap_or(30).clamp(1, 60 * 24 * 7);

        let Some(log_name) = request
            .log_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            let available = crate::platform::list_event_logs().await?;
            let response = ReadSystemLogResponse {
                log_name: None,
                available: Some(available),
                entries: Vec::new(),
                truncated: false,
                hint: Some("指定 log_name 以读取条目".to_owned()),
            };
            return serde_json::to_string_pretty(&response)
                .map_err(|error| ToolError::Failed(format!("序列化结果失败: {error}")));
        };

        let mut entries = crate::platform::read_event_log(log_name, minutes).await?;
        if let Some(ids) = &request.event_ids {
            entries.retain(|entry| ids.contains(&entry.event_id));
        }

        let truncated = entries.len() > limit;
        entries.truncate(limit);
        for entry in &mut entries {
            entry.message = truncate(&entry.message, MAX_MESSAGE_CHARS);
        }

        let response = ReadSystemLogResponse {
            log_name: Some(log_name.to_owned()),
            available: None,
            entries,
            truncated,
            hint: None,
        };
        serde_json::to_string_pretty(&response)
            .map_err(|error| ToolError::Failed(format!("序列化结果失败: {error}")))
    }
}

/// 压平空白并截断，事件消息里常有大量换行。
fn truncate(raw: &str, limit: usize) -> String {
    let squashed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if squashed.chars().count() <= limit {
        return squashed;
    }
    squashed.chars().take(limit).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::truncate;

    #[test]
    fn squeezes_whitespace() {
        assert_eq!(truncate("a\n\n  b", 100), "a b");
    }

    #[test]
    fn truncates_on_char_boundary() {
        let out = truncate(&"错".repeat(50), 10);
        assert_eq!(out.chars().count(), 11);
        assert!(out.ends_with('…'));
    }
}
