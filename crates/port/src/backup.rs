use domain::backup::{ContentHash, RelPath, Snapshot, SnapshotId};
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
