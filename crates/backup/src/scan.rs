//! 服务器目录扫描：把磁盘状态转成「相对路径 → 内容哈希」
//!
//! 扫描是备份的热路径，规则必须显式：
//!
//! - **不跟随符号链接**：跟着走会把服务器目录外的内容读进备份，恢复时
//!   又写到目录外（`reliability-security.md` 点名的高危面）。
//! - **跳过运行期文件**：`run/`（pid / socket）与 `console.log` 在备份途中
//!   仍在被 JVM 写入，复制它们只会得到截断内容，且对恢复毫无意义。
//! - **按范围过滤**：范围判定在进入目录前做，避免白扫一棵大树。

use domain::backup::{BackupScope, ContentHash, RelPath};
use port::ContentStore;
use std::fs;
use std::path::Path;

use crate::error::{BackupIoError, ScanError, SkipReason, Skipped};

/// 运行期文件 / 目录：不进备份
///
/// `console.log` 与 `run/` 由 supervisor 在服务器运行期间持续写入，
/// 复制它们既不稳定也没有恢复价值。
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

/// 扫描 `root` 并把每个文件的字节写进 `store`
///
/// `on_file` 在每个文件处理完时回调（已处理文件数），供入口画进度。
///
/// # Errors
///
/// 目录不可读、路径非法或 CAS 写入失败时返回 [`ScanError`]
pub fn scan_tree(
    root: &Path,
    store: &dyn ContentStore,
    rules: &ScanRules,
    on_file: &mut dyn FnMut(usize),
) -> Result<ScanOutcome, ScanError> {
    let mut outcome = ScanOutcome::default();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            // 目录在扫描途中消失（另一进程清理）：跳过而非整体失败
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(ScanError::Unreadable {
                    path: dir.clone(),
                    source: err,
                });
            }
        };
        let mut children: Vec<_> = entries
            .map(|entry| {
                entry.map_err(|err| ScanError::Unreadable {
                    path: dir.clone(),
                    source: err,
                })
            })
            .collect::<Result<_, _>>()?;
        // 目录遍历顺序由文件系统决定；排序后输出稳定，快照可比对
        children.sort_by_key(fs::DirEntry::file_name);

        for entry in children {
            let path = entry.path();
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let Ok(rel_path) = RelPath::from_path(relative) else {
                continue;
            };
            if !rules.scope.contains(&rel_path) {
                outcome.skipped.push(Skipped {
                    path: rel_path,
                    reason: SkipReason::OutOfScope,
                });
                continue;
            }
            // 顶层 `run/` 整棵跳过：里面的 pid / socket 都在被进程使用，
            // 逐个文件判断只会白遍历一次
            if is_runtime_entry(&rel_path) {
                outcome.skipped.push(Skipped {
                    path: rel_path,
                    reason: SkipReason::Runtime,
                });
                continue;
            }

            // `symlink_metadata` 不跟随链接：链接本身不是常规文件
            let meta = match fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => return Err(BackupIoError::io(&path, err).into()),
            };
            let file_type = meta.file_type();
            if file_type.is_symlink() {
                outcome.skipped.push(Skipped {
                    path: rel_path,
                    reason: SkipReason::Symlink,
                });
                continue;
            }
            if file_type.is_dir() {
                pending.push(path);
                continue;
            }
            if !file_type.is_file() {
                outcome.skipped.push(Skipped {
                    path: rel_path,
                    reason: SkipReason::NotRegularFile,
                });
                continue;
            }

            let size = meta.len();
            let mtime = mtime_secs(&meta);
            // 复用提示只信任「同路径 + 同大小 + 同 mtime」：任一不同都必须重算
            let reused = rules
                .reuse
                .iter()
                .find(|(known, _, known_size, known_mtime)| {
                    known == &rel_path && *known_size == size && *known_mtime == mtime
                })
                .map(|(_, hash, _, _)| hash.clone());

            let (hash, written) = if let Some(hash) = reused {
                (hash, 0)
            } else {
                let file = fs::File::open(&path).map_err(|err| ScanError::UnreadableFile {
                    path: path.clone(),
                    source: err,
                })?;
                let mut reader = std::io::BufReader::new(file);
                let (hash, written) = store.put(&mut reader, &rel_path)?;
                outcome.added += 1;
                (hash, written)
            };

            outcome.total_bytes = outcome.total_bytes.saturating_add(size);
            outcome.written_bytes = outcome.written_bytes.saturating_add(written);
            outcome.stamps.push((rel_path.clone(), size, mtime));
            outcome.entries.push((rel_path, hash));
            on_file(outcome.entries.len());
        }
    }
    outcome.entries.sort_by(|a, b| a.0.cmp(&b.0));
    outcome.stamps.sort_by(|a, b| a.0.cmp(&b.0));
    outcome.skipped.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(outcome)
}

