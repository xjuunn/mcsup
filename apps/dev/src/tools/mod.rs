//! 工具注册表。
//!
//! 新增工具只需在 [`tool_router`] 里加一行 `with_route`，工具名与描述直接取自
//! `*_tool_attr()`，不在两个地方重复声明。

pub(crate) mod build;
pub(crate) mod diagnostics;
pub(crate) mod diff;
pub(crate) mod logs;
pub(crate) mod notify;
pub(crate) mod paths;
pub(crate) mod wait;
pub(crate) mod workspace;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{ServerCapabilities, ServerConfig};

use crate::server::DevToolsServer;

/// 工具执行失败的原因。
///
/// 每个变体对应调用方应采取的不同动作：`InvalidInput` 改参数重试，
/// `NotFound` 检查环境，`Unsupported` 换平台或换工具。工具统一把它渲染成
/// 一行文本，不把内部错误类型直接暴露给调用方。
#[derive(Debug, thiserror::Error)]
pub(crate) enum ToolError {
    #[error("{0}")]
    InvalidInput(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Io(String),
    #[error("{0}")]
    Failed(String),
    /// 当前平台不具备该能力。与 `NotFound` 区分：不是「找不到」，
    /// 而是这个平台根本不支持，换参数也无济于事。
    #[error("{0}")]
    Unsupported(String),
}

impl ToolError {
    /// 渲染成给调用方看的一行说明。
    pub(crate) fn render(&self) -> String {
        self.to_string()
    }
}

/// 列出全部工具。
pub(crate) fn tool_router() -> ToolRouter<DevToolsServer> {
    ToolRouter::<DevToolsServer>::new()
        .with_route((
            DevToolsServer::send_notification_tool_attr(),
            DevToolsServer::send_notification,
        ))
        .with_route((
            DevToolsServer::run_check_tool_attr(),
            DevToolsServer::run_check,
        ))
        .with_route((
            DevToolsServer::get_diff_tool_attr(),
            DevToolsServer::get_diff,
        ))
        .with_route((
            DevToolsServer::get_build_diagnostics_tool_attr(),
            DevToolsServer::get_build_diagnostics,
        ))
        .with_route((
            DevToolsServer::wait_for_user_tool_attr(),
            DevToolsServer::wait_for_user,
        ))
        .with_route((
            DevToolsServer::read_system_log_tool_attr(),
            DevToolsServer::read_system_log,
        ))
        .with_route((
            DevToolsServer::open_path_tool_attr(),
            DevToolsServer::open_path,
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
        describe(DevToolsServer::run_check_tool_attr()),
        describe(DevToolsServer::get_diff_tool_attr()),
        describe(DevToolsServer::get_build_diagnostics_tool_attr()),
        describe(DevToolsServer::wait_for_user_tool_attr()),
        describe(DevToolsServer::read_system_log_tool_attr()),
        describe(DevToolsServer::open_path_tool_attr()),
        describe(DevToolsServer::list_tools_tool_attr()),
    ]
}

/// 当前平台名称与各通道说明。
pub(crate) fn platform_report() -> serde_json::Value {
    serde_json::json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "notification": notify::channel(),
        "reveal": paths::channel(),
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
            "本服务为 AI agent 提供本机开发工具。\n\
             - 改完代码用 run_check 跑门禁，不要自己拼 cargo 命令逐条跑。\n\
             - 编译失败用 get_build_diagnostics 拿结构化错误，不要读原始输出。\n\
             - 提交前用 get_diff 自查改动。\n\
             - 需要用户确认时用 wait_for_user，它会阻塞到用户回复或超时。\n\
             - 任务结束用 send_notification 通知用户，title 写功能名，body 写为什么通知。\n\
             - 路径参数一律相对工程根，不要传客户端 cwd 下的相对路径。",
        )
}
