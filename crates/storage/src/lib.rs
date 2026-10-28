//! 元数据存储：一条记录一个 JSON 文件
//!
//! 目录布局（`{root}` = `{data_dir}/mcsup/db`）：
//!
//! ```text
//! {root}/workspaces/state.json      {"version":2,"current":{"origin":"local","workspace":"<uuid>"}|null}
//!                                   v1 `{"current":"<uuid>"}` 读作本地选择
//! {root}/workspaces/<uuid>.json     一条工作目录
//! {root}/servers/<uuid>.json        一台服务器
//! {root}/java/state.json            {"version":1,"scanned_at":<unix 秒>|null}
//! {root}/java/<id>.json             一个 Java 运行时
//! ```
//!
//! 设计要点：
//!
//! - **故障隔离**：一条记录一个文件，坏一份只影响那一条（[`RepositoryError::Corrupt`]
//!   指向具体文件名），其余照常可读写。
//! - **原子写入**：同目录临时文件 → `fsync` → `rename` 覆盖。进程中断只会留下
//!   「替换前」或「替换后」的完整文件，不会出现半截 JSON。
//! - **当前工作区只有一处真相**：`state.json` 的 `current` 指针。记录文件里不再
//!   存 `is_current`，避免「n 个布尔表示单值」带来的不一致。
//! - **唯一性**：文件系统不提供唯一索引，写入前扫目录比对（名称 / 路径 /
//!   同工作区 `rel_dir`），代价与全量解析同量级。
//! - **文件名即身份**：文件名（不含 `.json`）必须等于记录 `id`，不一致按损坏拒绝，
//!   防止文件被手工改名后出现「两条同 id」。
//!
//! 并发边界：进程内 `Mutex` 串行，跨进程用数据根目录的 `.lock` 文件互斥；
//! 跨机器共享同一目录不在契约内。

mod models;

use crate::models::{JavaRuntimeEntry, ServerEntry, WorkspaceEntry, corrupt};
use domain::java::JavaRuntime;
use domain::server::meta::{ServerMeta, ServerName};
use domain::server::version::VersionId;
use domain::workspace::{Workspace, WorkspaceId, WorkspaceName};
use port::{
    JavaRuntimeRepository, LOCAL_NAME, Origin, RepositoryError, ServerRepository,
    WorkspaceRepository, WorkspaceSelection,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// `state.json` 的格式版本
///
/// v2 把裸 uuid 换成 `{origin, workspace}`：当前工作区可以指向远端。
const WORKSPACE_STATE_VERSION: u32 = 2;
/// Java 集合的 `state.json` 仍是 v1（只有扫描时间）
const JAVA_STATE_VERSION: u32 = 1;
/// 记录文件扩展名
const RECORD_EXT: &str = "json";
/// 当前工作区指针文件名
const STATE_FILE: &str = "state.json";
/// 跨进程锁文件名（位于数据根目录）
const LOCK_FILE: &str = ".lock";

/// 工作目录集合的全局状态
///
/// 只放「全局唯一」的东西：当前是哪个工作目录。其余字段都在各自记录里。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct WorkspaceState {
    version: u32,
    /// 本机视角的当前工作区。指向已删除 / 损坏 / 不可达来源时读取视为「无当前」。
    #[serde(default)]
    current: Option<CurrentPointer>,
}

/// `current` 的线上形态：兼容 v1 的裸 uuid
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum CurrentPointer {
    /// v1：裸 uuid，等价于 `origin = local`
    Bare(String),
    /// v2：带来源
    Scoped {
        /// 来源（`local` 或远端名）
        origin: String,
        /// 工作区 id
        workspace: String,
    },
}

/// Java 集合的全局状态
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct JavaState {
    version: u32,
    /// 上次扫描时间（unix 秒）
    scanned_at: Option<i64>,
}

impl WorkspaceState {
    fn new() -> Self {
        Self {
            version: WORKSPACE_STATE_VERSION,
            current: None,
        }
    }

    /// 解析为本机视角的选择；非法 / 损坏视为「无当前」
    fn selection(&self) -> Option<WorkspaceSelection> {
        let (origin, workspace) = match self.current.as_ref()? {
            CurrentPointer::Bare(id) => (Origin::Local, id.clone()),
            CurrentPointer::Scoped { origin, workspace } => {
                (Origin::parse(origin).ok()?, workspace.clone())
            }
        };
        Some(WorkspaceSelection {
            origin,
            workspace: WorkspaceId::parse(&workspace).ok()?,
        })
    }

    /// 本地 current 指针：只有 origin 为本地时才参与本地记录的 `is_current`
    fn local_id(&self) -> Option<String> {
        self.selection()
            .filter(|selection| selection.origin.is_local())
            .map(|selection| selection.workspace.to_string())
    }
}

impl JavaState {
    fn new() -> Self {
        Self {
            version: JAVA_STATE_VERSION,
            scanned_at: None,
        }
    }
}

/// 一条扫描结果：文件路径 + 解析成功或失败
type RawRecord<T> = (PathBuf, Result<T, RepositoryError>);

/// 一条记录的一个集合（目录 + 文件名规则 + 状态文件）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Collection {
    Workspaces,
    Servers,
    Java,
}

impl Collection {
    /// 子目录名
    fn dir(self) -> &'static str {
        match self {
            Self::Workspaces => "workspaces",
            Self::Servers => "servers",
            Self::Java => "java",
        }
    }
}

/// 进程内锁 + 跨进程锁的持有者
struct Guard<'a> {
    /// 保持 `MutexGuard` 存活；不做额外访问
    _thread: MutexGuard<'a, ()>,
    _dir: DirLock,
}

/// 多文件 JSON 实现的元数据仓库
///
/// 三个仓库（工作目录 / 服务器 / Java）共用同一个实例，因此共用一把锁：
/// 写入涉及跨集合校验（服务器引用工作目录），进程内必须串行。
pub struct JsonStore {
    root: PathBuf,
    lock: Mutex<()>,
    /// `open_in_memory` 的临时目录：随 store 存活，drop 时清理
    dir: Option<tempfile::TempDir>,
}

