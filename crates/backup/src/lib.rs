//! 备份实现层：内容寻址存储（CAS）、快照索引与目录扫描
//!
//! 布局（`{root}` = `{workspace}/backups`，见 `docs/workspace.md`）：
//!
//! ```text
//! {root}/cas/ab/cd/<sha256>        内容寻址对象，按哈希前四位分片
//! {root}/tmp/                      写入暂存区（跨设备时才用，见 `CasStore::put`）
//! {root}/snapshots/<id>/snapshot.json  一份快照的元数据
//! {root}/snapshots/<id>/snapshot.partial  写入中标记，完成后删除
//! ```
//!
//! 设计要点：
//!
//! - **CAS 是唯一真相**：快照只记录「路径 → 哈希」，不存增量包；某份快照
//!   被删不会影响其他快照，GC 才是回收对象的唯一手段。
//! - **写入原子**：对象先写同目录临时文件、刷盘、再 rename。中断只会留下
//!   「没有该对象」，不会留下半个对象。
//! - **失败可见**：快照先落 `in_progress`，写完改 `complete`；中断后索引里
//!   留下一条 `failed` 记录，而不是一个看不见的幽灵目录。
//! - **不跟随符号链接**：扫描时遇到链接一律跳过并记入 `skipped`，
//!   否则备份会把服务器目录之外的内容读进来，恢复时又写到目录之外。

mod cas;
mod error;
mod scan;
mod store;

pub use cas::CasStore;
pub use error::BackupIoError;
pub use port::workspace_backups_root;
pub use port::{RUNTIME_ENTRIES, ScanError, ScanOutcome, ScanRules, SkipReason, Skipped};
pub use scan::{DirectoryScanner, scan_tree};
pub use store::{SnapshotIndex, snapshot_dir};

/// 快照元数据文件名
pub const SNAPSHOT_FILE: &str = "snapshot.json";
/// 写入中标记文件名：存在即表示这份快照尚未完成
pub const PARTIAL_FILE: &str = "snapshot.partial";
/// 快照索引目录名
pub const SNAPSHOTS_DIR: &str = "snapshots";
/// CAS 目录名
pub const CAS_DIR: &str = "cas";
