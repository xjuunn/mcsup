//! `get_build_diagnostics` 工具：把 `cargo check` 的机器可读输出解析成结构化诊断。
//!
//! 用 `--message-format=json` 而不是正则抓人类可读文本：前者是 cargo 的稳定
//! 契约，字段有文档；后者随 rustc 文案变化就失效。

use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::ToolError;
use super::workspace;

const CHECK_TIMEOUT: Duration = Duration::from_secs(600);
/// 单条诊断内嵌源码片段的上限，避免一个长错误占满上下文。
const MAX_SNIPPET_CHARS: usize = 1200;
/// 返回的诊断条数上限。
const MAX_DIAGNOSTICS: usize = 50;

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct GetBuildDiagnosticsRequest {
    /// 是否包含警告，默认 false（只要错误）。
    #[serde(default)]
    pub(crate) include_warnings: bool,
    /// 检查目标 crate；省略检查整个 workspace。
    #[serde(default)]
    pub(crate) package: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Diagnostic {
    /// error / warning。
    level: String,
    /// rustc 错误码，例如 E0308；无码时为 null。
    code: Option<String>,
    /// 首行消息。
    message: String,
    /// 对应的 `.rs` 文件路径（相对工程根）。
    file: Option<String>,
    line: Option<u64>,
    column: Option<u64>,
    /// 源码片段，已截断。
    snippet: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct GetBuildDiagnosticsResponse {
    /// 编译是否成功。
    success: bool,
    /// 是否因超时被终止。
    timed_out: bool,
    /// 诊断总数（可能大于返回条数）。
    total: usize,
    /// 是否因超出上限被截断。
    truncated: bool,
    diagnostics: Vec<Diagnostic>,
    /// 编译失败但没有可解析诊断时的兜底信息（例如链接错误）。
    raw_tail: Option<String>,
}

pub(crate) async fn get_build_diagnostics(
    Parameters(request): Parameters<GetBuildDiagnosticsRequest>,
) -> Result<String, String> {
    get_build_diagnostics_inner(request)
        .await
        .map_err(|e| e.render())
}

async fn get_build_diagnostics_inner(
    request: GetBuildDiagnosticsRequest,
) -> Result<String, ToolError> {
    let root = workspace::workspace_root()?;

    let mut args = vec![
        "check".to_owned(),
        "--workspace".to_owned(),
        "--all-targets".to_owned(),
        "--all-features".to_owned(),
        "--message-format=json".to_owned(),
    ];
    if let Some(package) = request.package.as_deref() {
        if package.trim().is_empty() {
            return Err(ToolError::InvalidInput("package 不能为空字符串".to_owned()));
        }
        args.insert(2, "-p".to_owned());
        args.insert(3, package.to_owned());
    }

    let output = workspace::run("cargo", &args, Some(&root), CHECK_TIMEOUT).await?;

    let mut diagnostics = Vec::new();
    for line in output.stdout.lines() {
        // cargo 在 stderr 上也会混入进度的 JSON，这里只处理 stdout 的编译消息。
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("reason").and_then(|r| r.as_str()) != Some("compiler-message") {
            continue;
        }
        let Some(message) = value.get("message") else {
            continue;
        };
        let level = message
            .get("level")
            .and_then(|l| l.as_str())
            .unwrap_or_default();
        if level != "error" && !(request.include_warnings && level == "warning") {
            continue;
        }
        // 只取顶层消息：子诊断（`children`）重复同一文件位置，展开会让输出膨胀。
        diagnostics.push(to_diagnostic(message, &root));
    }

    let total = diagnostics.len();
    diagnostics.truncate(MAX_DIAGNOSTICS);

    let raw_tail = if output.success || !diagnostics.is_empty() {
        None
    } else {
        let stderr = output.stderr.trim();
        (!stderr.is_empty()).then(|| {
            stderr
                .chars()
                .rev()
                .take(MAX_SNIPPET_CHARS)
                .collect::<String>()
                .chars()
                .rev()
                .collect()
        })
    };

    let response = GetBuildDiagnosticsResponse {
        success: output.success,
        timed_out: output.timed_out,
        total,
        truncated: total > MAX_DIAGNOSTICS,
        diagnostics,
        raw_tail,
    };

    serde_json::to_string_pretty(&response)
        .map_err(|error| ToolError::Failed(format!("序列化结果失败: {error}")))
}

fn to_diagnostic(message: &serde_json::Value, root: &std::path::Path) -> Diagnostic {
    let text = |key: &str| message.get(key).and_then(|v| v.as_str()).map(str::to_owned);

    let code = message
        .get("code")
        .and_then(|c| c.get("code"))
        .and_then(|c| c.as_str())
        .map(str::to_owned);

    let mut file = None;
    let mut line = None;
    let mut column = None;
    let mut snippet = None;

    if let Some(primary) = message
        .get("spans")
        .and_then(|s| s.as_array())
        .and_then(|spans| spans.iter().find(|span| span["is_primary"] == true))
    {
        file = primary
            .get("file_name")
            .and_then(|f| f.as_str())
            .map(|name| relativize(name, root));
        line = primary
            .get("line_start")
            .and_then(serde_json::Value::as_u64);
        column = primary
            .get("column_start")
            .and_then(serde_json::Value::as_u64);
        if let Some(text) = primary.get("text").and_then(|t| t.as_array()) {
            let rendered = text
                .iter()
                .filter_map(|snippet| snippet.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n");
            let trimmed = rendered.trim();
            if !trimmed.is_empty() {
                snippet = Some(trim_at_char(trimmed, MAX_SNIPPET_CHARS));
            }
        }
    }

    Diagnostic {
        level: text("level").unwrap_or_default(),
        code,
        message: text("message").unwrap_or_default(),
        file,
        line,
        column,
        snippet,
    }
}

/// 把绝对路径转成相对工程根的路径；不在工程内则原样返回。
fn relativize(path: &str, root: &std::path::Path) -> String {
    let candidate = std::path::Path::new(path);
    match candidate.strip_prefix(root) {
        Ok(rest) => rest.to_string_lossy().replace('\\', "/"),
        Err(_) => path.replace('\\', "/"),
    }
}

fn trim_at_char(raw: &str, limit: usize) -> String {
    if raw.chars().count() <= limit {
        return raw.to_owned();
    }
    raw.chars().take(limit).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rustc_error_json() {
        let raw = r#"{
            "reason": "compiler-message",
            "message": {
                "level": "error",
                "code": { "code": "E0308" },
                "message": "mismatched types",
                "spans": [{
                    "is_primary": true,
                    "file_name": "apps/dev/src/main.rs",
                    "line_start": 12,
                    "column_start": 5,
                    "text": [{ "text": "    let x: u8 = \"s\";" }]
                }]
            }
        }"#;
        let value: serde_json::Value = serde_json::from_str(raw).expect("valid json");
        let diagnostic = to_diagnostic(
            value.get("message").expect("message"),
            std::path::Path::new(""),
        );
        assert_eq!(diagnostic.level, "error");
        assert_eq!(diagnostic.code.as_deref(), Some("E0308"));
        assert_eq!(diagnostic.line, Some(12));
        assert_eq!(diagnostic.column, Some(5));
        assert!(diagnostic.snippet.expect("snippet").contains("let x"));
    }

    #[test]
    fn relativizes_paths_under_root() {
        let root = std::path::Path::new("/repo");
        assert_eq!(relativize("/repo/src/a.rs", root), "src/a.rs");
        assert_eq!(relativize("/other/a.rs", root), "/other/a.rs");
    }

    #[test]
    fn trims_snippet_on_char_boundary() {
        let long = "编".repeat(MAX_SNIPPET_CHARS + 10);
        let trimmed = trim_at_char(&long, MAX_SNIPPET_CHARS);
        assert!(trimmed.ends_with('…'));
        assert_eq!(trimmed.chars().count(), MAX_SNIPPET_CHARS + 1);
    }
}