impl std::fmt::Debug for JsonStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsonStore")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl JsonStore {
    /// 打开（必要时创建）存放三个集合的数据目录
    ///
    /// 打开时只建目录、不校验内容：损坏的单条记录不应阻止其他命令运行，
    /// 只在真正读到它时报错。
    ///
    /// # Errors
    ///
    /// 目录创建失败时返回 [`RepositoryError::Storage`]
    pub fn open(root: &Path) -> Result<Self, RepositoryError> {
        let store = Self {
            root: root.to_path_buf(),
            lock: Mutex::new(()),
            dir: None,
        };
        for collection in [
            Collection::Workspaces,
            Collection::Servers,
            Collection::Java,
        ] {
            std::fs::create_dir_all(store.collection_dir(collection)).map_err(storage_err)?;
        }
        Ok(store)
    }

    /// 临时目录中的空仓库：测试与临时查询用
    ///
    /// 临时目录随返回的 store 一起保活，drop 时清理。
    ///
    /// # Errors
    ///
    /// 临时目录创建或子目录创建失败时返回 [`RepositoryError::Storage`]
    pub fn open_in_memory() -> Result<Self, RepositoryError> {
        let dir = tempfile::TempDir::new().map_err(storage_err)?;
        let mut store = Self::open(dir.path())?;
        store.dir = Some(dir);
        Ok(store)
    }

    /// 数据根目录（三个集合的父目录）
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn collection_dir(&self, collection: Collection) -> PathBuf {
        self.root.join(collection.dir())
    }

    /// 记录文件路径；`id` 必须是单层普通文件名片段
    ///
    /// `id` 进路径前必须校验：它既来自目录扫描的文件名（见
    /// [`Self::record_id_of`]），也来自记录内容。含分隔符 / `..` / 盘符的 id
    /// 会把写入引到集合目录之外，或让不同 id 映射到同一文件。
    /// 这类 id 视为损坏而不是拼接后放行。
    ///
    /// 规则不是白名单：`server link` 就地注册与旧版数据都可能带来带点的 id，
    /// 只要求「单层、非 `.` / `..`」，保证 `collection_dir()/id.json` 不越界。
    fn record_path(&self, collection: Collection, id: &str) -> Result<PathBuf, RepositoryError> {
        if !is_safe_record_id(id) {
            return Err(RepositoryError::Corrupt(format!(
                "{}: unsafe record id {id}",
                collection.dir()
            )));
        }
        Ok(self
            .collection_dir(collection)
            .join(format!("{id}.{RECORD_EXT}")))
    }

    fn state_path(&self, collection: Collection) -> PathBuf {
        self.collection_dir(collection).join(STATE_FILE)
    }

    /// 记录文件 id（文件名去掉 `.json`）；`state.json` 不是记录
    ///
    /// 与 [`Self::record_path`] 同一口径：`<id>.json` 里 `<id>` 必须是单层名，
    /// 否则扫目录会得到空 id 或跨层路径，拼回去指向别的文件。
    fn record_id_of(path: &Path) -> Option<String> {
        // Windows 上 `file_stem` 与 `extension` 会把 `x.` 这类尾点名识别成不同
        // 扩展名；先按字符串口径取 `<id>.json` 再校验 id，避免平台分叉
        let name = path.file_name()?.to_str()?;
        if name == STATE_FILE {
            return None;
        }
        let id = name.strip_suffix(".json")?;
        if !is_safe_record_id(id) {
            return None;
        }
        Some(id.to_owned())
    }

    /// 进程内排队 + 跨进程锁
    ///
    /// 返回的 guard 离开作用域时同时释放两把锁。
    fn lock(&self) -> Result<Guard<'_>, RepositoryError> {
        let guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let dir_lock = DirLock::acquire(self.root.join(LOCK_FILE))?;
        Ok(Guard {
            _thread: guard,
            _dir: dir_lock,
        })
    }

    // ---------------------------------------------------------------- 记录读写

    /// 读一条记录；文件不存在返回 `None`
    ///
    /// 文件名与记录 `id` 必须一致，`schema` 必须是本版本，否则按损坏拒绝。
    fn load_record<T>(&self, collection: Collection, id: &str) -> Result<Option<T>, RepositoryError>
    where
        T: DeserializeOwned + RecordId,
    {
        let path = self.record_path(collection, id)?;
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(storage_err(err)),
        };
        let record: T = serde_json::from_slice(&bytes)
            .map_err(|err| RepositoryError::Corrupt(format!("{}: {err}", self.display(&path))))?;
        if record.schema() != models::RECORD_VERSION {
            return Err(RepositoryError::Corrupt(format!(
                "{}: schema {}, expected {}",
                self.display(&path),
                record.schema(),
                models::RECORD_VERSION
            )));
        }
        if record.record_id() != id {
            return Err(RepositoryError::Corrupt(format!(
                "{}: holds id {}",
                self.display(&path),
                record.record_id()
            )));
        }
        Ok(Some(record))
    }

    /// 扫描整个集合
    ///
    /// `tolerate` 为 `true` 时跳过损坏文件（结果里带出它们的路径），
    /// 为 `false` 时第一个坏文件即失败。`list` 用前者，唯一性校验用后者：
    /// 校验时看不见的记录可能正好与待写记录冲突，不能默默放过。
    fn scan<T>(
        &self,
        collection: Collection,
        tolerate: bool,
    ) -> Result<(Vec<T>, Vec<String>), RepositoryError>
    where
        T: DeserializeOwned + RecordId,
    {
        let dir = self.collection_dir(collection);
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok((Vec::new(), Vec::new()));
            }
            Err(err) => return Err(storage_err(err)),
        };
        let mut records = Vec::new();
        let mut broken = Vec::new();
        for entry in entries {
            let entry = entry.map_err(storage_err)?;
            let path = entry.path();
            // 文件名规则与 record_path 同口径：不是 `<安全 id>.json` 的一律不是记录
            let Some(id) = Self::record_id_of(&path) else {
                continue;
            };
            if !path.is_file() {
                continue;
            }
            match self.load_record::<T>(collection, &id) {
                Ok(Some(record)) => records.push(record),
                // 扫描与读取之间被删除：跳过
                Ok(None) => {}
                Err(err @ RepositoryError::Corrupt(_)) => {
                    if tolerate {
                        broken.push(self.display(&path));
                    } else {
                        return Err(err);
                    }
                }
                Err(err) => return Err(err),
            }
        }
        Ok((records, broken))
    }

    /// 扫描并要求全部可读（唯一性校验等写入前置检查用）
    fn scan_strict<T>(&self, collection: Collection) -> Result<Vec<T>, RepositoryError>
    where
        T: DeserializeOwned + RecordId,
    {
        Ok(self.scan(collection, false)?.0)
    }

    /// 扫描并把每条结果与「解析错误」成对返回，供需要区分「删掉哪个」的调用方使用
    fn scan_raw<T>(&self, collection: Collection) -> Result<Vec<RawRecord<T>>, RepositoryError>
    where
        T: DeserializeOwned + RecordId,
    {
        let dir = self.collection_dir(collection);
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(storage_err(err)),
        };
        let mut out = Vec::new();
        for entry in entries {
            let path = entry.map_err(storage_err)?.path();
            let Some(id) = Self::record_id_of(&path) else {
                continue;
            };
            if !path.is_file() {
                continue;
            }
            match self.load_record::<T>(collection, &id) {
                Ok(Some(record)) => out.push((path, Ok(record))),
                Ok(None) => {}
                Err(err) => out.push((path, Err(err))),
            }
        }
        Ok(out)
    }

    /// 原子写入一条记录
    fn write_record(
        &self,
        collection: Collection,
        id: &str,
        record: &impl Serialize,
    ) -> Result<(), RepositoryError> {
        write_atomically(&self.record_path(collection, id)?, record)
    }

    fn remove_record(&self, collection: Collection, id: &str) -> Result<(), RepositoryError> {
        remove_file_checked(&self.record_path(collection, id)?)
    }

    // ---------------------------------------------------------------- 状态读写

    fn load_workspace_state(&self) -> Result<WorkspaceState, RepositoryError> {
        let path = self.state_path(Collection::Workspaces);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(WorkspaceState::new()),
            Err(err) => return Err(storage_err(err)),
        };
        let state: WorkspaceState = serde_json::from_slice(&bytes)
            .map_err(|err| RepositoryError::Corrupt(format!("{}: {err}", self.display(&path))))?;
        // 兼容 v1（裸 uuid）与 v2；其余版本视为损坏
        if !(1..=WORKSPACE_STATE_VERSION).contains(&state.version) {
            return Err(RepositoryError::Corrupt(format!(
                "{}: version {}, expected 1..={WORKSPACE_STATE_VERSION}",
                self.display(&path),
                state.version
            )));
        }
        Ok(state)
    }

    /// 读本机视角的当前工作区，读不出时退化为「无当前」
    ///
    /// `state.json` 是全局单点：解析失败或版本未知时若直接报错，
    /// `workspace list` / `server list` 会整体不可用。而这两个命令
    /// 并不需要「当前」才能给出有用结果，因此这里只降级不失败，
    /// 与悬空指针同一语义。写入路径仍走 [`Self::load_workspace_state`]：
    /// 写之前必须知道状态是否真的可改，不能静默覆盖一份读不懂的文件。
    fn selection_pointer(&self) -> Option<WorkspaceSelection> {
        self.load_workspace_state()
            .ok()
            .and_then(|state| state.selection())
    }

    /// 本地 current 指针；只有选择本身指向本地时才返回
    fn local_current_id(&self) -> Option<String> {
        self.load_workspace_state()
            .ok()
            .and_then(|state| state.local_id())
    }

    fn load_java_state(&self) -> Result<JavaState, RepositoryError> {
        self.load_state(Collection::Java, JavaState::new)
    }

    fn load_state<T>(&self, collection: Collection, empty: fn() -> T) -> Result<T, RepositoryError>
    where
        T: DeserializeOwned + StateVersion,
    {
        let path = self.state_path(collection);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(empty()),
            Err(err) => return Err(storage_err(err)),
        };
        let state: T = serde_json::from_slice(&bytes)
            .map_err(|err| RepositoryError::Corrupt(format!("{}: {err}", self.display(&path))))?;
        if state.state_version() != JAVA_STATE_VERSION {
            return Err(RepositoryError::Corrupt(format!(
                "{}: version {}, expected {JAVA_STATE_VERSION}",
                self.display(&path),
                state.state_version()
            )));
        }
        Ok(state)
    }

    fn write_state<T: Serialize>(
        &self,
        collection: Collection,
        state: &T,
    ) -> Result<(), RepositoryError> {
        write_atomically(&self.state_path(collection), state)
    }

    /// 相对根目录显示路径，错误信息不洩露完整用户路径
    fn display(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }
}

/// 把记录转成领域对象，并按 `state.json` 的指针填充 `is_current`
///
/// 单独构造而不是复用 `TryFrom`：记录文件里没有当前标记，
/// 直接构造可避免出现一个 `is_current = false` 的中间态。
fn into_workspace(
    entry: WorkspaceEntry,
    current: Option<&str>,
) -> Result<Workspace, RepositoryError> {
    let is_current = current == Some(entry.id.as_str());
    let WorkspaceEntry {
        id,
        name,
        path,
        created_at,
        updated_at,
        ..
    } = entry;
    Ok(Workspace {
        id: WorkspaceId::parse(&id).map_err(|err| corrupt("workspace id", &id, err))?,
        name: WorkspaceName::parse(&name).map_err(|err| corrupt("workspace name", &name, err))?,
        path: PathBuf::from(path),
        is_current,
        created_at,
        updated_at,
    })
}

/// 记录文件能自报身份与格式版本
trait RecordId {
    fn record_id(&self) -> &str;
    /// 格式版本字段：工作目录 / 服务器是 `version`，Java 是 `schema`
    fn schema(&self) -> u32;
}

impl RecordId for WorkspaceEntry {
    fn record_id(&self) -> &str {
        &self.id
    }
    fn schema(&self) -> u32 {
        self.version
    }
}

impl RecordId for ServerEntry {
    fn record_id(&self) -> &str {
        &self.id
    }
    fn schema(&self) -> u32 {
        self.schema
    }
}

