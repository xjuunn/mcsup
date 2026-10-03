# MCSUP 可靠性与安全规范

来自 `AGENTS.md`。

处理用户输入、密钥、配置或协议兼容时查阅。

---

# 安全

默认 Secure by Default。必须考虑 Authentication / Authorization / TLS /
Token / Secret Storage / Path Traversal / Command Injection / File Permission /
SSRF / 恶意文件 / 不可信 Provider / Remote Agent 安全。Minecraft Server、Shell
命令、文件系统、Remote Agent 都是高风险边界，禁止直接拼接用户输入执行 Shell。

# 路径安全

任何用户提供的路径都必须做安全检查，防止 `../../`、绝对路径逃逸、symlink
逃逸、目录遍历。Backup Restore、File API、Remote File Transfer 必须重点测试。

# 数据库设计

考虑 Index、Transaction、Consistency、Migration、Concurrency、Connection
Pool。不能因为「现在只有几个 Server」就完全忽略，也不要过度设计。

# 配置设计

配置必须明确、类型安全、可验证、可迁移、可默认，配置错误要在启动或使用前尽早
发现。

# API Compatibility

REST API 与 RPC Protocol 都要考虑版本兼容（如 `/api/v1`），Protocol 必须考虑
version / capability / compatibility。不能随意修改已使用的公共协议。

# 向后兼容

修改 Database / Config / REST API / RPC Protocol / Backup Format / Snapshot
Format 时必须考虑旧版本数据，不能默认直接删除旧数据。

# 可观测性

生产代码要便于定位问题，合理提供 Logs / Metrics / Tracing / Audit / Error
Context，重点关注 Server、Backup、RPC、Agent、File Transfer、HTTP。日志应带
必要上下文（server_id、request_id、task_id、backup_id、agent_id），但禁止记录
password / token / secret / private key。
