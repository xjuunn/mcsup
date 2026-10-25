//! `open_path` 工具：在系统文件管理器中定位路径。
//!
//! 路径相对工程根解释，并限制在工程根之内：工具只服务于开发场景，
//! 不应变成任意路径的打开入口。

use rmcp::handler::server::wrapper::Parameters;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::ToolError;
use super::workspace;

/// 返回给调用方的定位能力说明。
pub(crate) fn channel() -> &'static str {
    if cfg!(target_os = "windows") {
        "explorer.exe"
    } else if cfg!(target_os = "macos") {
        "open -R"
    } else {
        "xdg-open"
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct OpenPathRequest {
    /// 要定位的路径，相对工程根；省略则打开 worktree 根目录。
    #[serde(default)]
    pub(crate) path: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct OpenPathResponse {
    /// 实际打开或选中的绝对路径。
    path: String,
    opened: bool,
}

pub(crate) async fn open_path(
    Parameters(request): Parameters<OpenPathRequest>,
) -> Result<String, String> {
    open_path_inner(request).await.map_err(|e| e.render())
}

async fn open_path_inner(request: OpenPathRequest) -> Result<String, ToolError> {
    let root = workspace::workspace_root()?;

    let target = match request.path.as_deref() {
        Some(raw) if !raw.trim().is_empty() => workspace::resolve_in_workspace(raw)?,
        _ => root.clone(),
    };

    // 规范化后再判断是否越界，避免 `sub/../../etc` 这类路径绕过前缀检查。
    let canonical_root = root
        .canonicalize()
        .map_err(|error| ToolError::Io(format!("解析工程根失败: {error}")))?;
    let canonical_target = target.canonicalize().map_err(|error| {
        ToolError::NotFound(format!(
            "路径不存在或不可访问: {}（{error}）",
            target.display()
        ))
    })?;
    if !canonical_target.starts_with(&canonical_root) {
        return Err(ToolError::InvalidInput(format!(
            "只能打开工程根之内的路径：{}",
            canonical_target.display()
        )));
    }

    let revealed = canonical_target.clone();
    tokio::task::spawn_blocking(move || crate::platform::reveal_in_file_manager(&revealed))
        .await
        .map_err(|error| ToolError::Failed(format!("定位任务异常: {error}")))?
        .map_err(|error| match error {
            crate::platform::RevealError::NotAvailable(message) => {
                ToolError::Unsupported(format!("open_path 不可用：{message}"))
            }
            crate::platform::RevealError::Platform(message) => {
                ToolError::Failed(format!("定位失败：{message}"))
            }
        })?;

    let response = OpenPathResponse {
        // 规范化结果带 Windows 扩展长度前缀（`\\?\`），对调用方没有意义，
        // 展示时去掉；判定是否越界仍在带前缀的路径上做。
        path: displayable(&canonical_target),
        opened: true,
    };
    serde_json::to_string_pretty(&response)
        .map_err(|error| ToolError::Failed(format!("序列化结果失败: {error}")))
}

/// 去掉 Windows 规范化路径的扩展长度前缀，便于展示。
fn displayable(path: &std::path::Path) -> String {
    let raw = path.display().to_string();
    let stripped = raw.strip_prefix(r"\\?\");
    stripped.unwrap_or(&raw).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_missing_path() {
        let request = OpenPathRequest {
            path: Some("this/does/not/exist".to_owned()),
        };
        let result = open_path_inner(request).await;
        assert!(
            matches!(result, Err(ToolError::NotFound(_) | ToolError::Io(_))),
            "不存在的路径应报错而不是打开别处"
        );
    }

    #[tokio::test]
    async fn rejects_path_outside_workspace() {
        let request = OpenPathRequest {
            path: Some("../../..".to_owned()),
        };
        let result = open_path_inner(request).await;
        assert!(
            matches!(result, Err(ToolError::InvalidInput(_))),
            "越界路径应被拒绝"
        );
    }
}