impl RecordId for JavaRuntimeEntry {
    fn record_id(&self) -> &str {
        &self.id
    }
    fn schema(&self) -> u32 {
        self.schema
    }
}

/// `state.json` 能自报格式版本
trait StateVersion {
    fn state_version(&self) -> u32;
}

impl StateVersion for WorkspaceState {
    fn state_version(&self) -> u32 {
        self.version
    }
}

impl StateVersion for JavaState {
    fn state_version(&self) -> u32 {
        self.version
    }
}

/// 序列化 → 写同目录临时文件 → 刷盘 → rename 覆盖
///
/// 临时文件与目标同目录，保证 rename 是同一文件系统上的原子替换；
/// Windows 的 `rename` 覆盖已存在目标，无需先删除（先删会留下无文件窗口）。
/// 失败时清掉临时文件并抛出错误，目标文件保持原样。
fn write_atomically(path: &Path, value: &impl Serialize) -> Result<(), RepositoryError> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(storage_err)?;
    bytes.push(b'\n');
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(storage_err)?;
    }
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{file_name}.tmp"));
    let result = (|| -> io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if let Err(err) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(storage_err(err));
    }
    // 目录项也要落盘，否则断电后可能「文件在、名字不在」
    sync_parent(path);
    Ok(())
}

#[cfg(unix)]
fn sync_parent(path: &Path) {
    if let Some(parent) = path.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
}

/// Windows 无法对目录 fsync，rename 已由 NTFS 日志保证
#[cfg(not(unix))]
fn sync_parent(_path: &Path) {}

/// 记录 id 是否可安全地作为文件名使用
///
/// 只排除「单层文件名的越界形态」：空、`.` / `..`、含分隔符、含 NUL。
/// 不要求字母表——`server link` 就地注册的服务器 id 取自目录名，磁盘上本来
/// 就可能带点（`my.server`），白名单会在扫目录时直接丢掉整条记录。
///
/// 安全前提是**单层**：`collection_dir()/id.json` 中 `id` 不含 `/`、`\` 或就是
/// `..` 时不可能离开集合目录；`state.json` 由调用侧单独排除（见
/// [`JsonStore::record_id_of`]）。
fn is_safe_record_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id != "." && id != ".." && !id.contains(['/', '\\', '\0'])
}

fn storage_err(err: impl std::error::Error + Send + Sync + 'static) -> RepositoryError {
    RepositoryError::Storage(Box::new(err))
}

/// 删除文件；已不存在视为成功（幂等）
fn remove_file_checked(path: &Path) -> Result<(), RepositoryError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(storage_err(err)),
    }
}

/// 目录级互斥：进程内 `Mutex` 之外再加一把跨进程锁
///
/// 进程内锁挡不住两个 mcsup 进程同时写同一目录。唯一性靠「先扫后写」，
/// 没有这把锁时两个进程会一起通过校验、各自落盘，产生重名或重复 id。
///
/// 实现：`try_lock` 自旋 + 超时，不用 `flock`（Windows 语义不同），
/// 锁文件内容写持有者 pid 作为存活凭据。锁文件本身永不删除——删锁文件会
/// 引入「A 删了锁、B 也创建了锁」的竞态。
struct DirLock {
    path: PathBuf,
}

impl DirLock {
    fn acquire(path: PathBuf) -> Result<Self, RepositoryError> {
        // 进程内已排队，超时只可能是别的进程持锁或残留锁
        let deadline = std::time::Instant::now() + LOCK_TIMEOUT;
        let mut delay = std::time::Duration::from_millis(2);
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    // pid 是回收残留锁的唯一凭据，写失败就无法判断存活，
                    // 宁可失败也不留下一个无法回收的锁
                    write!(file, "{}", std::process::id())
                        .map_err(|err| storage_err(io::Error::new(err.kind(), err.to_string())))?;
                    return Ok(Self { path });
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    if is_stale(&path) {
                        // 持锁进程已死：清掉后重试。删除失败说明别的进程刚抢到，继续等
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    if std::time::Instant::now() >= deadline {
                        return Err(RepositoryError::Storage(Box::new(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            format!(
                                "storage lock busy: waited {LOCK_TIMEOUT:?}, another mcsup process holds it"
                            ),
                        ))));
                    }
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(std::time::Duration::from_millis(50));
                }
                Err(err) => return Err(storage_err(err)),
            }
        }
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// 单次拿到锁的最长等待时间
const LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// 锁文件是否是可回收的残留
///
/// 判据是**持有者进程是否还活着**，不是锁文件有多旧：
/// 旧的时间阈值只会误判——正常持锁超过阈值的进程会被当成崩溃，
/// 锁被别的进程删除后两者同处临界区。
fn is_stale(path: &Path) -> bool {
    match std::fs::read_to_string(path) {
        // 读得到 pid：活着就不是残留
        Ok(text) => match text.trim().parse::<u32>() {
            Ok(pid) => !process_alive(pid),
            // pid 写了一半（写入瞬间崩溃）或为垃圾：判为残留
            Err(_) => true,
        },
        // 文件刚被别的进程释放：不算残留，让调用方下一轮重试创建
        Err(_) => false,
    }
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // Unix 上 pid 会被复用；信号 0 只探测存在性，已足够区分「崩溃遗留」
    // 与「仍在运行」两种常见情况
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Windows：查询打开该 pid 的进程句柄
///
/// pid 已退出时 `OpenProcess` 失败并置 `ERROR_INVALID_PARAMETER`。
/// 不引入 Win32 绑定依赖，改用系统自带的任务列表查询，避免为一次
/// 诊断性判断扩大依赖面。
#[cfg(not(unix))]
fn process_alive(pid: u32) -> bool {
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .is_ok_and(|out| {
            out.status.success() && String::from_utf8_lossy(&out.stdout).contains(&pid.to_string())
        })
}

fn conflict_unique(exists: bool, key: &str) -> Result<(), RepositoryError> {
    if exists {
        Err(RepositoryError::AlreadyExists(key.to_owned()))
    } else {
        Ok(())
    }
}

// -------------------------------------------------------------------- 工作目录

impl WorkspaceRepository for JsonStore {
    fn create(&self, ws: &Workspace) -> Result<(), RepositoryError> {
        let _guard = self.lock()?;
        let entry = WorkspaceEntry::from(ws);
        // 唯一性必须在写入前确认：坏记录看不见就会放过冲突
        let existing = self.scan_strict::<WorkspaceEntry>(Collection::Workspaces)?;
        for other in &existing {
            if other.id == entry.id {
                return Err(RepositoryError::AlreadyExists(ws.name.as_str().to_owned()));
            }
            conflict_unique(other.name == entry.name, ws.name.as_str())?;
            conflict_unique(other.path == entry.path, ws.path.to_string_lossy().as_ref())?;
        }
        self.write_record(Collection::Workspaces, &entry.id, &entry)?;
        // 标记为当前时同步提升指针；`state.json` 单独一步，
        // 崩溃只会留下「已登记但不是当前」，不会留下两个当前
        if ws.is_current {
            let mut state = self.load_workspace_state()?;
            state.current = Some(CurrentPointer::Scoped {
                origin: LOCAL_NAME.to_owned(),
                workspace: ws.id.to_string(),
            });
            self.write_state(Collection::Workspaces, &state)?;
        }
        Ok(())
    }

    fn get(&self, id: &WorkspaceId) -> Result<Workspace, RepositoryError> {
        let _guard = self.lock()?;
        let entry = self
            .load_record::<WorkspaceEntry>(Collection::Workspaces, id.as_str())?
            .ok_or_else(|| RepositoryError::NotFound(id.as_str().to_owned()))?;
        into_workspace(entry, self.local_current_id().as_deref())
    }

    fn get_by_name(&self, name: &WorkspaceName) -> Result<Workspace, RepositoryError> {
        let _guard = self.lock()?;
        let (records, _) = self.scan::<WorkspaceEntry>(Collection::Workspaces, true)?;
        let entry = records
            .into_iter()
            .find(|entry| entry.name == name.as_str())
            .ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))?;
        into_workspace(entry, self.local_current_id().as_deref())
    }

    fn list(&self) -> Result<Vec<Workspace>, RepositoryError> {
        let _guard = self.lock()?;
        // 单条损坏不应让整个列表不可用：跳过它，其余照常返回
        let (records, _) = self.scan::<WorkspaceEntry>(Collection::Workspaces, true)?;
        let current = self.local_current_id();
        let mut rows = records
            .into_iter()
            .map(|entry| into_workspace(entry, current.as_deref()))
            .collect::<Result<Vec<_>, _>>()?;
        rows.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        Ok(rows)
    }

    fn current(&self) -> Result<Option<Workspace>, RepositoryError> {
        let _guard = self.lock()?;
        let Some(id) = self.local_current_id() else {
            return Ok(None);
        };
        // 指针悬空（记录已删 / 损坏）按「无当前」处理，不改文件：
        // 悬空是安全失败，下次 set_current 自然覆盖
        let Some(entry) = self
            .load_record::<WorkspaceEntry>(Collection::Workspaces, &id)
            .ok()
            .flatten()
        else {
            return Ok(None);
        };
        into_workspace(entry, Some(&id)).map(Some)
    }

    fn current_selection(&self) -> Result<Option<WorkspaceSelection>, RepositoryError> {
        Ok(self.selection_pointer())
    }

    fn set_current(&self, id: &WorkspaceId) -> Result<(), RepositoryError> {
        let _guard = self.lock()?;
        if self
            .load_record::<WorkspaceEntry>(Collection::Workspaces, id.as_str())?
            .is_none()
        {
            return Err(RepositoryError::NotFound(id.as_str().to_owned()));
        }
        let mut state = self.load_workspace_state()?;
        state.current = Some(CurrentPointer::Scoped {
            origin: LOCAL_NAME.to_owned(),
            workspace: id.to_string(),
        });
        self.write_state(Collection::Workspaces, &state)
    }

    fn set_current_selection(&self, selection: &WorkspaceSelection) -> Result<(), RepositoryError> {
        let _guard = self.lock()?;
        // 本地选择必须指向真实记录；远端选择由组合仓库 / 服务层在解析时校验，
        // 本地存储只负责保存这个指针
        if selection.origin.is_local()
            && self
                .load_record::<WorkspaceEntry>(
                    Collection::Workspaces,
                    selection.workspace.as_str(),
                )?
                .is_none()
        {
            return Err(RepositoryError::NotFound(
                selection.workspace.as_str().to_owned(),
            ));
        }
        let mut state = self.load_workspace_state()?;
        state.current = Some(CurrentPointer::Scoped {
            origin: selection.origin.as_str().to_owned(),
            workspace: selection.workspace.to_string(),
        });
        self.write_state(Collection::Workspaces, &state)
    }

    fn remove(&self, id: &WorkspaceId) -> Result<(), RepositoryError> {
        let _guard = self.lock()?;
        if self
            .load_record::<WorkspaceEntry>(Collection::Workspaces, id.as_str())?
            .is_none()
        {
            return Err(RepositoryError::NotFound(id.as_str().to_owned()));
        }
        self.remove_record(Collection::Workspaces, id.as_str())?;
        // 删的正是本机的当前项：清指针。另有剩余项的移交由 service 层完成
        let mut state = self.load_workspace_state()?;
        if state.local_id().as_deref() == Some(id.as_str()) {
            state.current = None;
            self.write_state(Collection::Workspaces, &state)?;
        }
        Ok(())
    }

    fn relink_path(
        &self,
        id: &WorkspaceId,
        path: &Path,
        updated_at: i64,
    ) -> Result<(), RepositoryError> {
        let _guard = self.lock()?;
        let new_path = path.to_string_lossy().into_owned();
        let existing = self.scan_strict::<WorkspaceEntry>(Collection::Workspaces)?;
        for other in &existing {
            conflict_unique(
                other.path == new_path && other.id != id.as_str(),
                new_path.as_str(),
            )?;
        }
        let Some(mut entry) =
            self.load_record::<WorkspaceEntry>(Collection::Workspaces, id.as_str())?
        else {
            return Err(RepositoryError::NotFound(id.as_str().to_owned()));
        };
        entry.path = new_path;
        entry.updated_at = updated_at;
        self.write_record(Collection::Workspaces, &entry.id, &entry)
    }
}

