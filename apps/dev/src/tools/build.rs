//! `run_check` 工具：运行仓库质量门禁并返回结构化结果。
//!
//! 门禁脚本本身负责「跑什么」，这里只负责执行与解析，不复制任何检查项——
//! 判定标准必须只有一处，否则脚本改了工具会悄悄过时。

use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::ToolError;
use super::workspace;

/// 门禁最长运行时间。全量 check + test + feature powerset 在冷缓存下可能超过
/// 十分钟，超过这个上限按超时报告，而不是让调用方无限等待。
const CHECK_TIMEOUT: Duration = Duration::from_secs(1800);

/// 返回给调用方的首条错误上限，避免把整段编译输出塞进上下文。
const MAX_ERROR_CHARS: usize = 2000;

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct RunCheckRequest {
    /// 只跑指定步骤，例如 `["fmt", "clippy"]`。省略则跑完整门禁。
    ///
    /// 步骤名见 check.sh：architecture / fmt / check / clippy / test / doc。
    #[serde(default)]
    pub(crate) steps: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
pub(crate) struct RunCheckResponse {
    /// 结果状态，见 [`CheckOutcome`]。
    outcome: CheckOutcome,
    /// 实际执行到的步骤（按顺序）。
    ran: Vec<String>,
    /// 失败的步骤名；全部通过时为 null。
    failed_step: Option<String>,
    /// 失败步骤的退出码；超时或未失败时为 null。
    failed_code: Option<i32>,
    /// 失败时的首条错误摘要，截断到 [`MAX_ERROR_CHARS`]。
    error_excerpt: Option<String>,
}

/// 门禁结果。把「跑没跑全量」「过没过」「有没有超时」合成一个值，
/// 避免调用方从三个独立布尔量里自行推导、误把子集通过当作门禁通过。
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub(crate) enum CheckOutcome {
    /// 全量门禁通过，可以用作提交依据。
    FullGatePassed,
    /// 全量门禁被超时终止。
    FullGateTimedOut,
    /// 全量门禁失败。
    FullGateFailed,
    /// 只跑了指定步骤且全部通过，不能当作提交依据。
    SubsetPassed,
    /// 只跑了指定步骤且被超时终止。
    SubsetTimedOut,
    /// 只跑了指定步骤且失败。
    SubsetFailed,
}

impl CheckOutcome {
    fn new(full_gate: bool, success: bool, timed_out: bool) -> Self {
        match (full_gate, success, timed_out) {
            (true, _, true) => Self::FullGateTimedOut,
            (true, true, false) => Self::FullGatePassed,
            (true, false, false) => Self::FullGateFailed,
            (false, _, true) => Self::SubsetTimedOut,
            (false, true, false) => Self::SubsetPassed,
            (false, false, false) => Self::SubsetFailed,
        }
    }
}

pub(crate) async fn run_check(
    Parameters(request): Parameters<RunCheckRequest>,
) -> Result<String, String> {
    run_check_inner(request).await.map_err(|e| e.render())
}

async fn run_check_inner(request: RunCheckRequest) -> Result<String, ToolError> {
    let root = workspace::workspace_root()?;
    let steps = normalize_steps(request.steps.as_deref())?;

    let mut args = vec!["scripts/check.sh".to_owned()];
    if let Some(steps) = &steps {
        args.extend(steps.iter().cloned());
    }

    let output = workspace::run("sh", &args, Some(&root), CHECK_TIMEOUT).await?;
    let combined = format!("{}{}", output.stdout, output.stderr);

    let parsed = parse_check_output(&combined);

    let error_excerpt = if output.success {
        None
    } else {
        Some(excerpt(&combined, parsed.failed_step.as_deref()))
    };

    let response = RunCheckResponse {
        outcome: CheckOutcome::new(steps.is_none(), output.success, output.timed_out),
        ran: parsed.ran,
        failed_step: parsed.failed_step,
        failed_code: output.code.filter(|_| !output.success),
        error_excerpt,
    };

    serde_json::to_string_pretty(&response)
        .map_err(|error| ToolError::Failed(format!("序列化结果失败: {error}")))
}

/// 校验并规范化步骤参数。
///
/// 步骤名限定在门禁脚本实际支持的集合内：拼错的步骤名会让脚本静默跑完整个
/// 门禁（或什么都不跑），调用方拿到「通过」却是假的。
fn normalize_steps(steps: Option<&[String]>) -> Result<Option<Vec<String>>, ToolError> {
    const KNOWN: &[&str] = &[
        "architecture",
        "fmt",
        "check",
        "clippy",
        "test",
        "doc",
        "deny",
    ];
    let Some(steps) = steps else {
        return Ok(None);
    };
    if steps.is_empty() {
        return Ok(None);
    }
    for step in steps {
        if !KNOWN.contains(&step.as_str()) {
            return Err(ToolError::InvalidInput(format!(
                "未知步骤 {step:?}，可选：{}",
                KNOWN.join(" / ")
            )));
        }
    }
    Ok(Some(steps.to_vec()))
}

#[derive(Debug, Default)]
struct ParsedCheck {
    ran: Vec<String>,
    failed_step: Option<String>,
}

/// 解析 `scripts/check.sh` 的输出。
///
/// 脚本的契约（见 `scripts/check.sh`）：
/// - 每步开始打印 `==> <step>`
/// - 失败打印 `失败于：<step>（<code>）` 到 stderr
/// - 全部通过打印 `门禁通过`
fn parse_check_output(output: &str) -> ParsedCheck {
    let mut parsed = ParsedCheck::default();
    // 只保留最近的若干行用于摘要，避免内存随输出线性增长。
    for line in output.lines() {
        let line = line.trim();
        if let Some(step) = line.strip_prefix("==> ") {
            let name = step.split_whitespace().next().unwrap_or_default();
            if !name.is_empty() && !parsed.ran.iter().any(|s| s == name) {
                parsed.ran.push(name.to_owned());
            }
        }
        if let Some(rest) = line.strip_prefix("失败于：") {
            let name = rest
                .split(['（', '('])
                .next()
                .unwrap_or_default()
                .trim()
                .to_owned();
            if !name.is_empty() {
                parsed.failed_step = Some(name);
            }
        }
    }
    parsed
}

/// 取失败步骤附近的输出作为摘要。
///
/// 优先从失败行往前找最近一条错误，找不到就取整个输出的尾部——尾部通常是
/// 真正出错的位置。
fn excerpt(output: &str, failed_step: Option<&str>) -> String {
    let marker = failed_step.map(|step| format!("失败于：{step}"));
    let slice = marker
        .as_deref()
        .and_then(|marker| output.find(marker))
        .map_or_else(
            || {
                // 没有失败行：取输出的尾部，尾部通常才是真正出错的位置。
                &output[last_chars_start(output, MAX_ERROR_CHARS)..]
            },
            |index| {
                // 有失败行：把步骤开始到失败行的输出一起带上。
                let start = output[..index]
                    .rfind("==> ")
                    .unwrap_or_else(|| last_chars_start(&output[..index], MAX_ERROR_CHARS));
                &output[start..]
            },
        );

    if slice.chars().count() <= MAX_ERROR_CHARS {
        return slice.to_owned();
    }
    let cut: String = slice.chars().take(MAX_ERROR_CHARS).collect();
    format!("{cut}\n…（已截断）")
}

/// 保留末尾 `chars` 个字符时的起始字节偏移。
///
/// 按字符而非字节回退：`output.len() - chars` 在含多字节字符时会切进字符内部。
fn last_chars_start(text: &str, chars: usize) -> usize {
    text.char_indices()
        .rev()
        .nth(chars)
        .map_or(0, |(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_passed_output() {
        let output =
            "\n==> architecture\n==> fmt\n==> check\n==> clippy\n==> test\n==> doc\n\n门禁通过。\n";
        let parsed = parse_check_output(output);
        assert_eq!(
            parsed.ran,
            vec!["architecture", "fmt", "check", "clippy", "test", "doc"]
        );
        assert!(parsed.failed_step.is_none());
    }

    #[test]
    fn parses_failed_step() {
        let output = "==> architecture\n==> fmt\n==> clippy\nerror[E0308]: mismatched types\n失败于：clippy（101）\n";
        let parsed = parse_check_output(output);
        assert_eq!(parsed.failed_step.as_deref(), Some("clippy"));
        let excerpt = excerpt(output, parsed.failed_step.as_deref());
        assert!(excerpt.contains("E0308"));
        assert!(!excerpt.contains("architecture"));
    }

    #[test]
    fn rejects_unknown_step() {
        let err = normalize_steps(Some(&["cargo".to_owned()])).unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[test]
    fn empty_steps_mean_full_gate() {
        assert!(normalize_steps(Some(&[])).unwrap().is_none());
        assert!(normalize_steps(None).unwrap().is_none());
    }

    #[test]
    fn subset_pass_is_not_full_gate_pass() {
        // 子集通过必须与全量通过区分，否则调用方会拿它当提交依据。
        for (full_gate, success, timed_out, expected) in [
            (true, true, false, "full_gate_passed"),
            (true, false, false, "full_gate_failed"),
            (true, true, true, "full_gate_timed_out"),
            (false, true, false, "subset_passed"),
            (false, false, false, "subset_failed"),
            (false, true, true, "subset_timed_out"),
        ] {
            let outcome = CheckOutcome::new(full_gate, success, timed_out);
            let rendered = serde_json::to_string(&outcome).expect("serialize");
            assert!(
                rendered.contains(expected),
                "{full_gate}/{success}/{timed_out} 应得到 {expected}，实际 {rendered}"
            );
        }
    }

    #[test]
    fn excerpt_is_cut_on_char_boundary() {
        let long = "错".repeat(MAX_ERROR_CHARS * 2);
        let cut = excerpt(&long, None);
        assert!(cut.starts_with('错'));
        assert!(cut.ends_with("（已截断）"));
    }
}
