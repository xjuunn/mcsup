//! MCP 服务实现：工具方法 + 协议接线。
//!
//! 每个工具是 `DevToolsServer` 上的一个方法，由 `#[tool_router]` 生成路由片段；
//! [`crate::tools::tool_router`] 汇总全部片段，这里只做转发。

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::ServerConfig;
use rmcp::{ServerHandler, tool, tool_handler, tool_router};

use crate::tools;

#[derive(Debug, Clone, Default)]
pub(crate) struct DevToolsServer {
    tool_router: ToolRouter<Self>,
}

impl DevToolsServer {
    pub(crate) fn new() -> Self {
        Self {
            tool_router: tools::tool_router(),
        }
    }
}

#[tool_router(router = tool_router)]
impl DevToolsServer {
    /// 发送一条系统通知，用于任务完成、需要用户介入等场景。
    #[tool(
        name = "send_notification",
        description = "发送系统通知给本机用户。任务完成、构建结束、需要用户确认或长时间等待有结果时调用。title 写功能名，body 写为什么通知。"
    )]
    pub(crate) async fn send_notification(
        &self,
        Parameters(request): Parameters<tools::notify::SendNotificationRequest>,
    ) -> Result<String, String> {
        tools::notify::send_notification(Parameters(request)).await
    }

    /// 运行质量门禁。
    #[tool(
        name = "run_check",
        description = "运行 scripts/check.sh 质量门禁并返回结构化结果（是否通过、失败步骤、首条错误）。用 steps 只跑指定步骤，例如 [\"fmt\",\"clippy\"]，此时 full_gate 为 false，不能当作提交依据。改完代码后用它，不要自己拼 cargo 命令逐条跑；提交前必须跑一次全量。"
    )]
    pub(crate) async fn run_check(
        &self,
        Parameters(request): Parameters<tools::build::RunCheckRequest>,
    ) -> Result<String, String> {
        tools::build::run_check(Parameters(request)).await
    }

    /// 读取当前工作树改动。
    #[tool(
        name = "get_diff",
        description = "返回当前 worktree 的改动摘要：分支名、文件列表、增删行数，可选完整 patch。提交前用它自查改动。"
    )]
    pub(crate) async fn get_diff(
        &self,
        Parameters(request): Parameters<tools::diff::GetDiffRequest>,
    ) -> Result<String, String> {
        tools::diff::get_diff(Parameters(request)).await
    }

    /// 获取结构化编译诊断。
    #[tool(
        name = "get_build_diagnostics",
        description = "运行 cargo check 并把编译错误解析成结构化列表（错误码、文件、行、列、源码片段）。比读原始输出准确，编译失败时优先用它。"
    )]
    pub(crate) async fn get_build_diagnostics(
        &self,
        Parameters(request): Parameters<tools::diagnostics::GetBuildDiagnosticsRequest>,
    ) -> Result<String, String> {
        tools::diagnostics::get_build_diagnostics(Parameters(request)).await
    }

    /// 阻塞等待用户确认。
    #[tool(
        name = "wait_for_user",
        description = "需要用户确认时调用：写入待确认问题并阻塞等待，用户在 replies 目录写入回复后返回答案；超时返回 timed_out。会顺带发系统通知。"
    )]
    pub(crate) async fn wait_for_user(
        &self,
        Parameters(request): Parameters<tools::wait::WaitForUserRequest>,
    ) -> Result<String, String> {
        tools::wait::wait_for_user(Parameters(request)).await
    }

    /// 读取系统事件日志。
    #[tool(
        name = "read_system_log",
        description = "读取 Windows 事件日志条目，可按 event_ids 与时间范围过滤。省略 log_name 时列出可用日志名。排查平台问题（通知没弹、进程被拦截）时使用。"
    )]
    pub(crate) async fn read_system_log(
        &self,
        Parameters(request): Parameters<tools::logs::ReadSystemLogRequest>,
    ) -> Result<String, String> {
        tools::logs::read_system_log(Parameters(request)).await
    }

    /// 在文件管理器中定位路径。
    #[tool(
        name = "open_path",
        description = "在系统文件管理器中定位工程内的路径（文件会被选中，目录会打开）。路径相对工程根，超出工程根会被拒绝。"
    )]
    pub(crate) async fn open_path(
        &self,
        Parameters(request): Parameters<tools::paths::OpenPathRequest>,
    ) -> Result<String, String> {
        tools::paths::open_path(Parameters(request)).await
    }

    /// 列出本服务提供的工具。
    #[tool(
        name = "list_tools",
        description = "列出本服务可用工具的名称、用途，以及当前平台的通知、定位通道。"
    )]
    pub(crate) fn list_tools() -> Result<String, String> {
        let payload = serde_json::json!({
            "platform": tools::platform_report(),
            "tools": tools::catalogue(),
        });
        serde_json::to_string_pretty(&payload).map_err(|error| error.to_string())
    }
}

#[tool_handler(router = self.tool_router)]
#[allow(
    clippy::unused_async_trait_impl,
    reason = "tool_handler 宏生成的协议处理函数统一为 async"
)]
impl ServerHandler for DevToolsServer {
    fn get_info(&self) -> ServerConfig {
        tools::server_info()
    }
}