// ---------------------------------------------------------------------- 服务器

impl ServerRepository for JsonStore {
    fn create(&self, meta: &ServerMeta) -> Result<(), RepositoryError> {
        let _guard = self.lock()?;
        let entry = ServerEntry::from(meta);
        // 工作目录必须已登记：文件系统没有外键，只能在这里显式检查
        if self
            .load_record::<WorkspaceEntry>(Collection::Workspaces, entry.workspace_id.as_str())?
            .is_none()
        {
            return Err(RepositoryError::Corrupt(format!(
                "server {} references unknown workspace {}",
                entry.name, entry.workspace_id
            )));
        }
        let existing = self.scan_strict::<ServerEntry>(Collection::Servers)?;
        for other in &existing {
            if other.id == entry.id {
                return Err(RepositoryError::AlreadyExists(entry.name.clone()));
            }
            check_server_conflict(other, &entry)?;
        }
        self.write_record(Collection::Servers, &entry.id, &entry)
    }

    fn get(&self, name: &ServerName) -> Result<ServerMeta, RepositoryError> {
        let _guard = self.lock()?;
        let (records, _) = self.scan::<ServerEntry>(Collection::Servers, true)?;
        let entry = records
            .into_iter()
            .find(|entry| entry.name == name.as_str())
            .ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))?;
        ServerMeta::try_from(entry)
    }

    fn list(&self) -> Result<Vec<ServerMeta>, RepositoryError> {
        let _guard = self.lock()?;
        let (records, _) = self.scan::<ServerEntry>(Collection::Servers, true)?;
        let mut rows = records
            .into_iter()
            .map(ServerMeta::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        rows.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        Ok(rows)
    }

    fn update_core(
        &self,
        name: &ServerName,
        version: &VersionId,
        jar: &domain::server::meta::InstalledJar,
        launch: Option<&domain::server::launch::LaunchSpec>,
        required_java_major: Option<domain::java::JavaMajor>,
        updated_at: i64,
    ) -> Result<ServerMeta, RepositoryError> {
        let _guard = self.lock()?;
        let mut entry = self.find_server_entry(name)?;
        entry.version = version.to_string();
        entry.jar_path = Some(jar.path.to_string_lossy().into_owned());
        entry.jar_size = Some(i64::try_from(jar.size).unwrap_or(i64::MAX));
        entry.checksum_algo = Some(jar.checksum.algorithm().as_str().to_owned());
        entry.checksum = Some(jar.checksum.hex().to_owned());
        let (kind, files) = models::launch_columns(launch);
        entry.launch_kind = kind;
        entry.launch_files = files;
        entry.required_java_major = required_java_major.map(domain::java::JavaMajor::as_i64);
        entry.updated_at = updated_at;
        self.write_record(Collection::Servers, &entry.id, &entry)?;
        ServerMeta::try_from(entry)
    }

    fn update_settings(
        &self,
        name: &ServerName,
        settings: &domain::server::settings::ServerSettings,
        updated_at: i64,
    ) -> Result<ServerMeta, RepositoryError> {
        let _guard = self.lock()?;
        let mut entry = self.find_server_entry(name)?;
        entry.java_major = settings.java_major.map(domain::java::JavaMajor::as_i64);
        entry.ram_mib = settings.ram_mib.and_then(|mib| i32::try_from(mib).ok());
        entry.autostart = settings.autostart;
        entry.updated_at = updated_at;
        self.write_record(Collection::Servers, &entry.id, &entry)?;
        ServerMeta::try_from(entry)
    }

    fn remove(&self, name: &ServerName) -> Result<(), RepositoryError> {
        let _guard = self.lock()?;
        let entry = self.find_server_entry(name)?;
        self.remove_record(Collection::Servers, &entry.id)
    }

    fn remove_by_workspace(&self, workspace_id: &WorkspaceId) -> Result<(), RepositoryError> {
        let _guard = self.lock()?;
        // 逐条处理，损坏文件也要删：留着它会在下次校验时挡住所有写入，
        // 而它的归属已无法判断，只能一并清理
        for (path, record) in self.scan_raw::<ServerEntry>(Collection::Servers)? {
            let owned = match record {
                Ok(entry) => entry.workspace_id == workspace_id.as_str(),
                Err(_) => true,
            };
            if owned {
                remove_file_checked(&path)?;
            }
        }
        Ok(())
    }

    fn relink(
        &self,
        name: &ServerName,
        workspace_id: &WorkspaceId,
        rel_dir: &Path,
        updated_at: i64,
    ) -> Result<ServerMeta, RepositoryError> {
        let _guard = self.lock()?;
        if self
            .load_record::<WorkspaceEntry>(Collection::Workspaces, workspace_id.as_str())?
            .is_none()
        {
            return Err(RepositoryError::NotFound(workspace_id.as_str().to_owned()));
        }
        let mut entry = self.find_server_entry(name)?;
        let target_workspace = workspace_id.as_str().to_owned();
        let mut candidate = entry.clone();
        candidate.workspace_id.clone_from(&target_workspace);
        candidate.rel_dir = rel_dir.to_string_lossy().into_owned();
        let existing = self.scan_strict::<ServerEntry>(Collection::Servers)?;
        for other in &existing {
            if other.id != candidate.id {
                check_server_conflict(other, &candidate)?;
            }
        }
        entry.workspace_id = candidate.workspace_id;
        entry.rel_dir = candidate.rel_dir;
        entry.updated_at = updated_at;
        self.write_record(Collection::Servers, &entry.id, &entry)?;
        ServerMeta::try_from(entry)
    }
}

impl JsonStore {
    /// 按名称读服务器原记录；名称冲突（大于 1 条）按损坏拒绝
    fn find_server_entry(&self, name: &ServerName) -> Result<ServerEntry, RepositoryError> {
        let (records, _) = self.scan::<ServerEntry>(Collection::Servers, true)?;
        let mut found = records
            .into_iter()
            .filter(|entry| entry.name == name.as_str());
        let Some(entry) = found.next() else {
            return Err(RepositoryError::NotFound(name.as_str().to_owned()));
        };
        if found.next().is_some() {
            return Err(RepositoryError::Corrupt(format!(
                "duplicate server name {}",
                name.as_str()
            )));
        }
        Ok(entry)
    }
}

