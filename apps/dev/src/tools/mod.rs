//! 工具注册表。
//!
//! 新增工具只需在 [`tool_router`] 里加一行 `with_route`，工具名与描述直接取自
//! `*_tool_attr()`，不在两个地方重复声明。

pub(crate) mod notify;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{ServerCapabilities, ServerConfig};

use crate::server::DevToolsServer;

/// 列出全部工具。
pub(crate) fn tool_router() -> ToolRouter<DevToolsServer> {
    ToolRouter::<DevToolsServer>::new()
        .with_route((
            DevToolsServer::send_notification_tool_attr(),
            DevToolsServer::send_notification,
        ))
        .with_route((
            DevToolsServer::list_tools_tool_attr(),
            DevToolsServer::list_tools,
        ))
}

/// 工具名与一句话描述，供 `list_tools` 工具返回。
pub(crate) fn catalogue() -> Vec<serde_json::Value> {
    let describe = |attr: rmcp::model::Tool| {
        let description = attr.description.unwrap_or_default().to_string();
        let name = attr.name.to_string();
        serde_json::json!({ "name": name, "description": description })
    };
    vec![
        describe(DevToolsServer::send_notification_tool_attr()),
        describe(DevToolsServer::list_tools_tool_attr()),
    ]
}

/// 当前平台名称与通知通道说明。
pub(crate) fn platform_report() -> serde_json::Value {
    serde_json::json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "notification": notify::channel(),
    })
}

/// MCP 服务能力与给 agent 的使用提示。
pub(crate) fn server_info() -> ServerConfig {
    let implementation =
        rmcp::model::Implementation::new("mcsup-dev-tools", env!("CARGO_PKG_VERSION"))
            .with_title("mcsup 开发工具");
    ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
        .with_server_info(implementation)
        .with_instructions(
            "本服务为 AI agent 提供本机开发工具。任务完成后请调用 send_notification 提醒用户，\
             不要把等待用户注意的内容只留在对话里。",
        )
}
