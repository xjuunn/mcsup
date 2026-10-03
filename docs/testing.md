# MCSUP 测试规范

来自 `AGENTS.md`。

设计测试、评估测试覆盖时查阅。

---

# 禁止过早优化

性能优先不等于盲目优化。顺序是 Correctness → Measure → Identify Bottleneck
→ Optimize → Benchmark → Verify。没有 Benchmark 或 Profiling 依据，不得因为
「感觉可能慢」引入复杂缓存、Lock-free 或复杂线程池。

# 测试是开发过程的一部分

任何新功能都必须同时编写测试，禁止「先写代码，以后再补」。顺序是设计 → 测试
设计 → 实现 → 运行测试 → 性能验证 → Review。

# 测试分层

- Unit：Domain、State、Parser、Validator、Backup、Hash、Runtime、Provider、
  Permission、Utility。放 `tests/` 或模块内 `#[cfg(test)] mod tests`。
- Integration：Service、Storage、Runtime、Backup、File、Provider，验证多个
  组件协作。
- API：Request / Response、Validation、AuthN / AuthZ、Error、Pagination、
  WebSocket。
- RPC：Connection、AuthN、Request / Response、Streaming、Timeout、
  Cancellation、版本不匹配、连接失败。
- CLI：command、argument、output、exit code、error、json。

# 测试矩阵

每个功能都要有完整矩阵。以 `backup create` 为例，至少覆盖：正常、空目录、大
文件、重复文件、修改文件、删除文件、权限错误、磁盘空间不足、Hash / IO 错误、
并发备份、中断、恢复、Snapshot 一致性、GC、Restore。禁止只写
`test_backup_success()`。

# 测试优先级

必须覆盖 Happy Path、Boundary、Invalid Input、Failure、Concurrency、
Cancellation、Timeout、Recovery、Persistence、Compatibility、Performance。
Backup、File Transfer、RPC、Runtime、Storage 等涉及数据和系统资源的模块必须
重点测试失败路径。

# 测试与实现同步

实现一个函数 / Service / API / RPC / Backup 功能，就立即测试对应单元，不要积累
大量未测试代码。

# 性能测试

Backup、CAS、Snapshot、Hashing、Restore、File Transfer、Serialization、RPC、
Provider Parsing 等高成本模块必须有 Benchmark，能回答 Throughput、Latency、
Memory、CPU、Allocation。不能只声称「性能很好」。
