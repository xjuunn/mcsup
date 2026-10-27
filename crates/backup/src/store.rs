//! 快照索引：一份快照一个 JSON，带「写入中」标记
//!
//! 目录布局：`{root}/snapshots/<id>/snapshot.json` + `snapshot.partial`
//!
//! `snapshot.partial` 存在即表示这份快照没有写完。读取时据此把状态修正为
//! [`SnapshotStatus::Failed`]：进程在写入途中崩溃后，索引里留下的是可识别
//! 的失败记录，而不是一个看不见的幽灵快照。

use domain::backup::{BackupScope, ContentHash, RelPath, Snapshot, SnapshotId, SnapshotStatus};
use port::{SnapshotRepository, SnapshotStoreError};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::BackupIoError;
use crate::{PARTIAL_FILE, SNAPSHOT_FILE, SNAPSHOTS_DIR};

/// 快照记录格式版本
const SNAPSHOT_SCHEMA: u32 = 1;

/// `{workspace}/backups/snapshots/<id>`
#[must_use]
pub fn snapshot_dir(workspace_path: &Path, id: &SnapshotId) -> PathBuf {
    port::workspace_backups_root(workspace_path)
        .join(SNAPSHOTS_DIR)
        .join(id.as_str())
}

/// 记录文件的结构（与领域对象分开：磁盘格式是兼容契约，领域对象可以演进）
#[derive(Debug, Serialize, Deserialize)]
struct SnapshotRecord {
    schema: u32,
    id: String,
    workspace_id: String,
    server: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    scope: Vec<String>,
    status: String,
    #[serde(default)]
    deleted: Vec<String>,
    entries: Vec<EntryRecord>,
    added: usize,
    reused: usize,
    total_bytes: u64,
    added_bytes: u64,
    server_online: bool,
    created_at: i64,
}

#[derive(Debug, Serialize, Deserialize)]
struct EntryRecord {
    path: String,
    hash: String,
    /// 备份时的文件大小（复用提示用；旧记录缺失按「不可复用」处理）
    #[serde(default)]
    size: u64,
    /// 备份时的修改时间秒
    #[serde(default)]
    mtime: i64,
}

fn corrupt(id: &str, message: impl std::fmt::Display) -> SnapshotStoreError {
    SnapshotStoreError::Corrupt(format!("{id}: {message}"))
}

impl TryFrom<SnapshotRecord> for Snapshot {
    type Error = SnapshotStoreError;

    fn try_from(record: SnapshotRecord) -> Result<Self, Self::Error> {
        if record.schema != SNAPSHOT_SCHEMA {
            return Err(corrupt(
                &record.id,
                format!("schema {}, expected {SNAPSHOT_SCHEMA}", record.schema),
            ));
        }
        let id = SnapshotId::parse(&record.id).map_err(|err| corrupt(&record.id, err))?;
        let parent = match record.parent {
            Some(raw) => Some(SnapshotId::parse(&raw).map_err(|err| corrupt(&record.id, err))?),
            None => None,
        };
        let status =
            SnapshotStatus::parse(&record.status).map_err(|err| corrupt(&record.id, err))?;
        let scope = BackupScope::from_entries(record.scope.clone())
            .map_err(|err| corrupt(&record.id, err))?;

        let mut entries = Vec::with_capacity(record.entries.len());
        let mut stamps = Vec::with_capacity(record.entries.len());
        for entry in &record.entries {
            let path = RelPath::parse(&entry.path).map_err(|err| corrupt(&record.id, err))?;
            let hash = ContentHash::parse(&entry.hash).map_err(|err| corrupt(&record.id, err))?;
            // 大小或 mtime 缺失（旧记录）时不给复用提示：重算一次比误判便宜
            if entry.size > 0 || entry.mtime > 0 {
                stamps.push((path.clone(), entry.size, entry.mtime));
            }
            entries.push((path, hash));
        }
        let mut deleted = Vec::with_capacity(record.deleted.len());
        for raw in &record.deleted {
            deleted.push(RelPath::parse(raw).map_err(|err| corrupt(&record.id, err))?);
        }
        let content = domain::backup::SnapshotContent::new(entries, deleted)
            .map_err(|err| corrupt(&record.id, err))?
            .with_stamps(stamps);

        Ok(Self {
            id,
            workspace_id: record.workspace_id,
            server: record.server,
            label: record.label,
            parent,
            scope,
            status,
            content,
            added: record.added,
            reused: record.reused,
            total_bytes: record.total_bytes,
            added_bytes: record.added_bytes,
            server_online: record.server_online,
            created_at: record.created_at,
        })
    }
}

