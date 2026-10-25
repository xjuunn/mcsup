//! 内容寻址存储：按 SHA-256 存取文件字节
//!
//! 写入路径：同目录临时文件 → 落盘 → rename。rename 在同一文件系统内是
//! 原子的，因此 CAS 中要么没有该对象，要么是完整对象。
//!
//! 已有对象不重写：内容寻址意味着「同名即同内容」，重写只会多一次 IO。

use domain::backup::ContentHash;
use port::{CasError, ContentStore};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::{CAS_DIR, error::BackupIoError};

/// 读 / 写缓冲大小：与常见文件系统块大小同量级，兼顾大文件吞吐与内存占用
const BUFFER_SIZE: usize = 64 * 1024;

/// 文件系统上的 CAS
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CasStore {
    root: PathBuf,
}

impl CasStore {
    /// 以 `{root}/cas` 为对象根目录；不建目录（首次写入时惰性创建）
    #[must_use]
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.join(CAS_DIR),
        }
    }

    /// 对象根目录
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 对象在磁盘上的路径
    #[must_use]
    pub fn object_path(&self, hash: &ContentHash) -> PathBuf {
        let (first, second) = hash.shard();
        self.root.join(first).join(second).join(hash.as_str())
    }

    /// 逐块读完 `reader`，同时算哈希并写入 `sink`
    ///
    /// 读与写在同一次遍历里完成：先完整读进内存会在几个 G 的存档上直接 OOM。
    fn stream_to(
        reader: &mut dyn Read,
        mut sink: impl FnMut(&[u8]) -> std::io::Result<()>,
    ) -> Result<(ContentHash, u64), CasError> {
        use std::fmt::Write as _;

        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; BUFFER_SIZE];
        let mut total: u64 = 0;
        loop {
            let read = reader.read(&mut buffer).map_err(|err| CasError::Io {
                path: PathBuf::from("<stream>"),
                source: err,
            })?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            sink(&buffer[..read]).map_err(|err| CasError::Io {
                path: PathBuf::from("<stream>"),
                source: err,
            })?;
            total = total.saturating_add(read as u64);
        }
        let mut hex = String::with_capacity(ContentHash::HEX_LEN);
        for byte in hasher.finalize() {
            // 写进 String 不可能失败；忽略返回值比 unwrap 更安全
            let _ = write!(hex, "{byte:02x}");
        }
        let hash = ContentHash::parse(&hex).map_err(|_| CasError::Corrupt {
            hash: ContentHash::parse(&"0".repeat(64)).unwrap_or_else(|_| unreachable!()),
            reason: format!("computed digest {hex} is not a valid sha256 hex"),
        })?;
        Ok((hash, total))
    }

    /// 读文件并算哈希（`put` 的文件入口）
    fn put_file(
        &self,
        path: &Path,
        hint: &domain::backup::RelPath,
    ) -> Result<(ContentHash, u64), CasError> {
        let file = fs::File::open(path).map_err(|err| BackupIoError::io(path, err))?;
        let mut source = std::io::BufReader::with_capacity(BUFFER_SIZE, file);
        self.put(&mut source, hint)
    }

    /// 把 `source` 复制进 CAS；目标已存在则跳过复制但仍算哈希
    ///
    /// 复制到同目录临时文件再 rename：同目录保证 rename 不跨设备。
    fn put_reader(
        &self,
        source: &mut dyn Read,
        hint: &domain::backup::RelPath,
    ) -> Result<(ContentHash, u64), CasError> {
        // 先流式算哈希并写一份临时文件；对象已存在时这份临时文件直接丢弃
        let staging_dir = self.root.clone();
        fs::create_dir_all(&staging_dir).map_err(|err| BackupIoError::io(&staging_dir, err))?;
        let mut staging = tempfile::Builder::new()
            .prefix(".staging-")
            .tempfile_in(&staging_dir)
            .map_err(|err| BackupIoError::io(&staging_dir, err))?;
        let (hash, size) = Self::stream_to(source, |chunk| staging.as_file_mut().write_all(chunk))?;
        staging
            .as_file_mut()
            .sync_all()
            .map_err(|err| BackupIoError::io(staging.path(), err))?;

        let final_path = self.object_path(&hash);
        if final_path.is_file() {
            // 去重命中：内容已在 CAS 中，丢弃暂存文件
            return Ok((hash, size));
        }
        let parent = final_path.parent().unwrap_or(&self.root);
        fs::create_dir_all(parent).map_err(|err| BackupIoError::io(parent, err))?;
        staging
            .persist(&final_path)
            .map_err(|err| BackupIoError::io(&final_path, err.error))?;
        tracing::debug!(hash = %hash, size, hint = %hint, "cas object stored");
        Ok((hash, size))
    }

    /// 读一个目录；不存在时返回空（与删除竞争时不整体失败）
    fn read_dir_optional(dir: &Path) -> Result<Vec<fs::DirEntry>, CasError> {
        match fs::read_dir(dir) {
            Ok(entries) => entries
                .map(|entry| entry.map_err(|err| BackupIoError::io(dir, err).into()))
                .collect(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(err) => Err(BackupIoError::io(dir, err).into()),
        }
    }

    /// 遍历 CAS 目录，产出全部对象哈希
    ///
    /// 布局固定为 `cas/<ab>/<cd>/<hash>` 三级；只读顶层会把对象全漏掉，
    /// 只读两级会被当成分片目录。这里按固定深度展开，不递归——深度是契约。
    fn walk(&self) -> Result<Vec<ContentHash>, CasError> {
        let mut out = Vec::new();
        for first in Self::read_dir_optional(&self.root)? {
            if !first.path().is_dir() {
                continue;
            }
            for second in Self::read_dir_optional(&first.path())? {
                if !second.path().is_dir() {
                    continue;
                }
                for object in Self::read_dir_optional(&second.path())? {
                    let name = object.file_name();
                    let Some(name) = name.to_str() else {
                        continue;
                    };
                    // 暂存文件（`.` 开头的临时名）与非法名一律不算对象
                    if let Ok(hash) = ContentHash::parse(name) {
                        out.push(hash);
                    }
                }
            }
        }
        out.sort();
        Ok(out)
    }
}

