//! `take_screenshot` 工具：截取屏幕并存成 PNG。
//!
//! 截图涉及屏幕隐私，只保存到工程内的 `.mcsup-dev/screenshots/`（已忽略），
//! 返回绝对路径，不把图片内容塞进 MCP 响应。

use rmcp::handler::server::wrapper::Parameters;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::ToolError;
use super::workspace;

/// 返回给调用方的截图能力说明。
pub(crate) fn channel() -> &'static str {
    if cfg!(target_os = "windows") {
        "不可用（Windows 截屏需 GDI，与 forbid(unsafe_code) 冲突）"
    } else if cfg!(target_os = "macos") {
        "screencapture（系统自带，需要屏幕录制权限）"
    } else {
        "grim / gnome-screenshot / import（按可用性探测）"
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct TakeScreenshotRequest {
    /// 文件名前缀，默认 `screen`。实际文件名会追加时间戳。
    #[serde(default)]
    pub(crate) prefix: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct TakeScreenshotResponse {
    /// 保存路径的绝对路径。
    path: String,
    /// 文件字节数。
    bytes: u64,
    /// 生成该截图的工具。
    tool: String,
}

pub(crate) async fn take_screenshot(
    Parameters(request): Parameters<TakeScreenshotRequest>,
) -> Result<String, String> {
    take_screenshot_inner(request).await.map_err(|e| e.render())
}

async fn take_screenshot_inner(request: TakeScreenshotRequest) -> Result<String, ToolError> {
    let root = workspace::workspace_root()?;
    let dir = root.join(".mcsup-dev").join("screenshots");
    std::fs::create_dir_all(&dir)
        .map_err(|error| ToolError::Io(format!("创建 {} 失败: {error}", dir.display())))?;

    let prefix = sanitize_prefix(request.prefix.as_deref().unwrap_or("screen"))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let path = dir.join(format!("{prefix}-{stamp}.png"));

    // 平台实现是同步的系统调用，放到阻塞线程池避免占住异步执行器。
    let target = path.clone();
    tokio::task::spawn_blocking(move || crate::platform::capture_screen(&target))
        .await
        .map_err(|error| ToolError::Failed(format!("截屏任务异常: {error}")))?
        .map_err(|error| match error {
            // 加前缀说明来源：调用方看到裸的「需要 GDI」不知道是哪一步失败的。
            crate::platform::CaptureError::Unavailable(message) => {
                ToolError::Unsupported(format!("take_screenshot 不可用：{message}"))
            }
            crate::platform::CaptureError::Platform(message) => {
                ToolError::Failed(format!("截屏失败：{message}"))
            }
        })?;

    let metadata = std::fs::metadata(&path)
        .map_err(|error| ToolError::Io(format!("读取截图 {} 失败: {error}", path.display())))?;

    let response = TakeScreenshotResponse {
        path: path.display().to_string(),
        bytes: metadata.len(),
        tool: channel().to_owned(),
    };
    serde_json::to_string_pretty(&response)
        .map_err(|error| ToolError::Failed(format!("序列化结果失败: {error}")))
}

/// 前缀只能含字母、数字、下划线与连字符，避免路径穿越。
fn sanitize_prefix(raw: &str) -> Result<String, ToolError> {
    let name = raw.trim();
    if name.is_empty() {
        return Ok("screen".to_owned());
    }
    if name.len() > 64 {
        return Err(ToolError::InvalidInput(
            "prefix 不能超过 64 字节".to_owned(),
        ));
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(ToolError::InvalidInput(
            "prefix 只能包含字母、数字、下划线与连字符".to_owned(),
        ));
    }
    Ok(name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_safe_prefix() {
        assert_eq!(sanitize_prefix("a_b-1").expect("ok"), "a_b-1");
        assert_eq!(sanitize_prefix("  ").expect("ok"), "screen");
    }

    #[test]
    fn rejects_path_traversal() {
        for raw in ["../evil", "a/b", "a\\b", "a..b", "有中文"] {
            assert!(
                sanitize_prefix(raw).is_err(),
                "{raw:?} 应被拒绝，避免写到截图目录之外"
            );
        }
    }
}