impl From<&Snapshot> for SnapshotRecord {
    fn from(snapshot: &Snapshot) -> Self {
        Self {
            schema: SNAPSHOT_SCHEMA,
            id: snapshot.id.as_str().to_owned(),
            workspace_id: snapshot.workspace_id.clone(),
            server: snapshot.server.clone(),
            label: snapshot.label.clone(),
            parent: snapshot.parent.as_ref().map(|id| id.as_str().to_owned()),
            scope: snapshot.scope.entries().to_vec(),
            status: snapshot.status.as_str().to_owned(),
            deleted: snapshot
                .content
                .deleted()
                .iter()
                .map(|path| path.as_str().to_owned())
                .collect(),
            entries: snapshot
                .content
                .entries()
                .iter()
                .map(|(path, hash)| {
                    let (size, mtime) = snapshot
                        .content
                        .stamps()
                        .iter()
                        .find(|(known, _, _)| known == path)
                        .map_or((0, 0), |(_, size, mtime)| (*size, *mtime));
                    EntryRecord {
                        path: path.as_str().to_owned(),
                        hash: hash.as_str().to_owned(),
                        size,
                        mtime,
                    }
                })
                .collect(),
            added: snapshot.added,
            reused: snapshot.reused,
            total_bytes: snapshot.total_bytes,
            added_bytes: snapshot.added_bytes,
            server_online: snapshot.server_online,
            created_at: snapshot.created_at,
        }
    }
}

/// 文件系统上的快照索引
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotIndex {
    root: PathBuf,
}

impl SnapshotIndex {
    /// 以 `{root}/snapshots` 为索引目录
    #[must_use]
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.join(SNAPSHOTS_DIR),
        }
    }

    /// 索引根目录
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn dir_of(&self, id: &SnapshotId) -> PathBuf {
        self.root.join(id.as_str())
    }

    /// 标为写入中：先建目录并写 `.partial`，此时快照还没有记录
    ///
    /// 崩溃发生在这一步之后、`save` 之前，会留下一个只有 `.partial` 的目录；
    /// [`Self::list_all`] 会把它识别为失败并保留，供用户清理。
    ///
    /// # Errors
    ///
    /// 目录或标记写入失败时返回 [`SnapshotStoreError::Storage`]
    pub fn begin(&self, id: &SnapshotId) -> Result<(), SnapshotStoreError> {
        let dir = self.dir_of(id);
        fs::create_dir_all(&dir).map_err(|err| storage(&dir, err))?;
        let marker = dir.join(PARTIAL_FILE);
        let mut file = fs::File::create(&marker).map_err(|err| storage(&marker, err))?;
        file.write_all(now_marker().as_bytes())
            .map_err(|err| storage(&marker, err))
    }

    /// 标记完成：写记录后删除 `.partial`
    ///
    /// 顺序不能反：先删标记再写记录，崩溃就会留下「看起来完成但没有记录」
    /// 的目录。
    ///
    /// # Errors
    ///
    /// 记录写入或标记删除失败时返回 [`SnapshotStoreError::Storage`]
    pub fn complete(&self, snapshot: &Snapshot) -> Result<(), SnapshotStoreError> {
        self.save(snapshot)?;
        let marker = self.dir_of(&snapshot.id).join(PARTIAL_FILE);
        match fs::remove_file(&marker) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(storage(&marker, err)),
        }
    }

    /// 读取一份快照；`.partial` 存在时状态修正为 `failed`
    fn load(&self, id: &SnapshotId) -> Result<Option<Snapshot>, SnapshotStoreError> {
        let dir = self.dir_of(id);
        let record_path = dir.join(SNAPSHOT_FILE);
        let bytes = match fs::read(&record_path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // 只有 `.partial` 没有记录：写入在 save 之前就中断了
                let marker = dir.join(PARTIAL_FILE);
                if marker.is_file() {
                    return Ok(Some(placeholder(id)));
                }
                return Ok(None);
            }
            Err(err) => return Err(storage(&record_path, err)),
        };
        let record: SnapshotRecord =
            serde_json::from_slice(&bytes).map_err(|err| corrupt(id.as_str(), err))?;
        let mut snapshot = Snapshot::try_from(record)?;
        if dir.join(PARTIAL_FILE).is_file() {
            snapshot.status = SnapshotStatus::Failed;
        }
        Ok(Some(snapshot))
    }

    /// 删除整个快照目录（记录 + 标记）
    fn remove_dir(&self, id: &SnapshotId) -> Result<(), SnapshotStoreError> {
        let dir = self.dir_of(id);
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(storage(&dir, err)),
        }
    }

    /// 扫描索引目录下的全部快照 id
    fn ids(&self) -> Result<Vec<SnapshotId>, SnapshotStoreError> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(storage(&self.root, err)),
        };
        let mut ids = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|err| storage(&self.root, err))?;
            if !entry.path().is_dir() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            // 目录名非法不算快照：宁可漏掉一条，也不让脏名字进路径
            if let Ok(id) = SnapshotId::parse(name) {
                ids.push(id);
            }
        }
        ids.sort();
        Ok(ids)
    }
}