impl ContentStore for CasStore {
    fn put(
        &self,
        reader: &mut dyn Read,
        hint: &domain::backup::RelPath,
    ) -> Result<(ContentHash, u64), CasError> {
        self.put_reader(reader, hint)
    }

    fn get(&self, hash: &ContentHash, dest: &Path) -> Result<u64, CasError> {
        let path = self.object_path(hash);
        let source = fs::File::open(&path).map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => CasError::Missing(hash.clone()),
            _ => BackupIoError::io(&path, err).into(),
        })?;
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|err| BackupIoError::io(parent, err))?;
        }
        let mut reader = std::io::BufReader::with_capacity(BUFFER_SIZE, source);
        let mut writer = std::io::BufWriter::with_capacity(
            BUFFER_SIZE,
            fs::File::create(dest).map_err(|err| BackupIoError::io(dest, err))?,
        );
        let mut size: u64 = 0;
        let mut buffer = vec![0u8; BUFFER_SIZE];
        loop {
            let read = reader
                .read(&mut buffer)
                .map_err(|err| BackupIoError::io(&path, err))?;
            if read == 0 {
                break;
            }
            writer
                .write_all(&buffer[..read])
                .map_err(|err| BackupIoError::io(dest, err))?;
            size = size.saturating_add(read as u64);
        }
        writer.flush().map_err(|err| BackupIoError::io(dest, err))?;
        Ok(size)
    }

    fn stat(&self, hash: &ContentHash) -> Result<Option<u64>, CasError> {
        let path = self.object_path(hash);
        match fs::metadata(&path) {
            Ok(meta) if meta.is_file() => Ok(Some(meta.len())),
            Ok(_) => Ok(None),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(BackupIoError::io(&path, err).into()),
        }
    }

    fn verify(
        &self,
        hashes: &[ContentHash],
        on_progress: &mut dyn FnMut(usize, usize),
    ) -> Result<Vec<ContentHash>, CasError> {
        let total = hashes.len();
        let mut broken = Vec::new();
        for (index, hash) in hashes.iter().enumerate() {
            let path = self.object_path(hash);
            match fs::File::open(&path) {
                Ok(file) => {
                    let mut reader = std::io::BufReader::with_capacity(BUFFER_SIZE, file);
                    // 只算哈希不落盘：校验不该写任何东西
                    let (actual, _) = Self::stream_to(&mut reader, |_| Ok(()))?;
                    if &actual != hash {
                        broken.push(hash.clone());
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    broken.push(hash.clone());
                }
                Err(err) => return Err(BackupIoError::io(&path, err).into()),
            }
            on_progress(index + 1, total);
        }
        Ok(broken)
    }

    fn list(&self) -> Result<Vec<ContentHash>, CasError> {
        self.walk()
    }

    fn remove(&self, hash: &ContentHash) -> Result<(), CasError> {
        let path = self.object_path(hash);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(BackupIoError::io(&path, err).into()),
        }
    }
}

