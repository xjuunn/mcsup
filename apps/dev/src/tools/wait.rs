//! `wait_for_user` 工具：阻塞等待用户确认。
//!
//! MCP 是请求-响应模型，工具执行期间 agent 无法继续对话，所以要有一个独立于
//! 对话的通道。这里用文件：
//!
//! 1. 工具在 `.mcsup-dev/requests/<id>.json` 写入待确认的问题
//! 2. 用户回复：在 `.mcsup-dev/replies/<id>.json` 写入 `{"answer": "..."}`
//! 3. 工具轮询到回复文件即返回；超时返回 `timed_out`
//!
//! 目录放在工程内便于用户直接用编辑器创建回复文件。`.mcsup-dev/` 已在
//! `.gitignore` 中忽略，不会污染工作树。

use std::path::{Path, PathBuf};
use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::ToolError;
use super::workspace;

/// 轮询间隔。足够短以感知用户操作，足够长以免空转。
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// 默认等待时长。
const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// 等待时长上限，防止 agent 被永久挂住。
const MAX_TIMEOUT_SECS: u64 = 3600;

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct WaitForUserRequest {
    /// 需要用户确认的问题。要具体，例如「合入 master 前请确认：...」
    pub(crate) question: String,
    /// 等待秒数，默认 300，最大 3600。
    #[serde(default)]
    pub(crate) timeout_secs: Option<u64>,
    /// 超时前是否顺便发一条系统通知提醒用户，默认 true。
    #[serde(default = "default_true")]
    pub(crate) notify: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Serialize)]
pub(crate) struct WaitForUserResponse {
    /// 用户是否已回复。
    acknowledged: bool,
    /// 用户的回答；未回复时为 null。
    answer: Option<String>,
    /// 是否因超时返回。
    timed_out: bool,
    /// 等待了多久。
    waited_secs: u64,
    /// 创建回复文件时用户应写入的精确路径。
    reply_path: String,
}

pub(crate) async fn wait_for_user(
    Parameters(request): Parameters<WaitForUserRequest>,
) -> Result<String, String> {
    wait_for_user_inner(request).await.map_err(|e| e.render())
}

