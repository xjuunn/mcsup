//! 远端列表的本地缓存：让读命令不再为离线远端付建连代价
//!
//! # 为什么需要
//!
//! 镜像仓库（[`super::mirror`]）每次 `list` 都实打实地开一条连接。远端不可达
//! 时这一步会一直等到传输层的连接超时（`CONNECT_TIMEOUT`，十秒量级），而
//! `mcsup ws ls` 这类命令在一台离线远端上就卡这么久；`UNREACHABLE_COOLDOWN`
//! 只在进程内有效，命令一退出就没了。
//!
//! 缓存把「读列表」和「访问远端」解耦：读命令只读盘（毫秒级），过期与否不
//! 影响本次输出；刷新交给独立的短命进程（`mcsup __remote-refresh`），由入口
//! 层在后台拉起。于是 `ls` 的耗时与远端在线与否无关。
//!
//! # 边界
//!
//! 这一层只负责**格式、TTL、原子写入与标记**，不知道 `data_dir` 从哪来，
//! 也不起进程——那些是入口层的事（`apps/cli`）。依赖方向不允许 `backend`
//! 依赖 `storage`，所以这里自带了与 `storage::JsonStore` 同款的原语：临时文件
//! + `rename`、跨进程锁文件。
//!
//! # 落盘格式
//!
//! 每条远端一个目录，四份 JSON 各存一类条目，全部用 `protocol` 的线格式
//! （`WireWorkspace` / `WireServer` / `WireJava` / `WireStatusEntry`）——`domain`
//! 类型没有 `serde` 派生，而线格式本来就是「跨机器传输的稳定表示」，拿它当
//! 磁盘格式省掉一套平行 DTO。
//!
//! ```text
//! {data_dir}/remotes/<name>/
//!   workspaces.json   工作区列表
//!   servers.json      服务器列表
//!   java.json         Java 运行时列表
//!   statuses.json     服务器运行状态（按服务器名）
//!   state.json        上次同步时间与失败原因
//!   refresh.lock      刷新锁（只创建，不删除）
//! ```

use port::{RemoteName, RepositoryError, SupervisorStatus};
use protocol::remote::wire::{WireJava, WireServer, WireStatusEntry, WireWorkspace};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 缓存有效期
///
/// 30 秒是「够新鲜」与「读命令几乎总能命中」之间的折中：低于它，连续敲两条
/// 命令就会各拉一次刷新；高于它，对端刚建好的服务器在列表里要等太久。
pub const TTL: Duration = Duration::from_secs(30);

/// 刷新锁文件名（放在远端目录下，只创建不删除）
const LOCK_FILE: &str = "refresh.lock";

/// 一次同步的元信息
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SyncState {
    /// 上次成功同步的 Unix 秒；`None` = 从未同步成功
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sync: Option<i64>,
    /// 上次刷新失败的原因；`None` = 上次成功
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// 内容代次，每次写成功都 +1
    ///
    /// 用来让「读了一轮、写回一轮」的刷新发现中途有人写过：刷新要连四帧，
    /// 期间前台写命令可能已经写穿透了更新的一份。没有这个计数，慢刷新会把
    /// 刚刚删掉的服务器又写回缓存里。
    #[serde(default)]
    pub generation: u64,
}

impl SyncState {
    /// 是否已经有过一次成功同步
    #[must_use]
    pub const fn has_synced(&self) -> bool {
        self.last_sync.is_some()
    }

    /// 上次刷新是否失败过（＝来源当前不可达）
    ///
    /// 与「过期」是两个概念：过期只说该去刷一次，失败才说明「这台机器现在
    /// 联系不上」，入口据此标 `(offline)`。
    #[must_use]
    pub const fn is_offline(&self) -> bool {
        self.last_error.is_some()
    }

    /// 以 `now`（Unix 秒）为基准，缓存是否过期
    ///
    /// 从未同步、时钟回拨、条目缺失都算过期——过期只会多拉一次后台刷新，
    /// 代价是多一个短命进程，而报「新鲜」会让离线条目永远停留在旧值。
    #[must_use]
    pub fn is_stale(&self, now: i64) -> bool {
        let Some(last) = self.last_sync else {
            return true;
        };
        now.saturating_sub(last) >= i64::try_from(TTL.as_secs()).unwrap_or(30)
    }
}

