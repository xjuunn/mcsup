use crate::server::version::VersionId;

/// 领域事件：跨模块通信的公开事实
///
/// 消费者包括 CLI、API、MCP、Scheduler、Monitoring、Audit、Notification；
/// `domain` / `service` 只产生事件，不依赖这些 UI。
///
/// 事件是「已经发生的事实」，命名用过去时；破坏性变更只加变体 / 加字段，
/// 不改已有语义。
///
/// **当前由 `service` 发布的变体**：`ServerStarting`（start 返回）、
/// `ServerStopped`（stop 成功）、`BackupCompleted`（备份完成）、
/// `BackupFailed`（备份失败）。
///
/// 其余变体（`ServerStarted` / `ServerCrashed` / `JarDownload*`）是既定
/// 词汇表，由后续的 supervision / 下载编排闭环节点发布——它们是**边沿**
/// 事件，不能从请求路径上的无状态 `status()` 轮询里产生（否则每次轮询都重发）。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomainEvent {
    /// 下载完成并校验通过
    JarDownloadCompleted {
        /// 服务端版本
        version: VersionId,
    },
    /// 下载或校验失败
    JarDownloadFailed {
        /// 服务端版本
        version: VersionId,
        /// 失败原因
        reason: String,
    },
    /// 服务器已拉起（进入 `Starting`）
    ServerStarting {
        /// 服务器名
        name: String,
    },
    /// 服务器已就绪（进入 `Running`）
    ServerStarted {
        /// 服务器名
        name: String,
    },
    /// 服务器已停机（进入 `Stopped`）
    ServerStopped {
        /// 服务器名
        name: String,
    },
    /// 服务器意外崩溃（进入 `Crashed`）
    ServerCrashed {
        /// 服务器名
        name: String,
        /// 退出码（未知时为 `None`）
        exit_code: Option<i32>,
    },
    /// 备份已完成
    BackupCompleted {
        /// 服务器名
        name: String,
        /// 快照 id
        snapshot: String,
        /// 新增对象字节数
        added_bytes: u64,
    },
    /// 备份失败
    BackupFailed {
        /// 服务器名
        name: String,
        /// 失败原因
        reason: String,
    },
}
