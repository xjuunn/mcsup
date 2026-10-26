use domain::backup::{BackupScope, ContentHash, RelPath, Snapshot, SnapshotId};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// CAS 读写失败
///
/// 与仓储的 `RepositoryError` 分开：CAS 的失败是「文件系统层面的大块数据」，
/// 触发它的原因（磁盘满、目标被占用、对象损坏）与元数据记录的失败不同，
/// 入口的判级与提示也要分开。
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CasError {
    /// 对象不存在
    #[error("cas object missing: {0}")]
    Missing(ContentHash),
    /// 对象内容与哈希不符（篡改或位翻转）
    #[error("cas object {hash} is corrupt: {reason}")]
    Corrupt {
        /// 对象哈希
        hash: ContentHash,
        /// 具体原因（大小不符 / 重算哈希不符）
        reason: String,
    },
    /// 本地 IO 失败
    #[error("io error at {path}: {source}")]
    Io {
        /// 出错路径
        path: PathBuf,
        /// 底层错误
        #[source]
        source: std::io::Error,
    },
}

impl CasError {
    /// 构造 IO 失败
    #[must_use]
    pub fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// 内容寻址存储：按内容哈希存取文件字节
///
/// 语义契约（实现必须满足，否则去重与 GC 会失效）：
/// - `put` 幂等：同哈希同内容重复写入不改变结果，也不报错
/// - `put` 是原子的：进程中断只可能留下「没有该对象」，不可能留下半个对象
/// - `get` 在目标处重建字节，调用方负责目标目录与路径安全
pub trait ContentStore: Send + Sync {
    /// 写入一段字节流并返回其哈希
    ///
    /// `hint` 只用于日志与临时文件命名，不参与寻址。
    ///
    /// # Errors
    ///
    /// 读取 `reader` 或写入 CAS 失败时返回 [`CasError`]
    fn put(
        &self,
        reader: &mut dyn std::io::Read,
        hint: &RelPath,
    ) -> Result<(ContentHash, u64), CasError>;

    /// 把对象内容写到 `dest`（覆盖写）
    ///
    /// # Errors
    ///
    /// 对象不存在、损坏或写入失败时返回 [`CasError`]
    fn get(&self, hash: &ContentHash, dest: &Path) -> Result<u64, CasError>;

    /// 对象是否存在且大小已知
    ///
    /// # Errors
    ///
    /// 元信息读取失败（非「不存在」）时返回 [`CasError`]
    fn stat(&self, hash: &ContentHash) -> Result<Option<u64>, CasError>;

    /// 逐个校验对象内容是否与哈希一致，返回损坏的哈希
    ///
    /// `on_progress` 报告「已校验 / 总数」，`total` 未知时为 `None`。
    ///
    /// # Errors
    ///
    /// 目录遍历失败时返回 [`CasError`]；单个对象损坏不算错误（记入返回值）
    fn verify(
        &self,
        hashes: &[ContentHash],
        on_progress: &mut dyn FnMut(usize, usize),
    ) -> Result<Vec<ContentHash>, CasError>;

    /// 列出 CAS 中全部对象
    ///
    /// # Errors
    ///
    /// 目录遍历失败时返回 [`CasError`]
    fn list(&self) -> Result<Vec<ContentHash>, CasError>;