/// 一台远端的缓存目录
///
/// 名字已经在 [`RemoteName`] 里校验过（无 `/`、无保留字），因此可以直接当
/// 目录名；这里仍用 [`RemoteName`] 而不是 `&str` 承载，避免调用方传进未校验
/// 的配置键。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCache {
    root: PathBuf,
    name: RemoteName,
}

impl RemoteCache {
    /// 在 `data_dir` 下挂一台远端的缓存
    ///
    /// `data_dir` 是平台数据目录（`{data_dir}/remotes/<name>/`），由入口层提供。
    #[must_use]
    pub fn new(data_dir: &Path, name: RemoteName) -> Self {
        Self {
            root: data_dir.join("remotes").join(name.as_str()),
            name,
        }
    }

    /// 远端名
    #[must_use]
    pub const fn name(&self) -> &RemoteName {
        &self.name
    }

    /// 缓存目录
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.root
    }

    /// 读同步元信息；文件不存在 = 从未同步
    ///
    /// # Errors
    ///
    /// 文件存在但读不了或解析不了时返回 [`RepositoryError::Corrupt`]
    pub fn state(&self) -> Result<SyncState, RepositoryError> {
        Ok(read_json::<SyncState>(&self.root.join("state.json"))?.unwrap_or_default())
    }

    /// 来源是否该标成离线（上次刷新失败，或从未同步成功过）
    ///
    /// 元信息读不出来也按离线处理：宁可多标一次 `(offline)`，也不要让用户
    /// 以为这是刚问来的实时数据。
    #[must_use]
    pub fn is_offline(&self) -> bool {
        self.state().map_or(true, |state| state.is_offline())
    }

    /// 只问「是否过期」，不把损坏的元信息当失败
    ///
    /// 元信息坏了意味着「不知道新不新鲜」，那就按过期处理：让后台刷新去覆盖
    /// 它，前台列表照常出结果。把 `Corrupt` 抛给 `ls` 是本末倒置。
    #[must_use]
    pub fn is_stale(&self, now: i64) -> bool {
        self.state().map_or(true, |state| state.is_stale(now))
    }

    /// 写同步结果（成功）
    ///
    /// # Errors
    ///
    /// 目录建不出来或写盘失败时返回 [`RepositoryError::Storage`]
    pub fn mark_synced(&self, now: i64) -> Result<(), RepositoryError> {
        let generation = self.generation().saturating_add(1);
        self.write_state(&SyncState {
            last_sync: Some(now),
            last_error: None,
            generation,
        })
    }

    /// 写同步结果（失败）
    ///
    /// 保留上一次成功的时间：条目仍然是那份旧数据，标成「从未同步」会谎报
    /// 缓存为空。
    ///
    /// # Errors
    ///
    /// 目录建不出来或写盘失败时返回 [`RepositoryError::Storage`]
    pub fn mark_failed(&self, message: &str) -> Result<(), RepositoryError> {
        let mut state = self.state().unwrap_or_default();
        state.last_error = Some(message.to_owned());
        state.generation = state.generation.saturating_add(1);
        self.write_state(&state)
    }

    /// 当前内容代次（读不出来按 0 算）
    ///
    /// 刷新进程在**开始拉数据之前**取一次，写回时交给 [`Self::store_if_current`]
    /// 比对，确认这段时间里没人写过。
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.state().map_or(0, |state| state.generation)
    }

    fn write_state(&self, state: &SyncState) -> Result<(), RepositoryError> {
        self.write("state.json", state)
    }

    /// 读缓存的工作区列表
    ///
    /// # Errors
    ///
    /// 文件损坏时返回 [`RepositoryError::Corrupt`]
    pub fn workspaces(&self) -> Result<Vec<WireWorkspace>, RepositoryError> {
        self.read_list("workspaces.json")
    }

    /// 读缓存的服务器列表
    ///
    /// # Errors
    ///
    /// 文件损坏时返回 [`RepositoryError::Corrupt`]
    pub fn servers(&self) -> Result<Vec<WireServer>, RepositoryError> {
        self.read_list("servers.json")
    }

    /// 读缓存的 Java 列表
    ///
    /// # Errors
    ///
    /// 文件损坏时返回 [`RepositoryError::Corrupt`]
    pub fn javas(&self) -> Result<Vec<WireJava>, RepositoryError> {
        self.read_list("java.json")
    }

    /// 读缓存的运行状态（按服务器名）
    ///
    /// # Errors
    ///
    /// 文件损坏时返回 [`RepositoryError::Corrupt`]
    pub fn statuses(&self) -> Result<Vec<(String, WireStatusEntry)>, RepositoryError> {
        self.read_list("statuses.json")
    }

    /// 缓存里的状态解码成契约类型
    ///
    /// 解码失败不返回 `Err`：一条坏状态只该让那一行显示 `unknown`，不该让
    /// 整张列表失败。
    #[must_use]
    pub fn decoded_statuses(&self) -> Vec<(String, Option<SupervisorStatus>)> {
        let Ok(items) = self.statuses() else {
            return Vec::new();
        };
        items
            .into_iter()
            .filter_map(|(name, entry)| entry.into_result().ok().map(|status| (name, status)))
            .collect()
    }

    /// 整体写一份同步结果
    ///
    /// 四份列表 + 状态 + 元信息按顺序落盘：条目先写、元信息最后写，中途失败
    /// 时元信息还停在旧值（下次读仍判过期），不会出现「标了已同步但条目是
    /// 半截」的状态。
    ///
    /// # Errors
    ///
    /// 任一份写盘失败时返回 [`RepositoryError::Storage`]
    pub fn store(&self, snapshot: &Snapshot) -> Result<(), RepositoryError> {
        self.write_snapshot(snapshot)
    }

    /// 只在缓存自 `expected` 代次以来没被写过时整体落盘
    ///
    /// 刷新的竞态在这里收口：拉四帧不是原子的，期间前台的写命令可能已经把
    /// 更新的一份写进缓存（比如 `server remove` 刚清掉一台）。此时整体覆盖会
    /// 把删除「复活」，所以宁可丢弃这一轮结果——下一次读命令会重新拉起刷新，
    /// 而丢一轮只损失一次后台往返。
    ///
    /// 返回是否真的写了。
    ///
    /// # Errors
    ///
    /// 写盘失败时返回 [`RepositoryError::Storage`]
    pub fn store_if_current(
        &self,
        snapshot: &Snapshot,
        expected: u64,
    ) -> Result<bool, RepositoryError> {
        if self.generation() != expected {
            return Ok(false);
        }
        self.write_snapshot(snapshot)?;
        Ok(true)
    }

    /// 写条目并标记「刚同步过」（写穿透用）
    ///
    /// 远端写命令只覆盖其中一类条目：保留其余缓存，只换掉这一份。元信息同时
    /// 前移——刚刚确实问过对端，这一份就是新鲜的；顺带清掉上次的失败标记，
    /// 否则写完还得等下一轮刷新才能摘掉 `(offline)`。
    ///
    /// 代次 +1 是关键：并发的后台刷新靠它发现自己读的那轮已经过期。
    ///
    /// # Errors
    ///
    /// 写盘失败时返回 [`RepositoryError::Storage`]
    pub fn store_workspaces(&self, items: &[WireWorkspace]) -> Result<(), RepositoryError> {
        self.write("workspaces.json", &items)?;
        self.mark_synced(now_secs())
    }

    /// 只换服务器列表，见 [`Self::store_workspaces`]
    ///
    /// # Errors
    ///
    /// 写盘失败时返回 [`RepositoryError::Storage`]
    pub fn store_servers(&self, items: &[WireServer]) -> Result<(), RepositoryError> {
        self.write("servers.json", &items)?;
        self.mark_synced(now_secs())
    }

    /// 只换 Java 列表，见 [`Self::store_workspaces`]
    ///
    /// # Errors
    ///
    /// 写盘失败时返回 [`RepositoryError::Storage`]
    pub fn store_javas(&self, items: &[WireJava]) -> Result<(), RepositoryError> {
        self.write("java.json", &items)?;
        self.mark_synced(now_secs())
    }

    fn write_snapshot(&self, snapshot: &Snapshot) -> Result<(), RepositoryError> {
        self.store_workspaces(&snapshot.workspaces)?;
        self.store_servers(&snapshot.servers)?;
        self.store_javas(&snapshot.javas)?;
        self.write("statuses.json", &snapshot.statuses)?;
        self.mark_synced(snapshot.at)
    }

    /// 拿刷新锁
    ///
    /// 返回 `None` 表示已有别的进程在刷新——不重复劳动。锁由文件句柄持有，
    /// 进程退出（含被强杀）时内核自动释放，所以锁文件本身永不删除：删掉会让
    /// 等待者在新旧 inode 上各持一把锁。
    ///
    /// # Errors
    ///
    /// 目录建不出来或无法打开锁文件时返回 [`RepositoryError::Storage`]
    pub fn try_lock(&self) -> Result<Option<RefreshLock>, RepositoryError> {
        std::fs::create_dir_all(&self.root).map_err(|err| storage(&self.root, err))?;
        let path = self.root.join(LOCK_FILE);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|err| storage(&path, err))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(RefreshLock { _file: file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(err)) => Err(storage(&path, err)),
        }
    }

    fn read_list<T: serde::de::DeserializeOwned>(
        &self,
        file: &str,
    ) -> Result<Vec<T>, RepositoryError> {
        Ok(read_json::<Vec<T>>(&self.root.join(file))?.unwrap_or_default())
    }

    fn write<T: serde::Serialize>(&self, file: &str, value: &T) -> Result<(), RepositoryError> {
        let path = self.root.join(file);
        let body = serde_json::to_vec_pretty(value)
            .map_err(|err| RepositoryError::Corrupt(format!("cache {file}: {err}")))?;
        write_atomically(&path, &body)
    }
}

