//! 备份领域模型：内容哈希、快照、备份范围与判定规则
//!
//! 备份 = File CAS + Snapshot：文件按内容哈希存入 CAS（相同内容只存一次），
//! 快照记录「相对路径 → 哈希」的映射。增量靠「与父快照比较」得到，
//! 不另存增量包——这是去重与并发的来源，也是 GC 的唯一真相。
//!
//! 本模块只放判定与值对象，不碰文件系统：读盘、写盘、发控制台命令都在
//! `backup` 实现层与 `service` 编排层。

use std::fmt;
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

/// 内容哈希：CAS 的键
///
/// 只用 SHA-256 十六进制小写（64 字符）。不做算法协商：同一份工作区的
/// CAS 必须只有一种键空间，换算法就等于换一套存储。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentHash(String);

impl ContentHash {
    /// 十六进制位数（SHA-256）
    pub const HEX_LEN: usize = 64;

    /// 校验并构造
    ///
    /// # Errors
    ///
    /// 非 64 个十六进制字符时返回 [`InvalidContentHash`]
    pub fn parse(raw: &str) -> Result<Self, InvalidContentHash> {
        if raw.len() == Self::HEX_LEN
            && raw
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            Ok(Self(raw.to_owned()))
        } else {
            Err(InvalidContentHash {
                raw: raw.to_owned(),
            })
        }
    }

    /// 十六进制文本
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// CAS 内的两级分片目录：`ab/cd/<hash>`
    ///
    /// 单目录放十万级文件会让目录扫描与 `readdir` 显著变慢；按哈希前四位
    /// 分片后每片同量级有界。
    #[must_use]
    pub fn shard(&self) -> (&str, &str) {
        (&self.0[0..2], &self.0[2..4])
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 内容哈希字面量非法
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid content hash: {raw}")]
pub struct InvalidContentHash {
    /// 原始输入
    pub raw: String,
}

/// 快照 id：时间序 + 随机后缀，可排序便于「按时间倒序」直接比较
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SnapshotId(String);

impl SnapshotId {
    /// 校验并构造
    ///
    /// 允许字符集是 `[0-9a-z-]`：id 会进文件名与快照目录，放开字母表
    /// 就等于放开路径穿越的可能。
    ///
    /// # Errors
    ///
    /// 空、超过 64 字节或含白名单外字符时返回 [`InvalidSnapshotId`]
    pub fn parse(raw: &str) -> Result<Self, InvalidSnapshotId> {
        if !raw.is_empty()
            && raw.len() <= 64
            && raw
                .bytes()
                .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase() || b == b'-')
        {
            Ok(Self(raw.to_owned()))
        } else {
            Err(InvalidSnapshotId {
                raw: raw.to_owned(),
            })
        }
    }

    /// 十六进制文本
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SnapshotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 快照 id 字面量非法
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid snapshot id: {raw}")]
pub struct InvalidSnapshotId {
    /// 原始输入
    pub raw: String,
}

/// 快照相对路径：服务器目录下的 POSIX 风格相对路径
///
/// 用 `/` 分隔而不是平台分隔符：快照可跨平台读取（工作区随目录整体搬迁），
/// 存 `\` 的元数据换到 unix 上就拼不出文件。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelPath(String);

impl RelPath {
    /// 从相对路径构造并校验
    ///
    /// 规则：非空、只含 `Normal` 分量与 `.`（`.` 直接归一掉）、不以上级跳转
    /// 起始、不含盘符前缀。这是快照能安全落盘与还原的前提——一个 `..`
    /// 就能让恢复写到服务器目录之外。
    ///
    /// # Errors
    ///
    /// 空路径、含 `..` / 根 / 盘符前缀时返回 [`InvalidRelPath`]
    pub fn from_path(path: &Path) -> Result<Self, InvalidRelPath> {
        let mut parts = Vec::new();
        for component in path.components() {
            match component {
                Component::Normal(part) => {
                    let part = part.to_str().ok_or_else(|| InvalidRelPath {
                        raw: path.to_string_lossy().into_owned(),
                    })?;
                    // 盘符 / NTFS ADS 的 `:` 在 Unix 上只是普通字符，却会在
                    // Windows 上把路径变成盘符相对路径；跨平台快照必须拒绝。
                    if part.contains(':') {
                        return Err(InvalidRelPath {
                            raw: path.to_string_lossy().into_owned(),
                        });
                    }
                    parts.push(part.to_owned());
                }
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(InvalidRelPath {
                        raw: path.to_string_lossy().into_owned(),
                    });
                }
            }
        }
        if parts.is_empty() {
            return Err(InvalidRelPath {
                raw: path.to_string_lossy().into_owned(),
            });
        }
        Ok(Self(parts.join("/")))
    }

    /// 从已存盘的 POSIX 文本构造并校验（读元数据用）
    ///
    /// 不能转交 [`Self::from_path`]：`Path::components` 按平台判定分隔符，
    /// 同一条文本在 Windows 上把 `\` 当分隔符切开、在 unix 上原样保留，
    /// 导致「回写后不含反斜杠」这条跨平台不变量随平台漂移。文本一律按
    /// POSIX 规则切分，`\` 在任何平台都是非法字符。
    ///
    /// # Errors
    ///
    /// 同上
    pub fn parse(raw: &str) -> Result<Self, InvalidRelPath> {
        let invalid = || InvalidRelPath {
            raw: raw.to_owned(),
        };
        let mut parts = Vec::new();
        for segment in raw.split('/') {
            match segment {
                "." => {}
                "" | ".." => return Err(invalid()),
                part if part.contains('\\') || part.contains(':') => return Err(invalid()),
                part => parts.push(part),
            }
        }
        if parts.is_empty() {
            return Err(invalid());
        }
        Ok(Self(parts.join("/")))
    }

    /// POSIX 文本
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 拼回平台路径；只在恢复时与服务器目录 `join`
    #[must_use]
    pub fn to_platform_path(&self) -> PathBuf {
        self.0.split('/').collect()
    }

    /// 是否落在给定顶层项之下（备份范围判定用）
    #[must_use]
    pub fn starts_with(&self, top: &str) -> bool {
        self.0 == top
            || self
                .0
                .strip_prefix(top)
                .is_some_and(|rest| rest.starts_with('/'))
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 快照相对路径非法
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid snapshot path: {raw}")]
pub struct InvalidRelPath {
    /// 原始输入
    pub raw: String,
}

/// 备份范围：整个服务器目录，或其中若干顶层项
///
/// 空集合 = 整个目录（默认）。只允许顶层项：备份「world/region 里的几个
/// 目录」既表达不清也挡不住 `level.dat` 这类跨层依赖，范围粒度定在顶层。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackupScope {
    /// 顶层项名，如 `world` / `mods`；空 = 全部
    include: Vec<String>,
}

impl BackupScope {
    /// 全量（整个服务器目录）
    #[must_use]
    pub fn all() -> Self {
        Self::default()
    }

    /// 由顶层项名构造
    ///
    /// # Errors
    ///
    /// 名称为空、含分隔符或为 `.` / `..` 时返回 [`InvalidScopeEntry`]
    pub fn from_entries<I, S>(entries: I) -> Result<Self, InvalidScopeEntry>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut include: Vec<String> = Vec::new();
        for entry in entries {
            let entry = entry.into();
            let trimmed = entry.trim();
            if !is_valid_scope_entry(trimmed) {
                return Err(InvalidScopeEntry { raw: entry });
            }
            if !include.iter().any(|existing| existing == trimmed) {
                include.push(trimmed.to_owned());
            }
        }
        include.sort();
        Ok(Self { include })
    }

    /// 是否备份整个目录
    #[must_use]
    pub fn is_all(&self) -> bool {
        self.include.is_empty()
    }

    /// 参与备份的顶层项；空 = 全部
    #[must_use]
    pub fn entries(&self) -> &[String] {
        &self.include
    }

    /// 给定相对路径是否在范围内
    #[must_use]
    pub fn contains(&self, path: &RelPath) -> bool {
        if self.is_all() {
            return true;
        }
        let first = path.as_str().split('/').next().unwrap_or_default();
        self.include.iter().any(|entry| entry == first)
    }
}

