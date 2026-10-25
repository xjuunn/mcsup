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
        description = "发送系统通知给本机用户。任务完成、构建结束、需要用户确认或长时间等待有结果时调用。"
    )]
    pub(crate) async fn send_notification(
        &self,
        Parameters(request): Parameters<tools::notify::SendNotificationRequest>,
    ) -> Result<String, String> {
        tools::notify::send_notification(Parameters(request)).await
    }

    /// 列出本服务提供的工具及当前平台的通知通道。
    #[tool(
        name = "list_tools",
        description = "列出本服务可用工具的名称、用途，以及当前平台使用的通知通道。"
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