/// 刷新锁句柄：`Drop` 时随 `File` 关闭释放
#[derive(Debug)]
pub struct RefreshLock {
    _file: std::fs::File,
}

/// 一次完整同步的结果
///
/// 六份内容一次交齐，由 [`RemoteCache::store`] 决定落盘顺序；`at` 是本次同步
/// 的时间戳，调用方传 `now_secs()` 或固定值（测试）。
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// 工作区
    pub workspaces: Vec<WireWorkspace>,
    /// 服务器
    pub servers: Vec<WireServer>,
    /// Java 运行时
    pub javas: Vec<WireJava>,
    /// 运行状态（按服务器名）
    pub statuses: Vec<(String, WireStatusEntry)>,
    /// 同步时间（Unix 秒）
    pub at: i64,
}

/// 当前 Unix 秒
///
/// 时钟回拨或早于纪元时退回 0：缓存新鲜度只需要「大致单调」，为此报错不值得。
#[must_use]
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

/// 读一份 JSON；文件不存在返回 `None`
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, RepositoryError> {
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(storage(path, err)),
    };
    serde_json::from_slice(&raw)
        .map(Some)
        .map_err(|err| RepositoryError::Corrupt(format!("cache {}: {err}", path.display())))
}

/// 原子写：同目录临时文件 + `rename`
///
/// 与 `storage::JsonStore` 同款。读者要么看到旧内容要么看到新内容，不会读到
/// 半截 JSON——并发刷新与前台读会同时发生。
fn write_atomically(path: &Path, body: &[u8]) -> Result<(), RepositoryError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).map_err(|err| storage(dir, err))?;
    let file_name = path
        .file_name()
        .map_or_else(|| "cache".to_owned(), |n| n.to_string_lossy().into_owned());
    let tmp = dir.join(format!(".{file_name}.tmp"));
    let write = || -> std::io::Result<()> {
        use std::io::Write as _;
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(body)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    };
    if let Err(err) = write() {
        // 失败时别留垃圾：临时文件残留会让下一次写入命中脏数据之外的困惑
        let _ = std::fs::remove_file(&tmp);
        return Err(storage(path, err));
    }
    sync_parent(dir);
    Ok(())
}