    /// 删除对象；已不存在视为成功（幂等）
    ///
    /// # Errors
    ///
    /// 删除失败时返回 [`CasError`]
    fn remove(&self, hash: &ContentHash) -> Result<(), CasError>;
}

/// 快照元数据存储失败
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SnapshotStoreError {
    /// 快照不存在
    #[error("snapshot not found: {0}")]
    NotFound(String),
    /// 快照记录无法解析（被手改或跨版本写入）
    #[error("corrupt snapshot record: {0}")]
    Corrupt(String),
    /// 底层存储失败
    #[error("snapshot storage error: {0}")]
    Storage(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// 快照索引：一份快照一个记录，按服务器查询
///
/// 「写入中的快照」先以「写入中」状态落盘，写完再改状态，
/// 这样进程中断后索引里留下的是一条可识别的失败记录，而不是消失的幽灵。
pub trait SnapshotRepository: Send + Sync {
    /// 标记一次快照写入开始（建占位目录 / 「写入中」标记）
    ///
    /// 与 [`Self::complete`] 成对使用：`begin` 之后到 `complete` 之前崩溃，
    /// 索引里留下的是可识别的失败记录。实现保证两标记幂等。
    ///
    /// # Errors
    ///
    /// 底层失败返回 [`SnapshotStoreError::Storage`]
    fn begin(&self, id: &SnapshotId) -> Result<(), SnapshotStoreError>;

    /// 标记一次快照写入完成（写记录并清除「写入中」标记）
    ///
    /// # Errors
    ///
    /// 底层失败返回 [`SnapshotStoreError::Storage`]
    fn complete(&self, snapshot: &Snapshot) -> Result<(), SnapshotStoreError>;

    /// 写入（覆盖）一条快照记录
    ///
    /// # Errors
    ///
    /// 底层失败返回 [`SnapshotStoreError::Storage`]
    fn save(&self, snapshot: &Snapshot) -> Result<(), SnapshotStoreError>;

    /// 按 id 读取
    ///
    /// # Errors
    ///
    /// 不存在返回 [`SnapshotStoreError::NotFound`]；损坏返回
    /// [`SnapshotStoreError::Corrupt`]
    fn get(&self, id: &SnapshotId) -> Result<Snapshot, SnapshotStoreError>;

    /// 某台服务器的全部快照，按创建时间升序
    ///
    /// # Errors
    ///
    /// 底层失败返回 [`SnapshotStoreError::Storage`]
    fn list_for_server(&self, server: &str) -> Result<Vec<Snapshot>, SnapshotStoreError>;

    /// 全部快照，按创建时间升序（GC 与跨服查询用）
    ///
    /// # Errors
    ///
    /// 底层失败返回 [`SnapshotStoreError::Storage`]
    fn list_all(&self) -> Result<Vec<Snapshot>, SnapshotStoreError>;

    /// 删除一条记录；已不存在视为成功（幂等）
    ///
    /// # Errors
    ///
    /// 底层失败返回 [`SnapshotStoreError::Storage`]
    fn remove(&self, id: &SnapshotId) -> Result<(), SnapshotStoreError>;
}

/// `{workspace}/backups`：工作区共享的备份根目录
///
/// 布局是产品口径（CAS 与快照按工作区共享），与实现无关，因此由契约层
/// 给出；实现层只按此根装配 `cas/` 与 `snapshots/`。
#[must_use]
pub fn workspace_backups_root(workspace_path: &Path) -> PathBuf {
    workspace_path.join("backups")
}

/// 运行期文件 / 目录：不进备份
///
/// `console.log` 与 `run/` 由 supervisor 在服务器运行期间持续写入，复制它们
/// 既不稳定也没有恢复价值。
pub const RUNTIME_ENTRIES: &[&str] = &["run", "console.log"];

/// 扫描规则
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanRules {
    /// 备份范围；缺省 = 整个目录
    pub scope: BackupScope,
    /// 扫描时优先信任 `(size, mtime)` 而跳过重算的条目
    ///
    /// 增量备份的优化：父快照里同路径、同大小、同 mtime 的文件直接复用
    /// 哈希，省掉一次完整读取。内容被原地改写但大小与 mtime 都不变的情况
    /// 在 MC 世界里不会发生（Region 文件追加写会改 mtime）。
    pub reuse: Vec<(RelPath, ContentHash, u64, i64)>,
}

/// 跳过原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SkipReason {
    /// 符号链接（不跟随）
    Symlink,
    /// 非常规文件（设备 / 管道 / 套接字）
    NotRegularFile,
    /// 运行期条目（`run/` / `console.log`）
    Runtime,
    /// 不在备份范围内
    OutOfScope,
}

impl SkipReason {
    /// 稳定文本口径（日志 / 统计用）
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Symlink => "symlink",
            Self::NotRegularFile => "not_regular_file",
            Self::Runtime => "runtime",
            Self::OutOfScope => "out_of_scope",
        }
    }
}

/// 被跳过的一项
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// 相对路径
    pub path: RelPath,
    /// 跳过原因
    pub reason: SkipReason,
}

/// 扫描结果
#[derive(Debug, Clone, Default)]
pub struct ScanOutcome {
    /// 条目（按路径升序）
    pub entries: Vec<(RelPath, ContentHash)>,
    /// 每个文件的 `(路径, 大小, mtime 秒)`：下次备份的复用提示来源
    pub stamps: Vec<(RelPath, u64, i64)>,
    /// 被跳过的条目
    pub skipped: Vec<Skipped>,
    /// 内容总字节数
    pub total_bytes: u64,
    /// 本次真正读盘并写入 CAS 的字节数
    pub written_bytes: u64,
    /// 新写入的 CAS 对象数
    pub added: usize,
}

/// 目录扫描失败
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ScanError {
    /// 目录不可读
    #[error("cannot read directory {path}: {source}")]
    Unreadable {
        /// 目录
        path: PathBuf,
        /// 底层错误
        #[source]
        source: std::io::Error,
    },
    /// 路径无法转为相对路径（非法文件名 / 越界）
    #[error(transparent)]
    InvalidPath(#[from] domain::backup::InvalidRelPath),
    /// 文件不可读
    #[error("cannot read file {path}: {source}")]
    UnreadableFile {
        /// 文件
        path: PathBuf,
        /// 底层错误
        #[source]
        source: std::io::Error,
    },
    /// CAS 写入失败
    #[error(transparent)]
    Cas(#[from] CasError),
}

/// 目录扫描器：把服务器目录转成「相对路径 → 内容哈希」并写入 CAS
///
/// 扫描是备份的热路径，实现必须满足的口径（不止一处实现要一致）：
/// - 不跟随符号链接
/// - 跳过运行期条目（[`RUNTIME_ENTRIES`]）
/// - 按 [`ScanRules::scope`] 过滤，且过滤发生在读取文件之前
/// - [`ScanRules::reuse`] 命中时跳过读盘，仅当 `(路径, 大小, mtime)` 全等
pub trait Scanner: Send + Sync {
    /// 扫描 `root` 并把每个文件的字节写进 `store`
    ///
    /// `on_file` 在每个文件处理完时回调（已处理文件数），供入口画进度。
    ///
    /// # Errors
    ///
    /// 目录不可读、路径非法或 CAS 写入失败时返回 [`ScanError`]
    fn scan(
        &self,
        root: &Path,
        store: &dyn ContentStore,
        rules: &ScanRules,
        on_file: &mut dyn FnMut(usize),
    ) -> Result<ScanOutcome, ScanError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cas_error_carries_path_and_hash() {
        let err = CasError::io(Path::new("/cas/ab/cd/x"), std::io::Error::other("boom"));
        assert!(err.to_string().contains("/cas/ab/cd/x"), "{err}");

        let hash = ContentHash::parse(&"a".repeat(64)).expect("hash");
        let err = CasError::Corrupt {
            hash: hash.clone(),
            reason: "size mismatch".to_owned(),
        };
        assert!(err.to_string().contains(hash.as_str()), "{err}");
    }
}
