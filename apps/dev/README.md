# 开发工具（MCP）

`apps/dev` 是面向 AI agent 的开发期 MCP 工具服务，经 MCP stdio 接入客户端。
它不属于 mcsup 产品运行时，也不链接 `mcsup` 门面：工具只服务于本仓库开发流程。

可执行文件：`mcsup-dev-tools`。

| 工具 | 用途 |
| --- | --- |
| `send_notification` | 发送系统通知：任务完成、构建结束、等待用户确认时提醒用户 |
| `list_tools` | 列出可用工具、当前平台与通知通道 |

## 平台实现

工具只调用 `src/platform/` 的 `notify`，平台差异在该目录按 `cfg` 分文件：

| 平台 | 通道 | 依赖 |
| --- | --- | --- |
| Windows | 直接调用 `Windows.UI.Notifications` Toast | `windows` crate（WinRT） |
| macOS | 通知中心 | `terminal-notifier`（可选），回退系统 `osascript` |
| Linux | 通知服务 | `notify-send`（libnotify） |

Windows 不启动任何外部进程：文案写进系统 Toast 模板的文本节点（由 MSXML 转义），
发送后读一次通知中心历史确认系统已收录；被拦截时返回
「通知已提交但未进入通知中心」，不谎报成功。

macOS / Linux 仍走系统命令，子进程有 10 秒超时，失败不静默：
`send_notification` 返回 `isError` 与原因。

## 构建与接入

```
cargo build -p dev
```

仓库根目录的 `opencode.jsonc` 已指向 `./target/debug/mcsup-dev-tools.exe`。
其他客户端把同样的命令配成 `stdio` 服务器即可，例如：

```
mcsup-dev-tools
```

`send_notification` 的参数：

| 参数 | 必填 | 说明 |
| --- | --- | --- |
| `title` | 是 | 1-128 字符 |
| `body` | 否 | 0-1024 字符 |
| `urgency` | 否 | `low` / `normal` / `critical`，默认 `normal` |

## 新增工具

1. 在 `src/tools/` 新建模块，写参数结构体（`serde::Deserialize` + `schemars::JsonSchema`）。
2. 在 `src/tools/mod.rs` 的 `tool_router()` 加一行 `with_route`，并在 `catalogue()` 补一项。
3. 在 `src/server.rs` 的 `#[tool_router]` impl 里加方法，方法体转发到工具模块。
4. 补失败路径测试，执行 `scripts/check.sh`。

`apps/dev` 与其他 apps 一样不依赖 `apps/*` 之外的同级入口；需要 mcsup 能力时
只经 `mcsup` 门面。
