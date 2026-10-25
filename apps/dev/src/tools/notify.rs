//! `send_notification` 工具：跨平台系统通知。

use rmcp::handler::server::wrapper::Parameters;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::platform::{self, Notification, Urgency};
/// 标题上限。过长标题在 Windows / macOS 会截断，提前拒绝以便调用方修正。
const MAX_TITLE_CHARS: usize = 128;
/// 正文上限。
const MAX_BODY_CHARS: usize = 1024;

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct SendNotificationRequest {
    /// 通知标题，例如「mcsup 构建完成」。1-128 字符。
    title: String,
    /// 通知正文，例如「scripts/check.sh 全部通过」。0-1024 字符。
    #[serde(default)]
    body: String,
    /// 紧急程度：low / normal / critical，默认 normal。
    #[serde(default)]
    urgency: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct SendNotificationResponse {
    sent: bool,
    title: String,
    urgency: &'static str,
    channel: &'static str,
}

pub(crate) async fn send_notification(
    Parameters(request): Parameters<SendNotificationRequest>,
) -> Result<String, String> {
    let title = request.title.trim();
    if title.is_empty() {
        return Err("title 不能为空".to_owned());
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err(format!("title 不能超过 {MAX_TITLE_CHARS} 字符"));
    }
    if request.body.chars().count() > MAX_BODY_CHARS {
        return Err(format!("body 不能超过 {MAX_BODY_CHARS} 字符"));
    }

    let urgency = match request.urgency.as_deref() {
        None => Urgency::Normal,
        Some(raw) => Urgency::parse(raw)
            .ok_or_else(|| format!("urgency 只能是 low / normal / critical，收到 {raw:?}"))?,
    };

    let notification = Notification {
        title: title.to_owned(),
        body: request.body,
        urgency,
    };

    platform::notify(&notification)
        .await
        .map_err(|error| error.to_string())?;

    let response = SendNotificationResponse {
        sent: true,
        title: notification.title,
        urgency: match urgency {
            Urgency::Low => "low",
            Urgency::Normal => "normal",
            Urgency::Critical => "critical",
        },
        channel: channel(),
    };
    Ok(serde_json::to_string(&response).unwrap_or_else(|_| "{\"sent\":true}".to_owned()))
}

/// 当前平台的通知实现说明，供 `list_tools` 工具报告能力。
pub(crate) fn channel() -> &'static str {
    platform::CHANNEL
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_empty_title() {
        let result = send_notification(Parameters(SendNotificationRequest {
            title: "   ".to_owned(),
            body: String::new(),
            urgency: None,
        }))
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn rejects_unknown_urgency() {
        let result = send_notification(Parameters(SendNotificationRequest {
            title: "t".to_owned(),
            body: String::new(),
            urgency: Some("high".to_owned()),
        }))
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn rejects_overlong_title() {
        let result = send_notification(Parameters(SendNotificationRequest {
            title: "a".repeat(MAX_TITLE_CHARS + 1),
            body: String::new(),
            urgency: None,
        }))
        .await;
        assert!(result.is_err());
    }
}