/// 名称全局唯一；同工作区下 `rel_dir` 唯一
fn check_server_conflict(
    other: &ServerEntry,
    candidate: &ServerEntry,
) -> Result<(), RepositoryError> {
    conflict_unique(other.name == candidate.name, &candidate.name)?;
    conflict_unique(
        other.workspace_id == candidate.workspace_id && other.rel_dir == candidate.rel_dir,
        &candidate.rel_dir,
    )
}

// ------------------------------------------------------------------- Java 运行时

impl JavaRuntimeRepository for JsonStore {
    fn upsert(&self, runtime: &JavaRuntime) -> Result<(), RepositoryError> {
        let _guard = self.lock()?;
        let mut entry = JavaRuntimeEntry::from(runtime);
        let existing = self.scan_strict::<JavaRuntimeEntry>(Collection::Java)?;
        // 同路径视为同一运行时：复用原 id，避免同一个 JDK 登记两遍
        if let Some(existing_id) = existing
            .iter()
            .find(|other| other.path == entry.path)
            .map(|other| other.id.as_str())
        {
            entry.id = existing_id.to_owned();
        }
        self.write_record(Collection::Java, &entry.id, &entry)
    }

    fn list(&self) -> Result<Vec<JavaRuntime>, RepositoryError> {
        let _guard = self.lock()?;
        let (records, _) = self.scan::<JavaRuntimeEntry>(Collection::Java, true)?;
        let mut rows = records
            .into_iter()
            .map(JavaRuntime::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        rows.sort_by(|a, b| {
            b.major
                .as_i64()
                .cmp(&a.major.as_i64())
                .then_with(|| a.path.cmp(&b.path))
        });
        Ok(rows)
    }

    fn get_by_path(&self, path: &Path) -> Result<JavaRuntime, RepositoryError> {
        let _guard = self.lock()?;
        let raw = path.to_string_lossy().into_owned();
        let (records, _) = self.scan::<JavaRuntimeEntry>(Collection::Java, true)?;
        let entry = records
            .into_iter()
            .find(|entry| entry.path == raw)
            .ok_or_else(|| RepositoryError::NotFound(raw))?;
        JavaRuntime::try_from(entry)
    }

    fn remove(&self, path: &Path) -> Result<(), RepositoryError> {
        let _guard = self.lock()?;
        let raw = path.to_string_lossy().into_owned();
        let (records, _) = self.scan::<JavaRuntimeEntry>(Collection::Java, true)?;
        let id = records
            .into_iter()
            .find(|entry| entry.path == raw)
            .map(|entry| entry.id)
            .ok_or_else(|| RepositoryError::NotFound(raw.clone()))?;
        self.remove_record(Collection::Java, &id)
    }

    fn scan_state(&self) -> Result<Option<i64>, RepositoryError> {
        let _guard = self.lock()?;
        Ok(self.load_java_state()?.scanned_at)
    }

    fn mark_scanned(&self, at: i64) -> Result<(), RepositoryError> {
        let _guard = self.lock()?;
        let mut state = self.load_java_state()?;
        state.scanned_at = Some(at);
        self.write_state(Collection::Java, &state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::java::JavaMajor;
    use domain::server::artifact::{Checksum, ChecksumAlgorithm};
    use domain::server::meta::{InstalledJar, ServerId};
    use domain::server::version::ServerVariant;

    const WS01: &str = "550e8400-e29b-41d4-a716-446655440001";
    const WS02: &str = "550e8400-e29b-41d4-a716-446655440002";

    fn sample_ws(name: &str, path: &str, id: &str) -> Workspace {
        Workspace {
            id: WorkspaceId::parse(id).expect("uuid"),
            name: WorkspaceName::parse(name).expect("valid"),
            path: PathBuf::from(path),
            is_current: false,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
        }
    }

    fn sample_meta(name: &str) -> ServerMeta {
        let mut id = *b"550e8400-e29b-41d4-a716-????????????";
        let mut hash: u32 = 0;
        for b in name.bytes() {
            hash = hash.wrapping_mul(31).wrapping_add(u32::from(b));
        }
        let hex = format!("{hash:012x}");
        id[24..].copy_from_slice(hex.as_bytes());
        let id = std::str::from_utf8(&id).expect("ascii");
        ServerMeta {
            id: ServerId::parse(id).expect("uuid"),
            name: ServerName::parse(name).expect("valid"),
            workspace_id: WorkspaceId::parse(WS01).expect("uuid"),
            rel_dir: PathBuf::from(name),
            version: VersionId::new("1.21.1"),
            variant: ServerVariant::Vanilla,
            jar: None,
            launch: None,
            settings: domain::server::settings::ServerSettings::default(),
            java_major: None,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
        }
    }

    fn sample_java(path: &str, major: i64, managed: bool) -> JavaRuntime {
        JavaRuntime {
            id: format!("java-{major}"),
            path: PathBuf::from(path),
            major: JavaMajor::parse(major).expect("major"),
            version: format!("{major}.0.9+9"),
            vendor: domain::java::JavaVendor::Temurin,
            managed,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
        }
    }

    fn add_ws(store: &JsonStore, ws: &Workspace) {
        WorkspaceRepository::create(store, ws).expect("ws");
    }

    fn add_server(store: &JsonStore, meta: &ServerMeta) {
        ServerRepository::create(store, meta).expect("server");
    }

    fn store_with_ws() -> JsonStore {
        let store = JsonStore::open_in_memory().expect("open");
        add_ws(&store, &sample_ws("ws01", "/tmp/ws01", WS01));
        store
    }

    fn file_count(store: &JsonStore, collection: Collection) -> usize {
        std::fs::read_dir(store.collection_dir(collection))
            .expect("read dir")
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some(RECORD_EXT))
            .count()
    }

    // ------------------------------------------------------------ 基本往返

    #[test]
    fn create_get_roundtrip() {
        let store = store_with_ws();
        add_server(&store, &sample_meta("alpha"));
        let got = ServerRepository::get(&store, &ServerName::parse("alpha").expect("valid"))
            .expect("get");
        assert_eq!(got, sample_meta("alpha"));
    }

    #[test]
    fn duplicate_name_rejected() {
        let store = store_with_ws();
        add_server(&store, &sample_meta("alpha"));
        let err = ServerRepository::create(&store, &sample_meta("alpha")).expect_err("dup");
        assert!(matches!(err, RepositoryError::AlreadyExists(_)));
    }

    #[test]
    fn update_core_persists_version_and_jar() {
        let store = store_with_ws();
        add_server(&store, &sample_meta("alpha"));
        let jar = InstalledJar {
            path: PathBuf::from("server.jar"),
            size: 42,
            checksum: Checksum::new(ChecksumAlgorithm::Sha1, "a".repeat(40)),
        };
        let version = VersionId::new("1.21.2");
        let required = JavaMajor::parse(21).expect("21");
        let updated = ServerRepository::update_core(
            &store,
            &ServerName::parse("alpha").expect("valid"),
            &version,
            &jar,
            None,
            Some(required),
            1_700_000_100,
        )
        .expect("update");
        assert_eq!(updated.version, version, "安装换版本要同步回写");
        assert_eq!(updated.jar, Some(jar));
        assert_eq!(updated.java_major, Some(required));
        assert_eq!(updated.updated_at, 1_700_000_100);
    }

    /// 未校验占位值（Fabric 服务端 JAR）落盘再读回必须仍是未校验：
    /// 空 hex 若被当成真实哈希，后续比对会永久失败
    #[test]
    fn unverified_jar_checksum_survives_roundtrip() {
        let store = store_with_ws();
        add_server(&store, &sample_meta("alpha"));
        let jar = InstalledJar {
            path: PathBuf::from("server.jar"),
            size: 293_000,
            checksum: Checksum::unverified(),
        };
        let name = ServerName::parse("alpha").expect("valid");
        ServerRepository::update_core(
            &store,
            &name,
            &VersionId::new("1.21.1"),
            &jar,
            None,
            None,
            1,
        )
        .expect("update");

        let got = ServerRepository::get(&store, &name).expect("get");
        let stored = got.jar.expect("jar 应落盘");
        assert_eq!(stored.size, 293_000);
        assert!(!stored.checksum.is_verified(), "未校验语义必须保留");
        assert_eq!(stored.checksum.hex(), "");
    }

    #[test]
    fn update_settings_persists_and_bumps_timestamp() {
        let store = store_with_ws();
        add_server(&store, &sample_meta("alpha"));
        let settings = domain::server::settings::ServerSettings {
            java_major: Some(JavaMajor::parse(21).expect("21")),
            ram_mib: Some(2048),
            autostart: true,
            backup_scope: domain::backup::BackupScope::all(),
        };
        let updated = ServerRepository::update_settings(
            &store,
            &ServerName::parse("alpha").expect("valid"),
            &settings,
            1_700_000_200,
        )
        .expect("update");
        assert_eq!(updated.settings, settings);
        let got = ServerRepository::get(&store, &ServerName::parse("alpha").expect("valid"))
            .expect("get");
        assert_eq!(got.settings, settings);
    }

    #[test]
    fn list_orders_by_name() {
        let store = store_with_ws();
        add_server(&store, &sample_meta("beta"));
        add_server(&store, &sample_meta("alpha"));
        let names: Vec<_> = ServerRepository::list(&store)
            .expect("list")
            .into_iter()
            .map(|m| m.name.as_str().to_owned())
            .collect();
        assert_eq!(names, ["alpha", "beta"]);
    }

    #[test]
    fn missing_server_is_not_found() {
        let store = store_with_ws();
        let err = ServerRepository::get(&store, &ServerName::parse("ghost").expect("valid"))
            .expect_err("missing");
        assert!(matches!(err, RepositoryError::NotFound(_)));
    }

    #[test]
    fn remove_single_server_clears_row() {
        let store = store_with_ws();
        add_server(&store, &sample_meta("alpha"));
        add_server(&store, &sample_meta("beta"));
        ServerRepository::remove(&store, &ServerName::parse("alpha").expect("valid"))
            .expect("remove");
        let rest: Vec<_> = ServerRepository::list(&store)
            .expect("list")
            .into_iter()
            .map(|m| m.name.as_str().to_owned())
            .collect();
        assert_eq!(rest, ["beta"]);
        assert_eq!(
            file_count(&store, Collection::Servers),
            1,
            "记录文件必须删掉"
        );
    }

    #[test]
    fn remove_by_workspace_clears_servers() {
        let store = store_with_ws();
        add_server(&store, &sample_meta("alpha"));
        add_server(&store, &sample_meta("beta"));
        let ws_id = WorkspaceId::parse(WS01).expect("uuid");
        ServerRepository::remove_by_workspace(&store, &ws_id).expect("clear");
        assert!(ServerRepository::list(&store).expect("list").is_empty());
        assert_eq!(file_count(&store, Collection::Servers), 0);
    }

    #[test]
    fn relink_moves_server() {
        let store = JsonStore::open_in_memory().expect("open");
        add_ws(&store, &sample_ws("ws01", "/tmp/ws01", WS01));
        add_ws(&store, &sample_ws("ws02", "/tmp/ws02", WS02));
        add_server(&store, &sample_meta("alpha"));
        let ws_id02 = WorkspaceId::parse(WS02).expect("uuid");
        let moved = ServerRepository::relink(
            &store,
            &ServerName::parse("alpha").expect("valid"),
            &ws_id02,
            Path::new("alpha"),
            1_700_000_200,
        )
        .expect("relink");
        assert_eq!(moved.workspace_id, ws_id02);
    }

    #[test]
    fn relink_missing_is_not_found() {
        let store = store_with_ws();
        let ws_id = WorkspaceId::parse(WS01).expect("uuid");
        let err = ServerRepository::relink(
            &store,
            &ServerName::parse("ghost").expect("valid"),
            &ws_id,
            Path::new("ghost"),
            0,
        )
        .expect_err("missing");
        assert!(matches!(err, RepositoryError::NotFound(_)));
    }

    #[test]
    fn relink_to_unknown_workspace_is_not_found() {
        let store = store_with_ws();
        add_server(&store, &sample_meta("alpha"));
        let err = ServerRepository::relink(
            &store,
            &ServerName::parse("alpha").expect("valid"),
            &WorkspaceId::parse(WS02).expect("uuid"),
            Path::new("alpha"),
            0,
        )
        .expect_err("unknown ws");
        assert!(matches!(err, RepositoryError::NotFound(_)));
    }

    // -------------------------------------------------------- 目录布局与文件

    /// 一条记录一个文件，文件名即 id
    #[test]
    fn one_file_per_record() {
        let store = JsonStore::open_in_memory().expect("open");
        add_ws(&store, &sample_ws("ws01", "/tmp/ws01", WS01));
        add_ws(&store, &sample_ws("ws02", "/tmp/ws02", WS02));
        assert_eq!(file_count(&store, Collection::Workspaces), 2);
        assert!(
            store
                .record_path(Collection::Workspaces, WS01)
                .expect("path")
                .is_file()
        );
        add_server(&store, &sample_meta("alpha"));
        assert_eq!(file_count(&store, Collection::Servers), 1);
        JavaRuntimeRepository::upsert(&store, &sample_java("/opt/jdk21", 21, true)).expect("java");
        assert_eq!(file_count(&store, Collection::Java), 1);
    }

    /// 记录文件内容是带 schema 的独立 JSON 对象
    #[test]
    fn record_file_is_self_describing() {
        let store = store_with_ws();
        let text = std::fs::read_to_string(
            store
                .record_path(Collection::Workspaces, WS01)
                .expect("path"),
        )
        .expect("read");
        let value: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(value["version"], models::RECORD_VERSION);
        assert_eq!(value["id"], WS01);
        assert_eq!(value["name"], "ws01");
        assert!(value.get("is_current").is_none(), "当前标记不进记录文件");
    }

    /// 数据跨 `open` 重开仍在
    #[test]
    fn data_survives_reopen() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        {
            let store = JsonStore::open(dir.path()).expect("open");
            add_ws(&store, &sample_ws("ws01", "/tmp/ws01", WS01));
            WorkspaceRepository::set_current(&store, &WorkspaceId::parse(WS01).expect("uuid"))
                .expect("current");
            add_server(&store, &sample_meta("alpha"));
            JavaRuntimeRepository::upsert(&store, &sample_java("/opt/jdk21", 21, true))
                .expect("java");
            JavaRuntimeRepository::mark_scanned(&store, 1_700_000_000).expect("mark");
        }
        let store = JsonStore::open(dir.path()).expect("reopen");
        assert_eq!(WorkspaceRepository::list(&store).expect("list").len(), 1);
        assert_eq!(
            WorkspaceRepository::current(&store)
                .expect("current")
                .map(|ws| ws.name.as_str().to_owned()),
            Some("ws01".to_owned())
        );
        assert_eq!(
            ServerRepository::list(&store).expect("servers"),
            vec![sample_meta("alpha")]
        );
        assert_eq!(
            JavaRuntimeRepository::scan_state(&store).expect("state"),
            Some(1_700_000_000)
        );
    }

    // ----------------------------------------------------------- 故障隔离

    /// 坏一个文件只影响那一条：list 跳过它，其余照常
    #[test]
    fn corrupt_record_is_isolated() {
        let store = store_with_ws();
        add_server(&store, &sample_meta("alpha"));
        add_server(&store, &sample_meta("beta"));
        let broken = store
            .record_path(Collection::Servers, sample_meta("beta").id.as_str())
            .expect("path");
        std::fs::write(&broken, b"{ not json").expect("corrupt");

        let list = ServerRepository::list(&store).expect("list 不应整体失败");
        assert_eq!(list.len(), 1, "只跳过坏的那一条");
        assert_eq!(list[0].name.as_str(), "alpha");

        let err = ServerRepository::get(&store, &ServerName::parse("beta").expect("valid"))
            .expect_err("坏记录读不出");
        assert!(
            matches!(err, RepositoryError::NotFound(_))
                || matches!(err, RepositoryError::Corrupt(_))
        );
    }

    /// 坏记录挡住写入：看不见的记录可能与待写项冲突，不能默默放过
    #[test]
    fn corrupt_record_blocks_write() {
        let store = store_with_ws();
        add_server(&store, &sample_meta("alpha"));
        let broken = store
            .record_path(Collection::Servers, sample_meta("alpha").id.as_str())
            .expect("path");
        std::fs::write(&broken, b"{ not json").expect("corrupt");
        let err = ServerRepository::create(&store, &sample_meta("beta"))
            .expect_err("校验必须看得到坏文件");
        assert!(matches!(err, RepositoryError::Corrupt(_)), "got {err:?}");
        assert!(
            !store
                .record_path(Collection::Servers, sample_meta("beta").id.as_str())
                .expect("path")
                .exists()
        );
    }

    /// 文件名与内部 id 不一致 → 损坏，防止改名造出两条同 id
    #[test]
    fn mismatched_id_is_corrupt() {
        let store = store_with_ws();
        let path = store
            .record_path(Collection::Workspaces, WS01)
            .expect("path");
        let text = std::fs::read_to_string(&path).expect("read");
        let renamed = store
            .record_path(Collection::Workspaces, WS02)
            .expect("path");
        std::fs::write(&renamed, text).expect("rename");
        let err = store
            .load_record::<WorkspaceEntry>(Collection::Workspaces, WS02)
            .expect_err("id 不一致");
        assert!(matches!(err, RepositoryError::Corrupt(_)), "got {err:?}");
    }

    /// schema 版本不符 → 损坏
    #[test]
    fn unknown_record_schema_is_corrupt() {
        let store = store_with_ws();
        let path = store
            .record_path(Collection::Workspaces, WS01)
            .expect("path");
        std::fs::write(
            &path,
            format!(
                r#"{{"version":99,"id":"{WS01}","name":"ws01","path":"/tmp/ws01","created_at":0,"updated_at":0}}"#
            ),
        )
        .expect("seed");
        let err = store
            .load_record::<WorkspaceEntry>(Collection::Workspaces, WS01)
            .expect_err("schema");
        assert!(matches!(err, RepositoryError::Corrupt(_)), "got {err:?}");
    }

    /// 写入失败不破坏旧文件
    #[test]
    fn failed_write_keeps_previous_file() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let store = JsonStore::open(dir.path()).expect("open");
        add_ws(&store, &sample_ws("ws01", "/tmp/ws01", WS01));
        let path = store
            .record_path(Collection::Workspaces, WS01)
            .expect("path");
        let before = std::fs::read(&path).expect("read before");
        // 用目录占住临时文件名，模拟写盘失败
        std::fs::create_dir(path.with_file_name(format!(".{WS01}.json.tmp"))).expect("block");
        let err = WorkspaceRepository::relink_path(
            &store,
            &WorkspaceId::parse(WS01).expect("uuid"),
            Path::new("/tmp/moved"),
            1,
        )
        .expect_err("write");
        assert!(matches!(err, RepositoryError::Storage(_)), "got {err:?}");
        assert_eq!(std::fs::read(&path).expect("read after"), before);
    }

