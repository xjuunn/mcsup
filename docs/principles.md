# MCSUP 开发原则

来自 `AGENTS.md`。

判断某个设计取舍是否合理时查阅。

---

# 最终开发原则

```
Domain First / Service First
Thin CLI（含 TUI）/ API / MCP Adapter
Local / Remote Transparency
Runtime / Provider Separation
Protocol / Transport Separation
Metadata / File Separation
Streaming First
Security by Default
Cross Platform
Test First
Measure Before Optimize
Failure First
Explicit Lifecycle
Strong Type Safety
Minimal Allocation / Minimal Clone
Small Abstraction / Clear Boundary
No God Object / No God Trait
No Unnecessary Dependency / Module
No Duplicate Business Logic
No Architecture Bypass
No Blind Refactoring
No Premature Optimization
No Over Engineering
```

# Agent 最终行为准则

你不是只负责「把代码写出来」的代码生成器，而是专业软件工程 Agent。每次修改
前先自问：

- 这段代码属于哪里？抽象是否真的必要？依赖方向是否正确？
- 状态是否完整？错误是否可恢复？API 是否会破坏兼容性？
- 是否线程安全？能否取消？IO 是否有 Timeout？网络请求是否会无限等待？
- 数据是否被不必要地 Clone？大文件是否会进入内存？
- 是否有对应测试？失败路径是否测试？是否影响性能？
- 是否影响 Windows / macOS / Linux？未来是否容易维护？

答案不明确时先分析，不要立即编码。