/// 顶层项名：非空、不含分隔符、不是 `.` / `..`
fn is_valid_scope_entry(raw: &str) -> bool {
    !raw.is_empty()
        && raw != "."
        && raw != ".."
        && !raw.contains(['/', '\\', '\0'])
        && raw.len() <= 255
}

/// 备份范围的顶层项非法
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid backup scope entry: {raw}")]
pub struct InvalidScopeEntry {
    /// 原始输入
    pub raw: String,
}

/// 快照内容：相对路径 → 内容哈希
///
/// `deleted` 是删除墓碑：相对父快照被删掉的文件。没有它，从某份快照恢复
/// 后已删文件会「复活」——只记新增与变更是错的。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotContent {
    entries: Vec<(RelPath, ContentHash)>,
    deleted: Vec<RelPath>,
    stamps: Vec<(RelPath, u64, i64)>,
}

impl SnapshotContent {
    /// 由（已排序的）条目与墓碑构造
    ///
    /// # Errors
    ///
    /// 同一路径重复出现时返回 [`DuplicatePath`]
    pub fn new(
        entries: Vec<(RelPath, ContentHash)>,
        deleted: Vec<RelPath>,
    ) -> Result<Self, DuplicatePath> {
        for pair in entries.windows(2) {
            if pair[0].0 == pair[1].0 {
                return Err(DuplicatePath {
                    path: pair[0].0.clone(),
                });
            }
        }
        Ok(Self {
            entries,
            deleted,
            stamps: Vec::new(),
        })
    }

