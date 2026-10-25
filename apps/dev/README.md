# 开发工具（MCP）

`apps/dev` 是面向 AI agent 的开发期 MCP 工具服务，经 MCP stdio 接入客户端。
它不属于 mcsup 产品运行时，也不链接 `mcsup` 门面：工具只服务于本仓库开发流程。

可执行文件：`mcsup-dev-tools`。

## 工具

| 工具 | 用途 |
| --- | --- |
| `send_notification` | 发送系统通知：任务完成、构建结束、等待确认时提醒用户 |
| `run_check` | 运行 `scripts/check.sh`，返回结果状态、失败步骤、首条错误 |
| `get_diff` | 返回当前 worktree 改动摘要，可选完整 patch |
| `get_build_diagnostics` | 跑 `cargo check`，把编译错误解析成结构化列表 |
| `wait_for_user` | 写入待确认问题并阻塞等待用户回复 |
| `read_system_log` | 读取 Windows 事件日志，可按事件 id 与时间过滤 |
| `open_path` | 在系统文件管理器中定位工程内路径 |
| `list_tools` | 列出可用工具、当前平台与各通道说明 |

## `run_check` 的结果

`outcome` 是一个枚举，同时表达「跑没跑全量」和「过没过」：

| `kind` | 含义 |
| --- | --- |
| `full_gate_passed` | 全量门禁通过，可作提交依据 |
| `full_gate_failed` / `full_gate_timed_out` | 全量门禁失败 / 超时 |
| `subset_passed` | 只跑了指定步骤且通过，**不能**作提交依据 |
| `subset_failed` / `subset_timed_out` | 子集失败 / 超时 |

`steps` 只在开发期做局部验证用；提交前必须跑一次不带 `steps` 的全量。

## 工程根定位

MCP 服务由客户端启动，工作目录不一定是仓库根。所有涉及仓库路径的工具经
`tools/workspace.rs` 的 `workspace_root()` 解析，依次尝试：

1. `MCSUP_DEV_TOOLS_ROOT` 环境变量
2. 当前工作目录及其祖先中含 `Cargo.toml` + `scripts/check.sh` 的目录
3. 可执行文件所在目录的祖先

都找不到时报错，不猜测。

## `wait_for_user` 的回复方式

MCP 是请求-响应模型，工具执行期间 agent 无法继续对话，因此需要独立通道：

1. 调用后生成 `.mcsup-dev/requests/<id>.json`，其中含 `question` 与 `reply_path`
2. 你在 `reply_path` 写入 `{"answer": "你的回答"}`
3. 工具轮询到该文件即返回答案；超时返回 `timed_out: true`

`.mcsup-dev/` 在 `.gitignore` 中忽略。`wait_for_user` 默认顺带发系统通知。

## 平台实现

工具只调用 `src/platform/` 的函数，平台差异在该目录按 `cfg` 分文件：

| 平台 | 通知 | 文件管理器 |
| --- | --- | --- |
| Windows | `Windows.UI.Notifications` Toast（WinRT） | `explorer.exe` |
| macOS | 通知中心（`terminal-notifier` 可选，回退 `osascript`） | `open -R` |
| Linux | `notify-send` | `xdg-open` |

Windows 通知不启动任何外部进程：文案写进系统 Toast 模板的文本节点（由 MSXML
转义），发送后读一次通知中心历史确认系统已收录；被拦截时返回
「通知已提交但未进入通知中心」，不谎报成功。

Windows 事件日志经 `wevtutil`（系统自带，参数为固定字面量）读取，在 Rust 侧
解析 XML——不走 `/f:text`，因为后者的消息文本依赖消息 DLL，缺 DLL 时只剩占位符。

## 构建与接入

```
cargo build -p dev
```

仓库根目录的 `opencode.jsonc` 已指向 `./target/debug/mcsup-dev-tools.exe`。
其他客户端把同样的命令配成 `stdio` 服务器即可。

## 新增工具

1. 在 `src/tools/` 新建模块，写参数结构体（`serde::Deserialize` + `schemars::JsonSchema`）
   和响应结构体（`serde::Serialize`）。
2. 在 `src/tools/mod.rs` 的 `tool_router()` 加一行 `with_route`，并在 `catalogue()` 补一项。
3. 在 `src/server.rs` 的 `#[tool_router]` impl 里加方法，方法体转发到工具模块。
4. 补失败路径测试，执行 `scripts/check.sh`。

失败统一用 `tools::ToolError`：`InvalidInput` 表示改参数重试，`NotFound` 表示
检查环境，`Unsupported` 表示当前平台不具备该能力，`Failed` 表示内部错误。
工具一律返回 `Result<String, String>`，成功时是 JSON 字符串。

`apps/dev` 与其他 apps 一样不依赖 `apps/*` 之外的同级入口；需要 mcsup 能力时
只经 `mcsup` 门面。
