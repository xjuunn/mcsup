//! 开发用 MCP 工具服务。
//!
//! 面向 AI agent 在本仓库开发时调用的本机工具（系统通知等），经 MCP stdio
//! 接入 Claude Code / Codex / `OpenCode` 等客户端。不属于 mcsup 产品运行时，
//! 因此放在 `apps/dev`。

mod platform;
mod server;
mod tools;

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // stdout 是 MCP 协议通道，日志只能写 stderr。
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("MCSUP_DEV_TOOLS_LOG")
                .unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let server = server::DevToolsServer::new();
    let service = rmcp::ServiceExt::serve(server, rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
