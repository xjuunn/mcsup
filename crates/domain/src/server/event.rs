use crate::server::version::VersionId;

/// 领域事件：跨模块通信的公开事实
///
/// 消费者包括 CLI、API、MCP、Scheduler、Monitoring、Audit、Notification；
/// `domain` / `service` 只产生事件，不依赖这些 UI。
///
/// 事件是「已经发生的事实」，命名用过去时；破坏性变更只加变体 / 加字段，
/// 不改已有语义。
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
