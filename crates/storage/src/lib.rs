//! 元数据存储：一条记录一个 JSON 文件
//!
//! 目录布局（`{root}` = `{data_dir}/mcsup/db`）：
//!
//! ```text
//! {root}/workspaces/state.json      {"version":3,"current":{...}|null,"last_local":{...}|null}
//!                                   v1 `{"current":"<uuid>"}` 读作本地选择
//!                                   v2 无 `last_local`，读作「未记录」
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
use domain::mirror::SourceId;
use domain::server::meta::{ServerMeta, ServerName};
use domain::server::version::VersionId;
use domain::workspace::{Workspace, WorkspaceId, WorkspaceName};
use port::{
    JavaRuntimeRepository, LOCAL_NAME, Origin, RepositoryError, ServerRepository, SourceRouteStore,
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
/// v3 增加 `last_local`：记住切换到远端之前用的是哪个本地工作区，远端配对
/// 被移除时凭它撤回本地。
const WORKSPACE_STATE_VERSION: u32 = 3;
/// Java 集合的 `state.json` 仍是 v1（只有扫描时间）
const JAVA_STATE_VERSION: u32 = 1;
/// 下载源集合的 `state.json` 版本（上游面 → 选中源）
const DOWNLOAD_STATE_VERSION: u32 = 1;
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
    /// 最近一次被设为当前的**本地**工作区。
    ///
    /// 只用于远端配对被移除时撤回本地：它指向的记录可能已经被删除，
    /// 读取方按「没有回退目标」处理。v2 及更早的文件没有这个字段。
    #[serde(default)]
    last_local: Option<CurrentPointer>,
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

/// 下载源集合的全局状态：每个上游面选中的源
///
/// 只有一个文件、没有记录文件：这是一张「上游面 → 源」的表，条目少且总是
/// 整体读写。拆成一条记录一个文件反而要为 `forge-maven` 这类含连字符的键
/// 绕过文件名规则，收益为零。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RouteState {
    version: u32,
    /// 上游面键 → 源标识
    #[serde(default)]
    routes: std::collections::BTreeMap<String, String>,
}

impl RouteState {
    fn new() -> Self {
        Self {
            version: DOWNLOAD_STATE_VERSION,
            routes: std::collections::BTreeMap::new(),
        }
    }
}