/// 是否运行期条目（仅顶层，`run/` 与 `console.log` 都在服务器根下）
///
/// 判定在进入目录之前：`run/` 整棵不展开，免去逐个文件判定的遍历开销。
fn is_runtime_entry(path: &RelPath) -> bool {
    let first = path.as_str().split('/').next().unwrap_or_default();
    RUNTIME_ENTRIES.iter().any(|entry| entry == &first)
}

/// 修改时间（Unix 秒）；取不到时为 0（此时不会命中复用，只会多重算一次）
fn mtime_secs(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| i64::try_from(since.as_secs()).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CasStore;

    fn rel(path: &str) -> RelPath {
        RelPath::parse(path).expect("rel")
    }

    fn fixture() -> (tempfile::TempDir, tempfile::TempDir, CasStore) {
        let server = tempfile::tempdir().expect("server");
        let store_dir = tempfile::tempdir().expect("store");
        let store = CasStore::new(store_dir.path());
        (server, store_dir, store)
    }

    fn write(root: &Path, path: &str, bytes: &[u8]) {
        let target = root.join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(target, bytes).expect("write");
    }

    fn scan(root: &Path, store: &CasStore, rules: &ScanRules) -> ScanOutcome {
        scan_tree(root, store, rules, &mut |_| {}).expect("scan")
    }

    #[test]
    fn scans_nested_directories_and_sorts() {
        let (server, _store_dir, store) = fixture();
        write(server.path(), "world/level.dat", b"level");
        write(server.path(), "world/region/r.0.0.mca", b"region");
        write(server.path(), "server.properties", b"motd=x");

        let outcome = scan(server.path(), &store, &ScanRules::default());
        let paths: Vec<&str> = outcome
            .entries
            .iter()
            .map(|(path, _)| path.as_str())
            .collect();
        assert_eq!(
            paths,
            [
                "server.properties",
                "world/level.dat",
                "world/region/r.0.0.mca"
            ]
        );
        assert_eq!(outcome.total_bytes, 5 + 6 + 6);
        assert_eq!(outcome.added, 3);
    }

    #[test]
    fn empty_directory_yields_empty_snapshot() {
        let (server, _store_dir, store) = fixture();
        let outcome = scan(server.path(), &store, &ScanRules::default());
        assert!(outcome.entries.is_empty());
        assert_eq!(outcome.total_bytes, 0);
        assert_eq!(outcome.added, 0);
    }

    /// `run/` 与 `console.log` 由运行中的进程持续写入：不进备份
    #[test]
    fn runtime_entries_are_skipped() {
        let (server, _store_dir, store) = fixture();
        write(server.path(), "console.log", b"log line");
        write(server.path(), "run/mcsup.pid", b"1234");
        write(server.path(), "world/level.dat", b"level");

        let outcome = scan(server.path(), &store, &ScanRules::default());
        assert_eq!(outcome.entries.len(), 1);
        let skipped: Vec<(&str, SkipReason)> = outcome
            .skipped
            .iter()
            .map(|item| (item.path.as_str(), item.reason))
            .collect();
        assert!(skipped.contains(&("console.log", SkipReason::Runtime)));
        // 整个 `run/` 在展开前就被跳过，因此记录的是目录本身
        assert!(skipped.contains(&("run", SkipReason::Runtime)));
    }

    /// 范围只放行指定的顶层项，其余在被读取之前就跳过
    #[test]
    fn scope_limits_scanned_files() {
        let (server, _store_dir, store) = fixture();
        write(server.path(), "world/level.dat", b"level");
        write(server.path(), "mods/x.jar", b"jar");
        write(server.path(), "plugins/y.jar", b"plugin");

        let rules = ScanRules {
            scope: BackupScope::from_entries(["world"]).expect("scope"),
            reuse: Vec::new(),
        };
        let outcome = scan(server.path(), &store, &rules);
        assert_eq!(outcome.entries.len(), 1);
        assert_eq!(outcome.entries[0].0.as_str(), "world/level.dat");
        assert_eq!(outcome.added, 1);
    }

    /// 复用命中的条目不再读盘，也不新增 CAS 对象（增量备份的核心）
    #[test]
    fn reuse_hint_skips_reading_unchanged_files() {
        let (server, _store_dir, store) = fixture();
        write(server.path(), "world/level.dat", b"level");
        let first = scan(server.path(), &store, &ScanRules::default());
        let (path, hash) = first.entries[0].clone();

        let meta = fs::metadata(server.path().join("world/level.dat")).expect("meta");
        let rules = ScanRules {
            scope: BackupScope::all(),
            reuse: vec![(path, hash.clone(), meta.len(), mtime_secs(&meta))],
        };
        let second = scan(server.path(), &store, &rules);
        assert_eq!(second.added, 0);
        assert_eq!(second.written_bytes, 0);
        assert_eq!(second.entries[0].1, hash);
        assert_eq!(second.total_bytes, meta.len());
    }

    /// 大小或 mtime 变化即视为变更，重新读盘
    #[test]
    fn changed_file_is_not_reused() {
        let (server, _store_dir, store) = fixture();
        write(server.path(), "world/level.dat", b"level");
        let first = scan(server.path(), &store, &ScanRules::default());
        let (path, hash) = first.entries[0].clone();

        write(server.path(), "world/level.dat", b"level-v2");
        let meta = fs::metadata(server.path().join("world/level.dat")).expect("meta");
        let rules = ScanRules {
            scope: BackupScope::all(),
            // 用旧大小 + 旧 mtime，模拟父快照记录
            reuse: vec![(path, hash, 5, mtime_secs(&meta) - 1)],
        };
        let second = scan(server.path(), &store, &rules);
        assert_eq!(second.added, 1);
        assert_ne!(second.entries[0].1, first.entries[0].1);
    }

    /// 符号链接不跟随：既不进备份，也不把目标带进来
    #[cfg(unix)]
    #[test]
    fn symlinks_are_skipped_not_followed() {
        let (server, _store_dir, store) = fixture();
        let outside = tempfile::tempdir().expect("outside");
        fs::write(outside.path().join("secret"), b"secret").expect("write");
        write(server.path(), "world/level.dat", b"level");
        std::os::unix::fs::symlink(outside.path(), server.path().join("escape")).expect("symlink");

        let outcome = scan(server.path(), &store, &ScanRules::default());
        assert_eq!(outcome.entries.len(), 1);
        assert!(
            outcome.skipped.iter().any(|item| {
                item.path.as_str() == "escape" && item.reason == SkipReason::Symlink
            })
        );
    }

    /// 同一内容出现在两个路径：只写一个 CAS 对象
    #[test]
    fn duplicate_content_is_deduplicated() {
        let (server, _store_dir, store) = fixture();
        write(server.path(), "a/data.bin", b"identical");
        write(server.path(), "b/data.bin", b"identical");

        let outcome = scan(server.path(), &store, &ScanRules::default());
        let hashes: Vec<&ContentHash> = outcome.entries.iter().map(|(_, hash)| hash).collect();
        assert_eq!(hashes[0], hashes[1]);
        assert_eq!(store.list().expect("list").len(), 1);
    }

    #[test]
    fn progress_callback_counts_processed_files() {
        let (server, _store_dir, store) = fixture();
        write(server.path(), "a", b"1");
        write(server.path(), "b", b"2");

        let mut seen = Vec::new();
        scan_tree(server.path(), &store, &ScanRules::default(), &mut |done| {
            seen.push(done);
        })
        .expect("scan");
        assert_eq!(seen, [1, 2]);
    }

    #[test]
    fn runtime_entry_matching_is_top_level_only() {
        assert!(is_runtime_entry(&rel("run")));
        assert!(is_runtime_entry(&rel("console.log")));
        assert!(is_runtime_entry(&rel("run/mcsup.pid")));
        assert!(!is_runtime_entry(&rel("world/run")));
        assert!(!is_runtime_entry(&rel("mods/console.log")));
    }
}