    /// 写入不留 `.tmp` 残留
    #[test]
    fn successful_write_leaves_no_temp_file() {
        let store = store_with_ws();
        let leftovers: Vec<_> = std::fs::read_dir(store.collection_dir(Collection::Workspaces))
            .expect("dir")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "残留 {leftovers:?}");
    }

    // ------------------------------------------------------------ 当前工作区

    #[test]
    fn create_current_workspace_sets_pointer() {
        let store = JsonStore::open_in_memory().expect("open");
        let mut ws = sample_ws("ws01", "/tmp/ws01", WS01);
        ws.is_current = true;
        add_ws(&store, &ws);
        let current = WorkspaceRepository::current(&store)
            .expect("current")
            .expect("some");
        assert_eq!(current.name.as_str(), "ws01");

        // 后续新增不抢当前标记
        add_ws(&store, &sample_ws("ws02", "/tmp/ws02", WS02));
        assert_eq!(
            WorkspaceRepository::current(&store)
                .expect("current")
                .expect("some")
                .name
                .as_str(),
            "ws01"
        );
    }

    #[test]
    fn workspace_current_is_single() {
        let store = JsonStore::open_in_memory().expect("open");
        add_ws(&store, &sample_ws("ws01", "/tmp/ws01", WS01));
        add_ws(&store, &sample_ws("ws02", "/tmp/ws02", WS02));
        let id01 = WorkspaceId::parse(WS01).expect("uuid");
        let id02 = WorkspaceId::parse(WS02).expect("uuid");
        WorkspaceRepository::set_current(&store, &id01).expect("current 1");
        WorkspaceRepository::set_current(&store, &id02).expect("current 2");
        let currents: Vec<_> = WorkspaceRepository::list(&store)
            .expect("list")
            .into_iter()
            .filter(|w| w.is_current)
            .collect();
        assert_eq!(currents.len(), 1, "单值语义只能有一个当前");
        assert_eq!(currents[0].name.as_str(), "ws02");
    }

    /// 删除当前工作区后指针清空
    #[test]
    fn remove_current_workspace_clears_marker() {
        let store = JsonStore::open_in_memory().expect("open");
        add_ws(&store, &sample_ws("ws01", "/tmp/ws01", WS01));
        let id = WorkspaceId::parse(WS01).expect("uuid");
        WorkspaceRepository::set_current(&store, &id).expect("current");
        WorkspaceRepository::remove(&store, &id).expect("remove");
        assert!(
            WorkspaceRepository::current(&store)
                .expect("current")
                .is_none()
        );
    }

    /// `state.json` 指向不存在的记录 → 按「无当前」处理，不报错也不改写文件
    #[test]
    fn dangling_current_pointer_is_none() {
        let store = store_with_ws();
        let state_path = store.state_path(Collection::Workspaces);
        std::fs::write(
            &state_path,
            format!(r#"{{"version":1,"current":"{WS02}"}}"#),
        )
        .expect("seed");
        assert!(
            WorkspaceRepository::current(&store)
                .expect("悬空指针是安全失败")
                .is_none()
        );
        let text = std::fs::read_to_string(&state_path).expect("read");
        assert!(text.contains(WS02), "读不应改写状态文件");
    }

    /// v1 的裸 uuid 指针读作「本地选择」
    #[test]
    fn v1_state_migrates_to_local_selection() {
        let store = store_with_ws();
        let state_path = store.state_path(Collection::Workspaces);
        std::fs::write(
            &state_path,
            format!(r#"{{"version":1,"current":"{WS01}"}}"#),
        )
        .expect("seed");
        let selection = WorkspaceRepository::current_selection(&store)
            .expect("selection")
            .expect("some");
        assert_eq!(selection.origin, Origin::Local);
        assert_eq!(selection.workspace.as_str(), WS01);
        assert!(
            WorkspaceRepository::current(&store)
                .expect("current")
                .is_some()
        );
    }

    /// 远端选择只存本机指针：不冒充本地 current，也不给本机记录打 current
    #[test]
    fn remote_selection_is_client_local_only() {
        let store = store_with_ws();
        let selection = WorkspaceSelection {
            origin: Origin::Remote(port::RemoteName::parse("r1").expect("name")),
            workspace: WorkspaceId::parse(WS02).expect("uuid"),
        };
        WorkspaceRepository::set_current_selection(&store, &selection).expect("set");
        assert_eq!(
            WorkspaceRepository::current_selection(&store).expect("selection"),
            Some(selection)
        );
        assert!(
            WorkspaceRepository::current(&store)
                .expect("current")
                .is_none(),
            "远端选择不能冒充本地 current"
        );
        assert!(
            WorkspaceRepository::list(&store)
                .expect("list")
                .iter()
                .all(|w| !w.is_current),
            "本机记录不能因为远端选择而被标成 current"
        );
        let text = std::fs::read_to_string(store.state_path(Collection::Workspaces)).expect("read");
        let value: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(value["current"]["origin"], "r1");
        assert_eq!(value["current"]["workspace"], WS02);
    }

    /// 新建即当前时写 v2 本地指针
    #[test]
    fn first_current_workspace_writes_v2_local_pointer() {
        let store = JsonStore::open_in_memory().expect("open");
        let mut ws = sample_ws("ws01", "/tmp/ws01", WS01);
        ws.is_current = true;
        add_ws(&store, &ws);
        let text = std::fs::read_to_string(store.state_path(Collection::Workspaces)).expect("read");
        let value: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(value["current"]["origin"], "local");
        assert_eq!(value["current"]["workspace"], WS01);
    }

    #[test]
    fn set_current_missing_is_not_found() {
        let store = JsonStore::open_in_memory().expect("open");
        let err =
            WorkspaceRepository::set_current(&store, &WorkspaceId::parse(WS01).expect("uuid"))
                .expect_err("missing");
        assert!(matches!(err, RepositoryError::NotFound(_)));
    }

    /// `state.json` 版本不符 → 写路径拒绝，读路径降级为「无当前」
    ///
    /// 读路径不能整体失败：`state.json` 是全局单点，若它坏掉就
    /// 让 `workspace list` / `server list` 全不可用，就退回到了单文件方案
    /// 「一处损坏全盘瘫痪」的老问题。
    #[test]
    fn corrupt_state_degrades_reads_but_blocks_writes() {
        let store = store_with_ws();
        std::fs::write(
            store.state_path(Collection::Workspaces),
            br#"{"version":7,"current":null}"#,
        )
        .expect("seed");

        let list = WorkspaceRepository::list(&store).expect("list 仍应可用");
        assert_eq!(list.len(), 1);
        assert!(!list[0].is_current, "读不懂指针时按无当前展示");
        assert!(
            WorkspaceRepository::current(&store)
                .expect("current 降级")
                .is_none()
        );

        // 写路径仍必须失败：不能静默覆盖一份读不懂的状态文件
        let err =
            WorkspaceRepository::set_current(&store, &WorkspaceId::parse(WS01).expect("uuid"))
                .expect_err("state 损坏时不得写入");
        assert!(matches!(err, RepositoryError::Corrupt(_)), "got {err:?}");
    }

    /// 不安全 id 被拒绝，不会被拼进路径
    ///
    /// `id` 既来自文件名也来自记录内容；含 `..` 或分隔符的 id
    /// 会让读写落到集合目录之外。
    #[test]
    fn unsafe_record_id_is_rejected() {
        let store = store_with_ws();
        for bad in ["..", ".", "a/b", "a\\b", ""] {
            assert!(
                store.record_path(Collection::Workspaces, bad).is_err(),
                "{bad} 必须被拒绝"
            );
        }
        assert!(
            store.record_path(Collection::Workspaces, WS01).is_ok(),
            "UUID 必须通过"
        );
        assert!(
            store.record_path(Collection::Java, "java-21").is_ok(),
            "生成式 id 必须通过"
        );
        assert!(
            store
                .record_path(Collection::Servers, "my.server")
                .is_ok_and(|path| path.parent()
                    == Some(store.collection_dir(Collection::Servers).as_path())),
            "带点的单层 id 必须留在集合目录内"
        );
    }

    /// 扫目录时 id 与 `record_path` 同口径：`state.json` 不是记录，
    /// 空 id / 越界 id 不会被拼成指向别的文件的路径
    #[test]
    fn scan_ids_share_record_path_rules() {
        let store = store_with_ws();
        for (name, id) in [
            ("state.json", None),
            (".json", None),
            ("a.json", Some("a")),
            ("my.server.json", Some("my.server")),
        ] {
            assert_eq!(
                JsonStore::record_id_of(Path::new(name)).as_deref(),
                id,
                "{name}"
            );
        }
        let dir = store.collection_dir(Collection::Servers);
        for name in ["state.json", ".json", "leftover.bak"] {
            std::fs::write(dir.join(name), b"{}").expect("seed");
        }
        std::fs::write(dir.join(".json"), r#"{"schema":1,"id":""}"#).expect("seed");
        assert!(
            ServerRepository::list(&store)
                .expect("list 不得被非记录文件拖垮")
                .is_empty()
        );
    }

    /// 记录内 id 被改成越界值 → 损坏，而不是写到目录外
    #[test]
    fn tampered_id_does_not_escape_collection_dir() {
        let store = store_with_ws();
        let path = store
            .record_path(Collection::Workspaces, WS01)
            .expect("path");
        std::fs::write(
            &path,
            r#"{"version":1,"id":"../escaped","name":"ws01","path":"/tmp/ws01","created_at":0,"updated_at":0}"#,
        )
        .expect("seed");
        let err = store
            .load_record::<WorkspaceEntry>(Collection::Workspaces, WS01)
            .expect_err("id 越界");
        assert!(matches!(err, RepositoryError::Corrupt(_)), "got {err:?}");
    }

    // -------------------------------------------------------------- 唯一键

    #[test]
    fn workspace_unique_keys() {
        let store = JsonStore::open_in_memory().expect("open");
        add_ws(&store, &sample_ws("ws01", "/tmp/ws01", WS01));

        let dup_name = WorkspaceRepository::create(&store, &sample_ws("ws01", "/tmp/other", WS02))
            .expect_err("dup name");
        assert!(matches!(dup_name, RepositoryError::AlreadyExists(_)));

        let dup_path = WorkspaceRepository::create(&store, &sample_ws("ws02", "/tmp/ws01", WS02))
            .expect_err("dup path");
        assert!(matches!(dup_path, RepositoryError::AlreadyExists(_)));

        assert_eq!(WorkspaceRepository::list(&store).expect("list").len(), 1);
    }

    #[test]
    fn server_unique_rel_dir_per_workspace() {
        let store = JsonStore::open_in_memory().expect("open");
        add_ws(&store, &sample_ws("ws01", "/tmp/ws01", WS01));
        add_ws(&store, &sample_ws("ws02", "/tmp/ws02", WS02));
        let mut first = sample_meta("alpha");
        first.rel_dir = PathBuf::from("shared");
        add_server(&store, &first);

        let mut other = sample_meta("beta");
        other.rel_dir = PathBuf::from("shared");
        let err = ServerRepository::create(&store, &other).expect_err("dup rel_dir");
        assert!(matches!(err, RepositoryError::AlreadyExists(_)));

        other.workspace_id = WorkspaceId::parse(WS02).expect("uuid");
        ServerRepository::create(&store, &other).expect("different workspace");
    }

    #[test]
    fn server_with_unknown_workspace_is_corrupt() {
        let store = JsonStore::open_in_memory().expect("open");
        let err = ServerRepository::create(&store, &sample_meta("alpha")).expect_err("no ws");
        assert!(matches!(err, RepositoryError::Corrupt(_)), "got {err:?}");
        assert_eq!(file_count(&store, Collection::Servers), 0, "失败不得落盘");
    }

    #[test]
    fn relink_path_rejects_taken_target() {
        let store = JsonStore::open_in_memory().expect("open");
        add_ws(&store, &sample_ws("ws01", "/tmp/ws01", WS01));
        add_ws(&store, &sample_ws("ws02", "/tmp/ws02", WS02));
        let err = WorkspaceRepository::relink_path(
            &store,
            &WorkspaceId::parse(WS02).expect("uuid"),
            Path::new("/tmp/ws01"),
            0,
        )
        .expect_err("taken");
        assert!(matches!(err, RepositoryError::AlreadyExists(_)));
        let ws = WorkspaceRepository::get(&store, &WorkspaceId::parse(WS02).expect("uuid"))
            .expect("get");
        assert_eq!(ws.path, PathBuf::from("/tmp/ws02"));
    }

    // ------------------------------------------------------------ 跨进程锁

    /// 锁存在时阻塞；释放后可继续（模拟另一个进程持锁）
    #[test]
    fn dir_lock_blocks_until_released() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join(".lock");
        let held = DirLock::acquire(path.clone()).expect("第一个拿到锁");
        assert!(path.exists(), "持锁期间锁文件存在");

        // 持锁期间其他持有者拿不到：用另一线程 + 超时验证不会立刻成功
        let probe_path = path.clone();
        let probe = std::thread::spawn(move || DirLock::acquire(probe_path).is_ok());
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(!probe.is_finished(), "第二个持有者必须等待，而不是立刻拿到");

        drop(held);
        // 等待者一放锁就会立刻补上，因此这里不能断言「锁文件不存在」，
        // 只能等它拿到、自己释放后再断言
        assert!(probe.join().expect("join"), "释放后等待者应拿到锁");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while path.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!path.exists(), "最终不残留锁文件");
    }

    /// 持有者已死（pid 不存在）的锁会被回收，不会永久卡住写入
    #[test]
    fn stale_lock_is_reclaimed() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join(".lock");
        // 一个几乎不可能存在的 pid：判据是「进程是否存活」而非文件年龄
        std::fs::write(&path, "4294967290").expect("seed");
        assert!(is_stale(&path), "死亡 pid 的锁必须判为残留");
        let lock = DirLock::acquire(path.clone()).expect("残留锁应被回收");
        drop(lock);
        assert!(!path.exists());
    }

    /// 持有者仍活着时，锁无论多旧都不回收
    ///
    /// 这是旧实现的缺陷：按文件 mtime 判断时，正常持锁超过阈值的进程
    /// 会被当成崩溃，锁被删除后两个进程同处临界区。
    #[test]
    fn live_lock_is_never_reclaimed() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join(".lock");
        std::fs::write(&path, std::process::id().to_string()).expect("seed");
        let ancient = std::time::SystemTime::now() - std::time::Duration::from_secs(86_400);
        let file = std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open");
        file.set_modified(ancient).expect("set mtime");
        drop(file);
        assert!(!is_stale(&path), "pid 存活则不是残留，与文件年龄无关");
    }

    /// 空或非数字的锁内容（写入瞬间崩溃）判为残留
    #[test]
    fn unreadable_lock_is_reclaimed() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join(".lock");
        std::fs::write(&path, "").expect("seed");
        assert!(is_stale(&path));
        std::fs::write(&path, "not-a-pid").expect("seed");
        assert!(is_stale(&path));
    }

    /// 写入结束后锁文件不残留
    #[test]
    fn lock_file_removed_after_write() {
        let store = store_with_ws();
        add_server(&store, &sample_meta("alpha"));
        assert!(!store.root().join(LOCK_FILE).exists(), "锁文件必须释放");
    }

    // ---------------------------------------------------------------- Java
    #[test]
    fn java_upsert_and_list_orders_by_major_desc() {
        let store = JsonStore::open_in_memory().expect("open");
        JavaRuntimeRepository::upsert(&store, &sample_java("/opt/jdk17", 17, false)).expect("17");
        JavaRuntimeRepository::upsert(&store, &sample_java("/opt/jdk21", 21, true)).expect("21");
        let list = JavaRuntimeRepository::list(&store).expect("list");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].major.as_i64(), 21);
        assert_eq!(list[1].major.as_i64(), 17);
    }

    #[test]
    fn java_upsert_same_path_updates_in_place() {
        let store = JsonStore::open_in_memory().expect("open");
        JavaRuntimeRepository::upsert(&store, &sample_java("/opt/jdk17", 17, false))
            .expect("first");
        let mut updated = sample_java("/opt/jdk17", 17, true);
        updated.version = "17.0.11+9".to_owned();
        JavaRuntimeRepository::upsert(&store, &updated).expect("second");
        let list = JavaRuntimeRepository::list(&store).expect("list");
        assert_eq!(list.len(), 1, "同路径不得产生两份记录");
        assert_eq!(file_count(&store, Collection::Java), 1);
        assert!(list[0].managed);
    }

    #[test]
    fn java_remove_missing_is_not_found() {
        let store = JsonStore::open_in_memory().expect("open");
        let err = JavaRuntimeRepository::remove(&store, Path::new("/ghost")).expect_err("missing");
        assert!(matches!(err, RepositoryError::NotFound(_)));
    }

    #[test]
    fn java_scan_state_roundtrip() {
        let store = JsonStore::open_in_memory().expect("open");
        assert_eq!(
            JavaRuntimeRepository::scan_state(&store).expect("state"),
            None
        );
        JavaRuntimeRepository::mark_scanned(&store, 1_700_000_000).expect("mark");
        assert_eq!(
            JavaRuntimeRepository::scan_state(&store).expect("state"),
            Some(1_700_000_000)
        );
    }
}