impl WorkspaceState {
    fn new() -> Self {
        Self {
            version: WORKSPACE_STATE_VERSION,
            current: None,
            last_local: None,
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

    /// 本地选择指针的线上形态
    fn local_pointer(id: &str) -> CurrentPointer {
        CurrentPointer::Scoped {
            origin: LOCAL_NAME.to_owned(),
            workspace: id.to_owned(),
        }
    }

    /// 最近一次的本地选择；坏值 / 非本地来源视为「未记录」
    fn last_local_id(&self) -> Option<WorkspaceId> {
        let (origin, workspace) = match self.last_local.as_ref()? {
            CurrentPointer::Bare(id) => (Origin::Local, id.clone()),
            CurrentPointer::Scoped { origin, workspace } => {
                (Origin::parse(origin).ok()?, workspace.clone())
            }
        };
        origin
            .is_local()
            .then(|| WorkspaceId::parse(&workspace).ok())
            .flatten()
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
    Download,
}

impl Collection {
    /// 子目录名
    fn dir(self) -> &'static str {
        match self {
            Self::Workspaces => "workspaces",
            Self::Servers => "servers",
            Self::Java => "java",
            Self::Download => "download",
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
            Collection::Download,
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
        let dir_lock = DirLock::acquire(&self.root.join(LOCK_FILE))?;
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
        let expected = T::expected_version();
        if state.state_version() != expected {
            return Err(RepositoryError::Corrupt(format!(
                "{}: version {}, expected {expected}",
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

/// `state.json` 能自报格式版本，也知道自己该是哪个版本
///
/// 版本常量由类型自己给出（[`Self::expected_version`]），调用方不再猜。
/// 原来 `load_state` 的泛型校验里写死了 `JAVA_STATE_VERSION`，于是任何非
/// Java 集合都会被拿去和 Java 的版本号比对：`WorkspaceState`（版本 2）读
/// java 的 1，直接被判 `Corrupt`。
trait StateVersion {
    /// 文件里记录的版本
    fn state_version(&self) -> u32;
    /// 当前代码期望的版本
    fn expected_version() -> u32;
}

impl StateVersion for WorkspaceState {
    fn state_version(&self) -> u32 {
        self.version
    }
    fn expected_version() -> u32 {
        WORKSPACE_STATE_VERSION
    }
}

impl StateVersion for JavaState {
    fn state_version(&self) -> u32 {
        self.version
    }
    fn expected_version() -> u32 {
        JAVA_STATE_VERSION
    }
}

impl StateVersion for RouteState {
    fn state_version(&self) -> u32 {
        self.version
    }
    fn expected_version() -> u32 {
        DOWNLOAD_STATE_VERSION
    }
}

/// 序列化 → 写同目录临时文件 → 刷盘 → rename 覆盖
///
/// 临时文件与目标同目录，保证 rename 是同一文件系统上的原子替换；
/// Windows 的 `rename` 覆盖已存在目标，无需先删除（先删会留下无文件窗口）。
/// 失败时清掉临时文件并抛出错误，目标文件保持原样。
fn write_atomically(path: &Path, value: &impl Serialize) -> Result<(), RepositoryError> {
    // 序列化失败是「值写不出来」，不是记录损坏（那条在读取侧）：包成 IO 失败
    // 保持后端等级，同时把真实原因留在错误链里
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|err| storage_err(io::Error::other(format!("serialize record: {err}"))))?;
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
/// 校验 `rel_dir` 是单层目录名，否则拒绝写入
fn check_rel_dir(rel_dir: &Path) -> Result<(), RepositoryError> {
    if domain::server::meta::is_single_segment(rel_dir) {
        Ok(())
    } else {
        Err(RepositoryError::InvalidRelDir {
            rel_dir: rel_dir.to_path_buf(),
        })
    }
}

fn is_safe_record_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id != "." && id != ".." && !id.contains(['/', '\\', '\0'])
}

/// 装箱一个实现层失败
///
/// 约束要求实现 [`port::StorageFailure`]：入口要靠它拿到等级，接受任意
/// `Error` 就只能默认后端失败，远端「未授权」会被压成「对端不可用」。
fn storage_err(err: impl port::StorageFailure) -> RepositoryError {
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
/// 实现是**操作系统文件锁**（Unix `flock` / Windows `LockFileEx`），不是
/// 「文件存在即持锁」。存在性判定在持有者崩溃后只能靠 pid 存活探测回收，
/// 而「探测到死亡 → 删文件 → 重建」这段窗口里，一个进程可能删掉别人刚
/// 创建的新锁、让两个进程同时进入临界区。内核锁挂在文件句柄上：进程一死
/// 就自动释放，不需要也不允许删除锁文件——删文件会让等待者在新旧 inode
/// 上各持一把锁。
struct DirLock {
    /// 锁句柄：关闭（含进程崩溃）即释放；只用于保活
    _file: std::fs::File,
}

impl DirLock {
    fn acquire(path: &Path) -> Result<Self, RepositoryError> {
        // 锁文件永不删除，这里 open 兼 create，等待者拿到的是同一个 inode
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(storage_err)?;
        // 进程内已排队，超时只可能是别的进程持锁
        let deadline = std::time::Instant::now() + LOCK_TIMEOUT;
        let mut delay = std::time::Duration::from_millis(2);
        loop {
            match file.try_lock() {
                Ok(()) => {
                    // 内容只是排障线索，不参与互斥判定；写失败不影响持锁
                    let _ = file.set_len(0);
                    let _ = writeln!(&file, "{}", std::process::id());
                    return Ok(Self { _file: file });
                }
                Err(std::fs::TryLockError::WouldBlock) => {
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
                Err(std::fs::TryLockError::Error(err)) => return Err(storage_err(err)),
            }
        }
    }
}

/// 单次拿到锁的最长等待时间
const LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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
            state.current = Some(WorkspaceState::local_pointer(ws.id.as_str()));
            state.last_local = Some(WorkspaceState::local_pointer(ws.id.as_str()));
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

    fn last_local_selection(&self) -> Result<Option<WorkspaceId>, RepositoryError> {
        Ok(self.load_workspace_state()?.last_local_id())
    }

    fn clear_current_selection(&self) -> Result<(), RepositoryError> {
        let _guard = self.lock()?;
        let mut state = self.load_workspace_state()?;
        if state.current.is_none() {
            return Ok(());
        }
        state.current = None;
        self.write_state(Collection::Workspaces, &state)
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
        state.current = Some(WorkspaceState::local_pointer(id.as_str()));
        state.last_local = Some(WorkspaceState::local_pointer(id.as_str()));
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
        // 指向远端时保留 `last_local`：它就是「断开后撤回到哪」的依据
        if selection.origin.is_local() {
            state.last_local = Some(WorkspaceState::local_pointer(selection.workspace.as_str()));
        }
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
        // 删的正是本机的当前项：清指针。另有剩余项的移交由 service 层完成。
        // `last_local` 指向被删记录时一并清掉：留着只会让撤回逻辑多一次无效查找
        let mut state = self.load_workspace_state()?;
        let clears_current = state.local_id().as_deref() == Some(id.as_str());
        let clears_last = state.last_local_id().as_ref() == Some(id);
        if clears_current {
            state.current = None;
        }
        if clears_last {
            state.last_local = None;
        }
        if clears_current || clears_last {
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
        // 落盘前校验：绝对路径 / `..` / 分隔符一旦写入，之后每次由
        // workspace + rel_dir 拼出的绝对路径都会指向工作区之外
        check_rel_dir(&meta.rel_dir)?;
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
        // 全量回写，逐字段抄一遍会漏：`backup_scope` 曾经就没写，重启后
        // `server set backup-scope` 的结果凭空消失，备份又回到全目录
        entry.ram_mib = settings.ram_mib.map(i64::from);
        entry.autostart = settings.autostart;
        entry.backup_scope = settings.backup_scope.entries().to_vec();
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
        // 边界校验前移到写入路径：`doctor` 的事后体检拦不住已经落盘的坏值
        check_rel_dir(rel_dir)?;
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

// --------------------------------------------------------------- 下载源选择

impl JsonStore {
    fn load_route_state(&self) -> Result<RouteState, RepositoryError> {
        self.load_state(Collection::Download, RouteState::new)
    }
}

impl SourceRouteStore for JsonStore {
    fn route(&self, upstream: &str) -> Result<Option<SourceId>, RepositoryError> {
        let _guard = self.lock()?;
        let state = self.load_route_state()?;
        Ok(state
            .routes
            .get(upstream)
            .and_then(|raw| parse_source_id(raw)))
    }

    fn remember(&self, upstream: &str, source: &SourceId) -> Result<(), RepositoryError> {
        let _guard = self.lock()?;
        let mut state = self.load_route_state()?;
        state
            .routes
            .insert(upstream.to_owned(), source.as_str().to_owned());
        self.write_state(Collection::Download, &state)
    }

    fn forget(&self, upstream: &str) -> Result<(), RepositoryError> {
        let _guard = self.lock()?;
        let mut state = self.load_route_state()?;
        // 本来就没有这条记录时不写盘：失败路径上无谓的写入会把状态文件
        // 的 mtime 推到下一次成功选择之后，排查时看不出真正的选择时间
        if state.routes.remove(upstream).is_none() {
            return Ok(());
        }
        self.write_state(Collection::Download, &state)
    }
}

/// 把状态文件里的源标识解析回领域类型
///
/// 解析失败按「没有记录」处理，不报 [`RepositoryError::Corrupt`]：这个文件的
/// 唯一作用是省掉一次探测，内容不可读时重新探测即可恢复。让一条手改过的记录
/// 把整个下载流程判成损坏，代价远大于收益。
fn parse_source_id(raw: &str) -> Option<SourceId> {
    SourceId::new(raw.to_owned()).ok()
}

#[cfg(test)]
mod tests;