/// 只有 `.partial` 的残留目录：以失败状态占位，让用户能看见并清理
///
/// 占位对象没有任何内容与归属信息，只用于 `list` 里露出残留；不能作为
/// 恢复源（[`SnapshotStatus::Failed`] 不可恢复）。
fn placeholder(id: &SnapshotId) -> Snapshot {
    Snapshot {
        id: id.clone(),
        workspace_id: String::new(),
        server: String::new(),
        label: None,
        parent: None,
        scope: BackupScope::all(),
        status: SnapshotStatus::Failed,
        content: domain::backup::SnapshotContent::default(),
        added: 0,
        reused: 0,
        total_bytes: 0,
        added_bytes: 0,
        server_online: false,
        created_at: 0,
    }
}

impl SnapshotRepository for SnapshotIndex {
    fn begin(&self, id: &SnapshotId) -> Result<(), SnapshotStoreError> {
        Self::begin(self, id)
    }

    fn complete(&self, snapshot: &Snapshot) -> Result<(), SnapshotStoreError> {
        Self::complete(self, snapshot)
    }

    fn save(&self, snapshot: &Snapshot) -> Result<(), SnapshotStoreError> {
        let dir = self.dir_of(&snapshot.id);
        fs::create_dir_all(&dir).map_err(|err| storage(&dir, err))?;
        let path = dir.join(SNAPSHOT_FILE);
        let bytes = serde_json::to_vec_pretty(&SnapshotRecord::from(snapshot))
            .map_err(|err| SnapshotStoreError::Corrupt(format!("{}: {err}", snapshot.id)))?;
        write_atomically(&path, &bytes)
    }

    fn get(&self, id: &SnapshotId) -> Result<Snapshot, SnapshotStoreError> {
        self.load(id)?
            .ok_or_else(|| SnapshotStoreError::NotFound(id.as_str().to_owned()))
    }

    fn list_for_server(&self, server: &str) -> Result<Vec<Snapshot>, SnapshotStoreError> {
        let mut out = Vec::new();
        for id in self.ids()? {
            match self.load(&id) {
                Ok(Some(snapshot)) if snapshot.server == server => out.push(snapshot),
                // 损坏的单条记录不应让整个列表失败：别的快照仍可恢复
                Ok(Some(_) | None) => {}
                Err(err @ SnapshotStoreError::Corrupt(_)) => {
                    tracing::warn!(id = %id, error = %err, "skipping corrupt snapshot record");
                }
                Err(err) => return Err(err),
            }
        }
        out.sort_by(|a, b| (a.created_at, a.id.as_str()).cmp(&(b.created_at, b.id.as_str())));
        Ok(out)
    }