    /// 附带每个文件备份时的 `(size, mtime)` 观察值
    ///
    /// 增量备份的复用提示必须拿**父快照记录时的值**去比对当前磁盘，
    /// 不能拿当前磁盘的值去比当前磁盘——那永远命中，等于永不重算。
    #[must_use]
    pub fn with_stamps(mut self, stamps: Vec<(RelPath, u64, i64)>) -> Self {
        self.stamps = stamps;
        self
    }

    /// 备份时的 `(路径, 大小, 修改时间秒)` 观察值
    #[must_use]
    pub fn stamps(&self) -> &[(RelPath, u64, i64)] {
        &self.stamps
    }

    /// 全部条目（按路径升序）
    #[must_use]
    pub fn entries(&self) -> &[(RelPath, ContentHash)] {
        &self.entries
    }

    /// 删除墓碑（按路径升序）
    #[must_use]
    pub fn deleted(&self) -> &[RelPath] {
        &self.deleted
    }

    /// 条目数
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空快照
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 全部被引用的哈希（GC 引用计数用）
    ///
    /// 快照内同一内容可能对应多个路径（例如相同的小文件），去重后才是
    /// 真正的「需要保留的对象」集合。
    pub fn referenced_hashes(&self) -> impl Iterator<Item = &ContentHash> {
        let mut seen: Vec<&ContentHash> = Vec::with_capacity(self.entries.len());
        for (_, hash) in &self.entries {
            if !seen.contains(&hash) {
                seen.push(hash);
            }
        }
        seen.into_iter()
    }

    /// 逻辑总字节数（同一哈希只算一次）
    ///
    /// 需要哈希 → 大小的映射，故由调用方传入查询闭包：领域层不持有 CAS 索引。
    #[must_use]
    pub fn logical_size(&self, size_of: impl Fn(&ContentHash) -> u64) -> u64 {
        self.referenced_hashes().map(size_of).sum()
    }
}

/// 快照内同一路径重复
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("duplicate path in snapshot: {path}")]
pub struct DuplicatePath {
    /// 重复路径
    pub path: RelPath,
}

/// 快照状态
///
/// 只把「完成」暴露给 `list`：写入中的快照随时可能缺对象，让用户看到并
/// 从它恢复是数据事故。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SnapshotStatus {
    /// 正在写入（`.partial` 标记仍在）
    InProgress,
    /// 已完成，可恢复
    Complete,
    /// 写入被中断，内容不完整
    Failed,
}

impl SnapshotStatus {
    /// 是否可作为恢复源
    #[must_use]
    pub const fn is_restorable(self) -> bool {
        matches!(self, Self::Complete)
    }

    /// 线缆 / 存储口径文本
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Complete => "complete",
            Self::Failed => "failed",
        }
    }

    /// 从存储文本解析
    ///
    /// # Errors
    ///
    /// 未知文本返回 [`InvalidSnapshotStatus`]
    pub fn parse(raw: &str) -> Result<Self, InvalidSnapshotStatus> {
        match raw {
            "in_progress" => Ok(Self::InProgress),
            "complete" => Ok(Self::Complete),
            "failed" => Ok(Self::Failed),
            other => Err(InvalidSnapshotStatus {
                raw: other.to_owned(),
            }),
        }
    }
}

impl fmt::Display for SnapshotStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 快照状态文本非法
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid snapshot status: {raw}")]
pub struct InvalidSnapshotStatus {
    /// 原始输入
    pub raw: String,
}