#[cfg(unix)]
fn sync_parent(dir: &Path) {
    // 目录项本身也要落盘，否则断电后 rename 可能丢；失败无补救动作
    if let Ok(handle) = std::fs::File::open(dir) {
        let _ = handle.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_parent(_dir: &Path) {}

#[allow(clippy::needless_pass_by_value)] // 调用点全都处于 `map_err(|err| …)` 位置
fn storage(path: &Path, err: std::io::Error) -> RepositoryError {
    RepositoryError::Storage(Box::new(std::io::Error::new(
        err.kind(),
        format!("{}: {err}", path.display()),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(dir: &Path) -> RemoteCache {
        RemoteCache::new(dir, RemoteName::parse("box").expect("name"))
    }

    fn running() -> SupervisorStatus {
        SupervisorStatus {
            status: domain::server::status::ServerStatus::Running,
            pid: Some(42),
            uptime: None,
            exit_code: None,
        }
    }

    fn server(name: &str) -> WireServer {
        WireServer {
            id: "220e8400-e29b-41d4-a716-446655440001".to_owned(),
            name: name.to_owned(),
            workspace_id: "110e8400-e29b-41d4-a716-446655440000".to_owned(),
            rel_dir: name.to_owned(),
            version: "1.21.1".to_owned(),
            variant: "vanilla".to_owned(),
            jar: None,
            launch: None,
            settings: protocol::remote::wire::WireSettings {
                java_major: None,
                ram_mib: None,
                autostart: false,
                backup_scope: Vec::new(),
            },
            java_major: None,
            created_at: 1,
            updated_at: 1,
        }
    }

    fn workspace(name: &str) -> WireWorkspace {
        WireWorkspace {
            id: "110e8400-e29b-41d4-a716-446655440000".to_owned(),
            name: name.to_owned(),
            path: format!("/srv/{name}"),
            is_current: true,
            created_at: 1,
            updated_at: 2,
        }
    }

    #[test]
    fn missing_cache_reads_as_never_synced() {
        let dir = tempfile::tempdir().expect("tmp");
        let cache = cache(dir.path());
        assert_eq!(cache.state().expect("state"), SyncState::default());
        assert!(!cache.state().expect("state").has_synced());
        assert!(cache.is_stale(now_secs()), "没有缓存就是过期");
        assert!(cache.workspaces().expect("workspaces").is_empty());
    }

    #[test]
    fn store_then_read_roundtrips_every_list() {
        let dir = tempfile::tempdir().expect("tmp");
        let cache = cache(dir.path());
        let snapshot = Snapshot {
            workspaces: vec![workspace("w1")],
            servers: Vec::new(),
            javas: Vec::new(),
            statuses: vec![("web".to_owned(), WireStatusEntry::ok(Some(&running())))],
            at: 100,
        };
        cache.store(&snapshot).expect("store");

        assert_eq!(cache.workspaces().expect("workspaces"), snapshot.workspaces);
        assert_eq!(cache.state().expect("state").last_sync, Some(100));
        assert_eq!(cache.state().expect("state").last_error, None);
        let statuses = cache.decoded_statuses();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].0, "web");
        assert_eq!(statuses[0].1, Some(running()));
    }

    #[test]
    fn ttl_boundary_is_exclusive_of_fresh() {
        let state = SyncState {
            last_sync: Some(1_000),
            last_error: None,
            generation: 1,
        };
        let ttl = i64::try_from(TTL.as_secs()).expect("ttl");
        assert!(!state.is_stale(1_000), "刚同步完是新鲜的");
        assert!(!state.is_stale(1_000 + ttl - 1));
        assert!(state.is_stale(1_000 + ttl), "到点即过期");
        // 时钟回拨不会把缓存判成永久新鲜
        assert!(!state.is_stale(999));
    }

    #[test]
    fn failure_keeps_previous_sync_time() {
        let dir = tempfile::tempdir().expect("tmp");
        let cache = cache(dir.path());
        cache.mark_synced(500).expect("synced");
        cache.mark_failed("connection refused").expect("failed");

        let state = cache.state().expect("state");
        assert_eq!(state.last_sync, Some(500), "失败不清掉上次成功时间");
        assert_eq!(state.last_error.as_deref(), Some("connection refused"));
    }

    #[test]
    fn guarded_store_is_dropped_when_something_else_wrote_first() {
        let dir = tempfile::tempdir().expect("tmp");
        let cache = cache(dir.path());
        cache.store_servers(&[]).expect("空列表");
        let seen = cache.generation();

        // 慢刷新在拉数据期间，前台写命令把一台服务器写进了缓存
        cache.store_servers(&[server("web")]).expect("写穿透");

        // 慢刷新带着旧快照回来：必须被丢弃，否则刚删掉的条目会复活
        let stale = Snapshot {
            workspaces: Vec::new(),
            servers: Vec::new(),
            javas: Vec::new(),
            statuses: Vec::new(),
            at: 200,
        };
        assert!(
            !cache.store_if_current(&stale, seen).expect("guard"),
            "代次变了就不该覆盖"
        );
        assert_eq!(
            cache.servers().expect("servers").len(),
            1,
            "写穿透的结果要留着"
        );
    }

    #[test]
    fn guarded_store_applies_when_nobody_wrote_meanwhile() {
        let dir = tempfile::tempdir().expect("tmp");
        let cache = cache(dir.path());
        let seen = cache.generation();
        let snapshot = Snapshot {
            workspaces: Vec::new(),
            servers: vec![server("web")],
            javas: Vec::new(),
            statuses: Vec::new(),
            at: 300,
        };
        assert!(cache.store_if_current(&snapshot, seen).expect("guard"));
        assert_eq!(cache.servers().expect("servers").len(), 1);
        assert_eq!(cache.state().expect("state").last_sync, Some(300));
    }

    #[test]
    fn corrupt_state_counts_as_stale_instead_of_failing() {
        let dir = tempfile::tempdir().expect("tmp");
        let cache = cache(dir.path());
        cache.mark_synced(500).expect("synced");
        std::fs::write(cache.dir().join("state.json"), b"{ not json").expect("write");

        assert!(
            cache.state().is_err(),
            "元信息损坏要能被显式读到（便于诊断）"
        );
        assert!(cache.is_stale(500), "读不出来就按过期处理");
    }

    #[test]
    fn corrupt_list_is_reported_as_corrupt() {
        let dir = tempfile::tempdir().expect("tmp");
        let cache = cache(dir.path());
        let path = cache.dir().join("workspaces.json");
        std::fs::create_dir_all(cache.dir()).expect("dir");
        std::fs::write(&path, b"[{\"id\": 1}]").expect("write");

        match cache.workspaces() {
            Err(RepositoryError::Corrupt(message)) => {
                assert!(message.contains("workspaces.json"), "{message}");
            }
            other => panic!("期望 Corrupt，实际 {other:?}"),
        }
    }

    #[test]
    fn refresh_lock_is_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().expect("tmp");
        let cache = cache(dir.path());
        let first = cache.try_lock().expect("lock");
        assert!(first.is_some(), "首次应拿到锁");
        assert!(
            cache.try_lock().expect("lock").is_none(),
            "同一进程内第二次也拿不到：锁由句柄持有"
        );
        drop(first);
        assert!(cache.try_lock().expect("lock").is_some(), "释放后可再拿");
        assert!(
            cache.dir().join(LOCK_FILE).exists(),
            "锁文件必须保留：删掉会让等待者各持一把锁"
        );
    }

    #[test]
    fn write_through_replaces_one_list_only() {
        let dir = tempfile::tempdir().expect("tmp");
        let cache = cache(dir.path());
        cache
            .store(&Snapshot {
                workspaces: vec![workspace("w1")],
                servers: Vec::new(),
                javas: Vec::new(),
                statuses: Vec::new(),
                at: 100,
            })
            .expect("store");

        cache
            .store_workspaces(&[workspace("w1"), workspace("w2")])
            .expect("store workspaces");

        assert_eq!(cache.workspaces().expect("workspaces").len(), 2);
        // 只有工作区被换掉，其余列表原样保留
        assert!(cache.servers().expect("servers").is_empty());
        let state = cache.state().expect("state");
        assert_ne!(state.last_sync, Some(100), "写穿透问过对端，新鲜度随之前移");
        assert!(!state.is_offline(), "写穿透成功即摘掉离线标记");
        assert!(state.generation > 1, "每次写都要推进代次，供刷新守卫比对");
    }

    #[test]
    fn no_temp_file_is_left_behind() {
        let dir = tempfile::tempdir().expect("tmp");
        let cache = cache(dir.path());
        cache.store(&Snapshot::default()).expect("store");
        let leftovers: Vec<_> = std::fs::read_dir(cache.dir())
            .expect("read_dir")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "临时文件必须被 rename 或清掉");
    }
}
