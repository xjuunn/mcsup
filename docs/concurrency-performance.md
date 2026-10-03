# MCSUP 并发与性能规范

来自 `AGENTS.md`。

涉及并发、性能、IO 或任务取消时查阅。

---

# 并发设计

区分 Concurrency（并发）与 Parallelism（并行）。设计时关注 Tokio、async/await、
Send / Sync、Mutex / RwLock、channels、backpressure、cancellation、timeout、
task lifecycle。

禁止 `Arc<Mutex<Everything>>`，禁止用全局锁解决一切，锁粒度尽量小。禁止持锁做
网络请求或长等待：正确顺序是读取状态 → 释放锁 → 执行 IO → 重新更新状态。

# 性能原则

以生产级性能为目标，优化必须基于实际测量而非感觉。关注 CPU、内存、IO、磁盘、
网络、锁竞争、分配、拷贝、序列化、哈希、延迟、吞吐。高风险路径：

- Backup：大文件、Hash、去重、磁盘 IO、并行哈希、Snapshot、GC
- File Transfer：Streaming、Chunk、Checksum、压缩、带宽、内存、Backpressure
- RPC：延迟、序列化、Streaming、连接复用、压缩、并发
- Server Management：进程 IO、日志流、事件分发、状态更新

# 资源生命周期

Process / File / Socket / Connection / Task / Channel / Lock / 临时文件 /
Backup 都必须明确 Create → Use → Cancel → Timeout → Failure → Cleanup，
尤其是 spawn、文件、网络、备份、临时目录，不能泄漏。

# Cancellation

Backup / Download / Upload / File Copy / Restore / RPC Streaming / Log
Streaming 等长任务必须支持 CancellationToken，取消后正确清理资源。

# Timeout

HTTP / RPC / Provider / File Transfer / Agent / Database 等外部 IO 必须有
Timeout，不能无限等待。

# 网络设计

网络操作必须考虑 Timeout、Retry、Backoff、Cancellation、连接复用、错误分类、
Rate Limit。Retry 必须区分可重试与不可重试错误，禁止对所有错误无限 Retry。

# 性能与可观测性

高频路径上，日志 / Trace 不能制造大量 String 或高成本 allocation；注意
Allocation、Logging、Lock、Clone、Serialization。