/// 一份快照的元数据
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// 快照 id
    pub id: SnapshotId,
    /// 所属工作区（CAS 与快照目录的归属）
    pub workspace_id: String,
    /// 所属服务器名
    pub server: String,
    /// 用户标签，如 `before-upgrade`
    pub label: Option<String>,
    /// 父快照：增量基线；首份为 `None`
    pub parent: Option<SnapshotId>,
    /// 备份范围
    pub scope: BackupScope,
    /// 状态
    pub status: SnapshotStatus,
    /// 内容
    pub content: SnapshotContent,
    /// 本次新建的 CAS 对象数（父快照中没有的哈希）
    pub added: usize,
    /// 复用已存在 CAS 对象的文件数
    pub reused: usize,
    /// 逻辑总字节数（文件大小之和）
    pub total_bytes: u64,
    /// 本次实际写入 CAS 的字节数（增量净增）
    pub added_bytes: u64,
    /// 备份时服务器是否在运行（决定 `save-off` 是否被发过）
    pub server_online: bool,
    /// Unix 秒
    pub created_at: i64,
}

impl Snapshot {
    /// 逻辑大小是否全部来自复用（即「本次没有新内容」）
    #[must_use]
    pub fn is_redundant(&self) -> bool {
        self.added == 0 && !self.content.is_empty()
    }
}

/// 恢复目标：原地，或恢复为一个新服务
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreTarget {
    /// 覆盖原服务器目录
    InPlace,
    /// 恢复到新服务器目录（原服务器不动）
    NewServer {
        /// 新服务器名
        name: String,
    },
}

/// 校验：原地恢复时服务器必须已停止
///
/// 运行中的 JVM 持有世界文件句柄，替换文件后它继续写旧 inode：在 Windows
/// 上是被占用直接失败，在 Unix 上是「恢复了但服务器又把旧数据写回去」。
/// 两种都不可接受，故一律要求停在恢复前。
///
/// # Errors
///
/// 原地恢复且服务器存活时返回 [`RestoreWhileRunning`]
pub fn ensure_restore_stopped(
    target: &RestoreTarget,
    alive: bool,
) -> Result<(), RestoreWhileRunning> {
    match target {
        // 恢复到新目录不动原服务，运行与否都允许
        RestoreTarget::NewServer { .. } => Ok(()),
        RestoreTarget::InPlace => {
            if alive {
                Err(RestoreWhileRunning)
            } else {
                Ok(())
            }
        }
    }
}

/// 运行中恢复被拒绝
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("refusing to restore in place while the server is running")]
pub struct RestoreWhileRunning;

/// 保留策略：`--keep N` 与 `--older-than 7d` 二选一
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionPolicy {
    /// 保留最近 N 份，其余删除
    Keep(usize),
    /// 删除早于该秒数的所有快照
    OlderThan(u64),
}

impl RetentionPolicy {
    /// 选出应删除的快照（不改入参顺序：`snapshots` 已按时间升序）
    ///
    /// 写入中的快照不参与「保留」计数：中间中断的快照本就不该占保留名额，
    /// 被算进去会让用户实际能回滚的版本数少于预期。
    #[must_use]
    pub fn victims(self, snapshots: &[Snapshot], now: i64) -> Vec<&Snapshot> {
        let complete: Vec<&Snapshot> = snapshots
            .iter()
            .filter(|snapshot| snapshot.status.is_restorable())
            .collect();
        match self {
            Self::Keep(keep) => {
                let excess = complete.len().saturating_sub(keep);
                complete.into_iter().take(excess).collect()
            }
            Self::OlderThan(age_secs) => {
                let cutoff = now.saturating_sub(i64::try_from(age_secs).unwrap_or(i64::MAX));
                complete
                    .into_iter()
                    .filter(|snapshot| snapshot.created_at < cutoff)
                    .collect()
            }
        }
    }
}

/// 解析 `--older-than` 的时长字面量：`30s` / `15m` / `12h` / `7d` / `4w`
///
/// 不接受裸数字：单位缺失时 7 到底是秒还是天说不清，猜错就是误删几十份快照。
///
/// # Errors
///
/// 无单位、单位未知、数字非法或换算溢出时返回 [`InvalidAge`]
pub fn parse_age(raw: &str) -> Result<u64, InvalidAge> {
    let raw = raw.trim();
    let (digits, unit) = raw.split_at(raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len()));
    let value: u64 = digits.parse().map_err(|_| InvalidAge {
        raw: raw.to_owned(),
    })?;
    let scale = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        "w" => 7 * 24 * 60 * 60,
        _ => {
            return Err(InvalidAge {
                raw: raw.to_owned(),
            });
        }
    };
    value.checked_mul(scale).ok_or_else(|| InvalidAge {
        raw: raw.to_owned(),
    })
}