impl CasStore {
    /// 直接把磁盘文件纳入 CAS（扫描时的热路径：省一次内存中转）
    ///
    /// # Errors
    ///
    /// 文件读取或对象写入失败时返回 [`port::CasError`]
    pub fn ingest_file(
        &self,
        path: &Path,
        hint: &domain::backup::RelPath,
    ) -> Result<(ContentHash, u64), CasError> {
        self.put_file(path, hint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::backup::RelPath;
    use std::io::Cursor;

    fn store() -> (tempfile::TempDir, CasStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CasStore::new(dir.path());
        (dir, store)
    }

    fn rel(path: &str) -> RelPath {
        RelPath::parse(path).expect("rel")
    }

    #[test]
    fn put_and_get_round_trip() {
        let (dir, store) = store();
        let payload = b"hello world".to_vec();
        let (hash, size) = store
            .put(&mut Cursor::new(payload.clone()), &rel("world/level.dat"))
            .expect("put");
        assert_eq!(size, payload.len() as u64);
        assert_eq!(store.stat(&hash).expect("stat"), Some(size));

        let dest = dir.path().join("out/level.dat");
        assert_eq!(store.get(&hash, &dest).expect("get"), size);
        assert_eq!(fs::read(&dest).expect("read"), payload);
    }

    /// 同内容只存一份：第二次写入不产生新对象
    #[test]
    fn put_is_deduplicated() {
        let (_dir, store) = store();
        let payload = b"same bytes".to_vec();
        let (first, _) = store
            .put(&mut Cursor::new(payload.clone()), &rel("a"))
            .expect("put");
        let (second, _) = store
            .put(&mut Cursor::new(payload), &rel("b"))
            .expect("put");
        assert_eq!(first, second);
        assert_eq!(store.list().expect("list"), vec![first]);
    }

    #[test]
    fn object_path_is_sharded() {
        let (_dir, store) = store();
        let hash = ContentHash::parse(&format!("{}{}", "ab", "c".repeat(62))).expect("hash");
        let path = store.object_path(&hash);
        assert!(path.ends_with(Path::new("ab").join("cc").join(hash.as_str())));
    }

    #[test]
    fn get_missing_object_reports_missing() {
        let (dir, store) = store();
        let hash = ContentHash::parse(&"a".repeat(64)).expect("hash");
        let err = store
            .get(&hash, &dir.path().join("x"))
            .expect_err("missing");
        assert!(matches!(err, CasError::Missing(_)), "{err}");
    }

    #[test]
    fn empty_file_is_valid_object() {
        let (_dir, store) = store();
        let (hash, size) = store
            .put(&mut Cursor::new(Vec::new()), &rel("empty"))
            .expect("put");
        assert_eq!(size, 0);
        assert_eq!(store.stat(&hash).expect("stat"), Some(0));
    }

    #[test]
    fn verify_detects_missing_and_corrupt_objects() {
        let (_dir, store) = store();
        let (hash, _) = store
            .put(&mut Cursor::new(b"payload".to_vec()), &rel("a"))
            .expect("put");
        let (other, _) = store
            .put(&mut Cursor::new(b"payload2".to_vec()), &rel("b"))
            .expect("put");

        // 篡改一个对象内容
        let path = store.object_path(&other);
        fs::write(&path, b"tampered").expect("tamper");
        // 删掉另一个
        fs::remove_file(store.object_path(&hash)).expect("remove");

        let mut steps = 0;
        let broken = store
            .verify(&[hash.clone(), other], &mut |done, total| {
                steps += 1;
                assert_eq!(total, 2);
                assert!(done <= total);
            })
            .expect("verify");
        assert_eq!(steps, 2);
        assert_eq!(broken.len(), 2);
    }

    #[test]
    fn verify_accepts_intact_objects() {
        let (_dir, store) = store();
        let (hash, _) = store
            .put(&mut Cursor::new(b"ok".to_vec()), &rel("a"))
            .expect("put");
        let broken = store.verify(&[hash], &mut |_, _| {}).expect("verify");
        assert!(broken.is_empty());
    }

    #[test]
    fn list_empty_store_is_empty() {
        let (_dir, store) = store();
        assert!(store.list().expect("list").is_empty());
    }

    #[test]
    fn remove_is_idempotent() {
        let (_dir, store) = store();
        let (hash, _) = store
            .put(&mut Cursor::new(b"x".to_vec()), &rel("a"))
            .expect("put");
        store.remove(&hash).expect("remove");
        store.remove(&hash).expect("remove again");
        assert!(store.list().expect("list").is_empty());
    }

    #[test]
    fn ingest_file_copies_into_cas() {
        let (dir, store) = store();
        let source = dir.path().join("level.dat");
        fs::write(&source, b"world").expect("write");
        let (hash, size) = store
            .ingest_file(&source, &rel("level.dat"))
            .expect("ingest");
        assert_eq!(size, 5);
        assert_eq!(store.stat(&hash).expect("stat"), Some(5));
    }

    /// 大文件跨多个缓冲块：流式哈希必须与整体哈希一致
    #[test]
    fn large_file_spans_multiple_buffers() {
        let (dir, store) = store();
        let payload: Vec<u8> = (0..(BUFFER_SIZE * 3 + 17))
            .map(|i| u8::try_from(i % 251).unwrap_or(0))
            .collect();
        let source = dir.path().join("big.bin");
        fs::write(&source, &payload).expect("write");
        let (hash, size) = store.ingest_file(&source, &rel("big.bin")).expect("ingest");
        assert_eq!(size, payload.len() as u64);

        let dest = dir.path().join("out.bin");
        store.get(&hash, &dest).expect("get");
        assert_eq!(fs::read(&dest).expect("read"), payload);
    }
}
