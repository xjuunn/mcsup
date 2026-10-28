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

# 元数据存储设计

元数据不是数据库：一条记录一个 JSON 文件，没有 Index / Transaction /
Connection Pool。约束由应用层保证，因此必须显式设计而不是依赖 DBMS：

- **唯一性**：写入前扫目录核对（名称、路径、`(workspace_id, rel_dir)`），
  校验时不得跳过损坏文件。
- **引用完整性**：没有外键，写入前确认被引用记录存在。
- **原子性**：单文件 `tmp → fsync → rename`；跨文件的一致性只能靠
  「只有一个真相源」（如 `state.json` 里的当前指针）。
- **并发**：进程内 `Mutex` + 跨进程操作系统文件锁；跨机器共享目录不在契约内。
- **格式版本**：每条记录自带 `schema`，读到未知版本拒绝而不是覆盖。

不能因为「现在只有几个 Server」就完全忽略这些，也不要过度设计。

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