    fn list_all(&self) -> Result<Vec<Snapshot>, SnapshotStoreError> {
        let mut out = Vec::new();
        for id in self.ids()? {
            match self.load(&id) {
                Ok(Some(snapshot)) => out.push(snapshot),
                Ok(None) => {}
                // GC 依据必须完整：跳过损坏记录会让 prune 把它引用的 CAS
                // 对象当成孤儿删掉，而记录仍留在盘上（看起来还能恢复）。
                Err(err) => return Err(err),
            }
        }
        out.sort_by(|a, b| (a.created_at, a.id.as_str()).cmp(&(b.created_at, b.id.as_str())));
        Ok(out)
    }

    fn remove(&self, id: &SnapshotId) -> Result<(), SnapshotStoreError> {
        self.remove_dir(id)
    }
}

fn storage(path: &Path, err: std::io::Error) -> SnapshotStoreError {
    SnapshotStoreError::Storage(Box::new(BackupIoError::io(path, err)))
}

/// 写入标记内容：只有 pid 与时间，便于人工排查残留
fn now_marker() -> String {
    let pid = std::process::id();
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    format!("pid={pid} at={at}\n")
}

/// 原子写入：临时文件 → 落盘 → rename
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), SnapshotStoreError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).map_err(|err| storage(dir, err))?;
    let mut file = tempfile::NamedTempFile::new_in(dir).map_err(|err| storage(dir, err))?;
    file.write_all(bytes).map_err(|err| storage(path, err))?;
    file.as_file_mut()
        .sync_all()
        .map_err(|err| storage(path, err))?;
    file.persist(path).map_err(|err| storage(path, err.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::backup::SnapshotContent;
    use port::workspace_backups_root;

    fn index() -> (tempfile::TempDir, SnapshotIndex) {
        let dir = tempfile::tempdir().expect("tempdir");
        let index = SnapshotIndex::new(dir.path());
        (dir, index)
    }

    fn hash(seed: u8) -> ContentHash {
        ContentHash::parse(&format!("{seed:02x}").repeat(32)).expect("hash")
    }

    fn sample(id: &str, server: &str, at: i64) -> Snapshot {
        Snapshot {
            id: SnapshotId::parse(id).expect("id"),
            workspace_id: "550e8400-e29b-41d4-a716-446655440000".to_owned(),
            server: server.to_owned(),
            label: Some("manual".to_owned()),
            parent: None,
            scope: BackupScope::from_entries(["world"]).expect("scope"),
            status: SnapshotStatus::Complete,
            content: SnapshotContent::new(
                vec![(RelPath::parse("world/level.dat").expect("rel"), hash(1))],
                vec![RelPath::parse("world/old.dat").expect("rel")],
            )
            .expect("content"),
            added: 1,
            reused: 2,
            total_bytes: 100,
            added_bytes: 40,
            server_online: true,
            created_at: at,
        }
    }

    #[test]
    fn save_and_get_round_trips_all_fields() {
        let (_dir, index) = index();
        let snapshot = sample("20260101t000000-a", "alpha", 100);
        index.complete(&snapshot).expect("complete");

        let loaded = index.get(&snapshot.id).expect("get");
        assert_eq!(loaded, snapshot);
        // 完成后不应残留 partial 标记
        assert!(!index.dir_of(&snapshot.id).join(PARTIAL_FILE).exists());
    }

    #[test]
    fn missing_snapshot_reports_not_found() {
        let (_dir, index) = index();
        let err = index
            .get(&SnapshotId::parse("20260101t000000-a").expect("id"))
            .expect_err("missing");
        assert!(matches!(err, SnapshotStoreError::NotFound(_)), "{err}");
    }

    /// 只有 `.partial` 的残留目录：识别为失败，而不是消失
    #[test]
    fn partial_only_directory_is_reported_as_failed() {
        let (_dir, index) = index();
        let id = SnapshotId::parse("20260101t000000-a").expect("id");
        index.begin(&id).expect("begin");

        let listed = index.list_all().expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, SnapshotStatus::Failed);
    }

    /// `begin` 之后 `save` 之前崩溃：崩溃后读取必须报失败
    #[test]
    fn interrupted_write_is_marked_failed() {
        let (_dir, index) = index();
        let snapshot = sample("20260101t000000-a", "alpha", 100);
        index.begin(&snapshot.id).expect("begin");
        // 写入记录但不删标记：模拟「记录写完、标记未删」的中断
        index.save(&snapshot).expect("save");

        let loaded = index.get(&snapshot.id).expect("get");
        assert_eq!(loaded.status, SnapshotStatus::Failed);
        assert!(!loaded.status.is_restorable());
    }

    #[test]
    fn lists_are_scoped_by_server_and_sorted_by_time() {
        let (_dir, index) = index();
        index
            .complete(&sample("20260102t000000-b", "alpha", 200))
            .expect("save");
        index
            .complete(&sample("20260101t000000-a", "alpha", 100))
            .expect("save");
        index
            .complete(&sample("20260103t000000-c", "beta", 300))
            .expect("save");

        let alpha = index.list_for_server("alpha").expect("list");
        assert_eq!(alpha.len(), 2);
        assert_eq!(alpha[0].created_at, 100);
        assert_eq!(alpha[1].created_at, 200);
        assert_eq!(index.list_all().expect("all").len(), 3);
    }

    /// 单条损坏不影响其余快照：否则一份手改的文件会让整个 list 不可用
    #[test]
    fn corrupt_record_is_skipped_not_fatal() {
        let (_dir, index) = index();
        index
            .complete(&sample("20260101t000000-a", "alpha", 100))
            .expect("save");
        let broken = index.dir_of(&SnapshotId::parse("20260102t000000-b").expect("id"));
        fs::create_dir_all(&broken).expect("mkdir");
        fs::write(broken.join(SNAPSHOT_FILE), b"{not json").expect("write");

        let listed = index.list_for_server("alpha").expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id.as_str(), "20260101t000000-a");
    }

    /// GC 视图必须 fail closed：跳过损坏记录会让 prune 删掉仍被引用的对象
    #[test]
    fn list_all_fails_closed_on_corrupt_record() {
        let (_dir, index) = index();
        index
            .complete(&sample("20260101t000000-a", "alpha", 100))
            .expect("save");
        let broken = index.dir_of(&SnapshotId::parse("20260102t000000-b").expect("id"));
        fs::create_dir_all(&broken).expect("mkdir");
        fs::write(broken.join(SNAPSHOT_FILE), b"{not json").expect("write");

        let err = index
            .list_all()
            .expect_err("corrupt record must fail closed");
        assert!(matches!(err, SnapshotStoreError::Corrupt(_)), "{err}");
    }

    #[test]
    fn store_error_on_wrong_schema() {
        let (_dir, index) = index();
        let id = SnapshotId::parse("20260101t000000-a").expect("id");
        let dir = index.dir_of(&id);
        fs::create_dir_all(&dir).expect("mkdir");
        let mut record = SnapshotRecord::from(&sample("20260101t000000-a", "alpha", 100));
        record.schema = 99;
        fs::write(
            dir.join(SNAPSHOT_FILE),
            serde_json::to_vec(&record).expect("serialize"),
        )
        .expect("write");

        let err = index.get(&id).expect_err("schema");
        assert!(matches!(err, SnapshotStoreError::Corrupt(_)), "{err}");
    }

    #[test]
    fn remove_deletes_directory_and_is_idempotent() {
        let (_dir, index) = index();
        let snapshot = sample("20260101t000000-a", "alpha", 100);
        index.complete(&snapshot).expect("save");
        index.remove(&snapshot.id).expect("remove");
        index.remove(&snapshot.id).expect("remove again");
        assert!(index.list_all().expect("list").is_empty());
    }

    #[test]
    fn paths_are_under_workspace_backups() {
        let workspace = Path::new("/ws");
        assert_eq!(
            workspace_backups_root(workspace),
            Path::new("/ws").join("backups")
        );
        let id = SnapshotId::parse("20260101t000000-a").expect("id");
        assert_eq!(
            snapshot_dir(workspace, &id),
            Path::new("/ws")
                .join("backups")
                .join("snapshots")
                .join("20260101t000000-a")
        );
    }
}