/// 时长字面量非法
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid duration: {raw} (expected e.g. 30s, 15m, 12h, 7d, 4w)")]
pub struct InvalidAge {
    /// 原始输入
    pub raw: String,
}

/// 分页窗口：从 1 开始的页码
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    /// 页码，从 1 开始
    pub number: usize,
    /// 每页条数
    pub size: usize,
}

impl Page {
    /// 默认每页条数
    pub const DEFAULT_SIZE: usize = 20;

    /// 构造并夹紧：页码至少 1，每页至少 1 条
    #[must_use]
    pub fn new(number: usize, size: usize) -> Self {
        Self {
            number: number.max(1),
            size: size.max(1),
        }
    }

    /// 该页的起始下标（0 基）
    #[must_use]
    pub fn offset(self) -> usize {
        (self.number - 1).saturating_mul(self.size)
    }

    /// 总页数（空集合算 0 页）
    #[must_use]
    pub fn pages(self, total: usize) -> usize {
        total.div_ceil(self.size)
    }

    /// 分页切片
    #[must_use]
    pub fn slice<T>(self, items: &[T]) -> &[T] {
        let start = self.offset().min(items.len());
        let end = start.saturating_add(self.size).min(items.len());
        &items[start..end]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(seed: u8) -> ContentHash {
        ContentHash::parse(&format!("{seed:02x}").repeat(32)).expect("hash")
    }

    fn rel(path: &str) -> RelPath {
        RelPath::parse(path).expect("rel")
    }

    // ---- ContentHash ----

    #[test]
    fn content_hash_requires_lowercase_sha256() {
        let ok = "a".repeat(64);
        assert_eq!(ContentHash::parse(&ok).expect("ok").as_str(), ok.as_str());
        assert!(ContentHash::parse(&"A".repeat(64)).is_err());
        assert!(ContentHash::parse(&"a".repeat(63)).is_err());
        assert!(ContentHash::parse("").is_err());
        assert!(ContentHash::parse(&"g".repeat(64)).is_err());
    }

    #[test]
    fn content_hash_shards_by_prefix() {
        let hash = ContentHash::parse(&format!("{}{}", "ab", "c".repeat(62))).expect("hash");
        assert_eq!(hash.shard(), ("ab", "cc"));
    }

    // ---- SnapshotId ----

    #[test]
    fn snapshot_id_rejects_path_like_input() {
        for bad in ["", "../etc", "a/b", "UPPER", "a b"] {
            assert!(SnapshotId::parse(bad).is_err(), "{bad} should be rejected");
        }
        assert_eq!(
            SnapshotId::parse("20260101t000000-abc123")
                .expect("ok")
                .as_str(),
            "20260101t000000-abc123"
        );
    }

    /// id 按字典序即时间序：`list` 的排序不额外维护时间戳
    #[test]
    fn snapshot_ids_sort_by_time() {
        let mut ids = [
            SnapshotId::parse("20260102t000000-b").expect("id"),
            SnapshotId::parse("20260101t000000-a").expect("id"),
        ];
        ids.sort();
        assert_eq!(ids[0].as_str(), "20260101t000000-a");
    }

    // ---- RelPath ----

    #[test]
    fn rel_path_rejects_traversal_and_absolute() {
        assert!(RelPath::from_path(Path::new("../escape")).is_err());
        assert!(RelPath::from_path(Path::new("world/../../escape")).is_err());
        assert!(RelPath::from_path(Path::new("/abs")).is_err());
        assert!(RelPath::from_path(Path::new("")).is_err());
        assert!(RelPath::from_path(Path::new("./")).is_err());
    }

    #[test]
    fn rel_path_normalizes_cur_dir_and_uses_posix_separator() {
        let path = RelPath::from_path(Path::new("./world/level.dat")).expect("ok");
        assert_eq!(path.as_str(), "world/level.dat");
        assert_eq!(
            path.to_platform_path(),
            Path::new("world").join("level.dat")
        );
    }

    /// 反斜杠在任何平台都是非法字符
    ///
    /// `parse` 曾转交 `from_path`，而 `Path::components` 按平台判分隔符：
    /// Windows 上 `\` 被切开、unix 上原样残留，同一条元数据跨平台读出的
    /// 路径不一致。快照要在平台间搬运，判定必须与平台无关。
    #[test]
    fn rel_path_rejects_backslash_on_every_platform() {
        assert!(RelPath::parse("world\\level.dat").is_err());
        assert!(RelPath::parse("-/\\").is_err());
        assert!(RelPath::parse("a/b\\c").is_err());
        // 正常路径不受影响
        assert_eq!(RelPath::parse("a/b").expect("ok").as_str(), "a/b");
    }

    /// 盘符 / NTFS ADS 的 `:` 在 Unix 上是普通字符，必须按跨平台口径拒绝
    #[test]
    fn rel_path_rejects_colon_on_every_platform() {
        assert!(RelPath::parse("C:/evil").is_err());
        assert!(RelPath::parse("a/C:evil").is_err());
        assert!(RelPath::from_path(Path::new("a/C:evil")).is_err());
    }

    #[test]
    fn rel_path_prefix_match_respects_component_boundary() {
        let path = rel("world_nether/region/r.0.0.mca");
        assert!(path.starts_with("world_nether"));
        assert!(!path.starts_with("world"));
        assert!(rel("world").starts_with("world"));
    }

    // ---- BackupScope ----

    #[test]
    fn empty_scope_means_everything() {
        let scope = BackupScope::all();
        assert!(scope.is_all());
        assert!(scope.contains(&rel("mods/x.jar")));
        assert!(scope.contains(&rel("server.jar")));
    }

    #[test]
    fn scope_limits_to_top_level_entries() {
        let scope = BackupScope::from_entries(["world", "mods"]).expect("scope");
        assert!(!scope.is_all());
        assert!(scope.contains(&rel("world/level.dat")));
        assert!(scope.contains(&rel("mods/a/b.jar")));
        assert!(!scope.contains(&rel("plugins/x.jar")));
        // 前缀相同的兄弟目录不能被误判
        assert!(!scope.contains(&rel("world_nether/x.mca")));
    }

    #[test]
    fn scope_entries_are_deduped_and_sorted() {
        let scope = BackupScope::from_entries(["mods", "world", "mods"]).expect("scope");
        assert_eq!(scope.entries(), ["mods".to_owned(), "world".to_owned()]);
    }

    #[test]
    fn scope_rejects_separators_and_dot_entries() {
        for bad in ["", ".", "..", "a/b", "a\\b"] {
            assert!(BackupScope::from_entries([bad]).is_err(), "{bad}");
        }
    }

    // ---- SnapshotContent ----

    #[test]
    fn content_rejects_duplicate_paths() {
        let entries = vec![
            (rel("world/level.dat"), hash(1)),
            (rel("world/level.dat"), hash(2)),
        ];
        assert!(SnapshotContent::new(entries, Vec::new()).is_err());
    }

    #[test]
    fn content_counts_referenced_hashes_once() {
        let entries = vec![
            (rel("a"), hash(1)),
            (rel("b"), hash(1)),
            (rel("c"), hash(2)),
        ];
        let content = SnapshotContent::new(entries, Vec::new()).expect("content");
        assert_eq!(content.referenced_hashes().count(), 2);
        assert_eq!(content.logical_size(|_| 10), 20);
    }

    /// 墓碑是「删除」的唯一记录：没有它，恢复后已删文件会复活
    #[test]
    fn content_keeps_deletion_tombstones() {
        let content = SnapshotContent::new(vec![], vec![rel("world/old.dat")]).expect("content");
        assert!(content.is_empty());
        assert_eq!(content.deleted(), [rel("world/old.dat")]);
    }

    // ---- RetentionPolicy ----

    fn snapshot(id: &str, at: i64, status: SnapshotStatus) -> Snapshot {
        Snapshot {
            id: SnapshotId::parse(id).expect("id"),
            workspace_id: "ws".to_owned(),
            server: "alpha".to_owned(),
            label: None,
            parent: None,
            scope: BackupScope::all(),
            status,
            content: SnapshotContent::default(),
            added: 0,
            reused: 0,
            total_bytes: 0,
            added_bytes: 0,
            server_online: false,
            created_at: at,
        }
    }

    #[test]
    fn keep_policy_drops_oldest_beyond_limit() {
        let list = vec![
            snapshot("s1", 10, SnapshotStatus::Complete),
            snapshot("s2", 20, SnapshotStatus::Complete),
            snapshot("s3", 30, SnapshotStatus::Complete),
        ];
        let victims = RetentionPolicy::Keep(2).victims(&list, 100);
        assert_eq!(victims.len(), 1);
        assert_eq!(victims[0].id.as_str(), "s1");
    }

    /// 未完成的快照不占保留名额：它们本就不可恢复
    #[test]
    fn keep_policy_ignores_incomplete_snapshots() {
        let list = vec![
            snapshot("s1", 10, SnapshotStatus::Failed),
            snapshot("s2", 20, SnapshotStatus::Complete),
            snapshot("s3", 30, SnapshotStatus::Complete),
        ];
        assert!(RetentionPolicy::Keep(2).victims(&list, 100).is_empty());
    }

    #[test]
    fn older_than_policy_uses_cutoff() {
        let list = vec![
            snapshot("s1", 1_000, SnapshotStatus::Complete),
            snapshot("s2", 9_000, SnapshotStatus::Complete),
        ];
        let victims = RetentionPolicy::OlderThan(60 * 60).victims(&list, 10_000);
        assert_eq!(victims.len(), 1);
        assert_eq!(victims[0].id.as_str(), "s1");
    }

    #[test]
    fn keep_zero_removes_every_complete_snapshot() {
        let list = vec![snapshot("s1", 10, SnapshotStatus::Complete)];
        assert_eq!(RetentionPolicy::Keep(0).victims(&list, 100).len(), 1);
    }

    // ---- parse_age ----

    #[test]
    fn age_parses_units() {
        assert_eq!(parse_age("30s").expect("age"), 30);
        assert_eq!(parse_age("15m").expect("age"), 900);
        assert_eq!(parse_age("12h").expect("age"), 43_200);
        assert_eq!(parse_age("7d").expect("age"), 604_800);
        assert_eq!(parse_age("1w").expect("age"), 604_800);
    }

    /// 裸数字没有单位：7 是秒还是天说不清，猜错就是误删
    #[test]
    fn age_requires_a_unit() {
        for bad in ["7", "", "d", "7y", "-1d", "1.5d"] {
            assert!(parse_age(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn age_overflow_is_rejected() {
        assert!(parse_age(&format!("{}d", u64::MAX)).is_err());
    }

    // ---- Page ----

    #[test]
    fn page_slices_and_counts() {
        let items: Vec<u32> = (0..45).collect();
        let page = Page::new(2, 20);
        assert_eq!(page.offset(), 20);
        assert_eq!(page.slice(&items), &items[20..40]);
        assert_eq!(page.pages(items.len()), 3);
        assert_eq!(Page::new(3, 20).slice(&items), &items[40..45]);
        assert!(Page::new(9, 20).slice(&items).is_empty());
        assert_eq!(Page::new(0, 0), Page { number: 1, size: 1 });
        assert_eq!(Page::new(1, 20).pages(0), 0);
    }

    // ---- restore guard ----

    #[test]
    fn in_place_restore_requires_stopped_server() {
        assert!(ensure_restore_stopped(&RestoreTarget::InPlace, false).is_ok());
        assert_eq!(
            ensure_restore_stopped(&RestoreTarget::InPlace, true).expect_err("running"),
            RestoreWhileRunning
        );
        // 恢复到新服务不动原服务，运行与否都允许
        let target = RestoreTarget::NewServer {
            name: "copy".to_owned(),
        };
        assert!(ensure_restore_stopped(&target, true).is_ok());
    }

    #[test]
    fn redundant_snapshot_means_no_new_content() {
        let mut snap = snapshot("s1", 10, SnapshotStatus::Complete);
        snap.content =
            SnapshotContent::new(vec![(rel("a"), hash(1))], Vec::new()).expect("content");
        assert!(snap.is_redundant());
        snap.added = 1;
        assert!(!snap.is_redundant());
    }

    #[test]
    fn status_round_trips_through_storage_text() {
        for status in [
            SnapshotStatus::InProgress,
            SnapshotStatus::Complete,
            SnapshotStatus::Failed,
        ] {
            assert_eq!(
                SnapshotStatus::parse(status.as_str()).expect("parse"),
                status
            );
        }
        assert!(SnapshotStatus::parse("unknown").is_err());
    }
}