async fn wait_for_user_inner(request: WaitForUserRequest) -> Result<String, ToolError> {
    let question = request.question.trim();
    if question.is_empty() {
        return Err(ToolError::InvalidInput("question 不能为空".to_owned()));
    }

    let timeout_secs = request.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
    if timeout_secs == 0 || timeout_secs > MAX_TIMEOUT_SECS {
        return Err(ToolError::InvalidInput(format!(
            "timeout_secs 必须在 1..={MAX_TIMEOUT_SECS} 之间"
        )));
    }

    let root = workspace::workspace_root()?;
    let dirs = RequestDirs::new(&root);
    dirs.prepare()?;

    let id = new_request_id();
    let request_path = dirs.requests.join(format!("{id}.json"));
    let reply_path = dirs.replies.join(format!("{id}.json"));

    let payload = serde_json::json!({
        "id": id,
        "question": question,
        "reply_path": reply_path.to_string_lossy(),
        "reply_format": { "answer": "你的回答" },
    });
    std::fs::write(
        &request_path,
        serde_json::to_vec_pretty(&payload)
            .map_err(|error| ToolError::Failed(format!("序列化请求失败: {error}")))?,
    )
    .map_err(|error| ToolError::Io(format!("写入 {} 失败: {error}", request_path.display())))?;

    if request.notify {
        // 通知失败不影响等待：用户仍可能在编辑器里看到请求文件。
        let notification = crate::platform::Notification {
            title: "需要确认".to_owned(),
            body: truncate(question, 200),
            urgency: crate::platform::Urgency::Critical,
        };
        if let Err(error) = crate::platform::notify(&notification).await {
            tracing::warn!(%error, "wait_for_user 的通知发送失败");
        }
    }

    let started = std::time::Instant::now();
    let deadline = started + Duration::from_secs(timeout_secs);

    let answer = loop {
        if let Some(answer) = read_reply(&reply_path)? {
            break Some(answer);
        }
        if std::time::Instant::now() >= deadline {
            break None;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    };

    // 无论成功还是超时都清理请求文件，避免目录堆积。
    let _ = std::fs::remove_file(&request_path);

    let acknowledged = answer.is_some();
    let response = WaitForUserResponse {
        acknowledged,
        answer,
        timed_out: !acknowledged,
        waited_secs: started.elapsed().as_secs(),
        reply_path: reply_path.display().to_string(),
    };
    serde_json::to_string_pretty(&response)
        .map_err(|error| ToolError::Failed(format!("序列化结果失败: {error}")))
}

struct RequestDirs {
    requests: PathBuf,
    replies: PathBuf,
}

impl RequestDirs {
    fn new(root: &Path) -> Self {
        let base = root.join(".mcsup-dev");
        Self {
            requests: base.join("requests"),
            replies: base.join("replies"),
        }
    }

    fn prepare(&self) -> Result<(), ToolError> {
        for dir in [&self.requests, &self.replies] {
            std::fs::create_dir_all(dir)
                .map_err(|error| ToolError::Io(format!("创建 {} 失败: {error}", dir.display())))?;
        }
        Ok(())
    }
}

/// 读取并解析回复文件。
///
/// 文件不存在返回 `Ok(None)`；存在但内容非法返回错误——静默当成没回复，
/// 会让用户以为自己已经答过了。
fn read_reply(path: &Path) -> Result<Option<String>, ToolError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(ToolError::Io(format!(
                "读取回复 {} 失败: {error}",
                path.display()
            )));
        }
    };

    let parsed: Reply = serde_json::from_slice(&bytes).map_err(|error| {
        ToolError::InvalidInput(format!(
            "回复文件 {} 不是合法 JSON（{error}），应写 {{\"answer\": \"...\"}}",
            path.display()
        ))
    })?;

    if parsed.answer.trim().is_empty() {
        return Err(ToolError::InvalidInput(format!(
            "回复文件 {} 的 answer 为空",
            path.display()
        )));
    }
    Ok(Some(parsed.answer))
}

#[derive(Debug, Deserialize)]
struct Reply {
    answer: String,
}

/// 生成请求 id：时间戳 + 进程内计数，避免同一毫秒内并发调用撞号。
fn new_request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}-{seq}")
}

fn truncate(raw: &str, limit: usize) -> String {
    if raw.chars().count() <= limit {
        return raw.to_owned();
    }
    raw.chars().take(limit).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_question() {
        let request = WaitForUserRequest {
            question: "  ".to_owned(),
            timeout_secs: None,
            notify: false,
        };
        let result = block_on(wait_for_user_inner(request));
        assert!(matches!(result, Err(ToolError::InvalidInput(_))));
    }

    #[test]
    fn rejects_out_of_range_timeout() {
        for value in [0, MAX_TIMEOUT_SECS + 1] {
            let request = WaitForUserRequest {
                question: "确认?".to_owned(),
                timeout_secs: Some(value),
                notify: false,
            };
            let result = block_on(wait_for_user_inner(request));
            assert!(
                matches!(result, Err(ToolError::InvalidInput(_))),
                "timeout {value} 应被拒绝"
            );
        }
    }

    #[test]
    fn reads_valid_reply() {
        let dir = std::env::temp_dir().join("mcsup-dev-tools-wait-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create dir");
        let path = dir.join("r.json");

        assert!(read_reply(&path).expect("missing is ok").is_none());

        std::fs::write(&path, r#"{"answer":"同意"}"#).expect("write");
        assert_eq!(read_reply(&path).expect("valid").as_deref(), Some("同意"));

        std::fs::write(&path, r#"{"answer":"   "}"#).expect("write");
        assert!(read_reply(&path).is_err(), "空白回答应报错");

        std::fs::write(&path, b"not json").expect("write");
        assert!(read_reply(&path).is_err(), "非法 JSON 应报错");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn request_ids_are_unique() {
        let a = new_request_id();
        let b = new_request_id();
        assert_ne!(a, b);
    }

    use std::future::Future;

    /// 测试里同步执行 async 函数，避免为单个断言引入额外依赖。
    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime")
            .block_on(future)
    }
}
