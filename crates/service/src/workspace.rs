use crate::fs_error::FsError;
use domain::workspace::{InvalidWorkspaceName, Workspace, WorkspaceId, WorkspaceName};
use port::{RepositoryError, ServerRepository, WorkspaceRepository};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;

/// `workspace.toml` marker 文件名；入口侧判空预检与本层判空必须同口径
pub const MARKER_FILE: &str = "workspace.toml";
const MARKER_SCHEMA: u32 = 1;
/// 噪声文件：判空时忽略（macOS / Windows 产物）；入口侧预检复用
pub const NOISE_ENTRIES: &[&str] = &[".DS_Store", "Thumbs.db", "desktop.ini", ".localized"];

/// 工作目录操作失败
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WorkspaceError {
    /// 目标目录非空
    #[error("directory is not empty: {0}")]
    NotEmpty(PathBuf),
    /// 目录缺少 `workspace.toml`，不是工作目录
    #[error("not a workspace (missing {MARKER_FILE}): {0}")]
    NotAWorkspace(PathBuf),
    /// 名称或路径冲突
    #[error("already exists: {0}")]
    AlreadyExists(String),
    /// 工作目录不存在
    #[error("workspace not found: {0}")]
    NotFound(String),
    /// 前缀匹配到多个工作目录
    #[error(
        "ambiguous workspace selector {selector:?}; candidates: {}",
        candidates.join(", ")
    )]
    Ambiguous {
        selector: String,
        candidates: Vec<String>,
    },
    /// 尚无当前工作区
    #[error("no current workspace; run `mcsup workspace init` or `mcsup workspace use` first")]
    NoCurrent,
    /// 文件系统 / marker 失败（共用载荷见 [`FsError`]）
    #[error(transparent)]
    Fs(#[from] FsError),
    /// 注册表失败
    #[error(transparent)]
    Repo(#[from] RepositoryError),
    /// 名称非法
    #[error(transparent)]
    InvalidName(#[from] InvalidWorkspaceName),
}

impl WorkspaceError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        FsError::io(path, source).into()
    }

    fn marker(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        FsError::marker(path, message).into()
    }
}

/// `workspace.toml` 载荷（service 层直接读写，不抽 port）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MarkerToml {
    schema: u32,
    id: String,
    name: String,
    created_at: i64,
}

/// init 结果
#[derive(Debug, Clone)]
pub enum InitOutcome {
    /// 新建并注册
    Created(Workspace),
    /// 目录已有合法 marker、数据库缺记录：补注册（中断恢复）
    Recovered(Workspace),
    /// 已在数据库中
    AlreadyRegistered(Workspace),
}

/// 工作目录编排：判空、目录结构、marker、注册表
pub struct WorkspaceService {
    repo: Arc<dyn WorkspaceRepository>,
    /// 服务器登记（外键引用工作目录）：注销前先清，见 [`WorkspaceService::remove`]
    servers: Arc<dyn ServerRepository>,
}

impl std::fmt::Debug for WorkspaceService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceService").finish_non_exhaustive()
    }
}

impl WorkspaceService {
    /// 组合注册表
    #[must_use]
    pub fn new(repo: Arc<dyn WorkspaceRepository>, servers: Arc<dyn ServerRepository>) -> Self {
        Self { repo, servers }
    }

    /// 创建并注册工作目录
    ///
    /// 顺序：判空 → 建目录 → 写 marker → 入库。目录已有合法 marker 且库中
    /// 无记录时补注册；库中已有则返回 [`InitOutcome::AlreadyRegistered`]。
    ///
    /// # Errors
    ///
    /// 目录非空、名称冲突、IO / marker / 注册表失败时返回 [`WorkspaceError`]
    pub fn init(&self, dir: &Path, name: Option<&str>) -> Result<InitOutcome, WorkspaceError> {
        let now = crate::util::unix_now();
        let dir = absolute_path(dir);

        if let Some(marker) = read_marker(&dir)? {
            return self.recover(dir, &marker, now);
        }

        if dir.exists() && !is_empty_dir(&dir)? {
            return Err(WorkspaceError::NotEmpty(dir));
        }

        let name = match name {
            Some(raw) => WorkspaceName::parse(raw)?,
            None => self.next_default_name()?,
        };
        if self.name_taken(&name) {
            return Err(WorkspaceError::AlreadyExists(name.as_str().to_owned()));
        }

        create_layout(&dir)?;
        let id = WorkspaceId::parse(&uuid::Uuid::new_v4().to_string())
            .map_err(|err| WorkspaceError::marker(dir.join(MARKER_FILE), err.to_string()))?;
        write_marker(
            &dir,
            &MarkerToml {
                schema: MARKER_SCHEMA,
                id: id.as_str().to_owned(),
                name: name.as_str().to_owned(),
                created_at: now,
            },
        )?;

        let is_first = self.repo.list()?.is_empty();
        let ws = Workspace {
            id,
            name,
            path: dir,
            is_current: is_first,
            created_at: now,
            updated_at: now,
        };
        self.repo.create(&ws)?;
        Ok(InitOutcome::Created(ws))
    }

    /// 全部工作目录，按名称升序
    ///
    /// # Errors
    ///
    /// 注册表失败时返回 [`WorkspaceError`]
    pub fn list(&self) -> Result<Vec<Workspace>, WorkspaceError> {
        Ok(self
            .repo
            .list()?
            .into_iter()
            .map(normalize_workspace)
            .collect())
    }

    /// 当前工作区：`create` / `link` 等缺省操作的落点
    ///
    /// # Errors
    ///
    /// 无当前或注册表失败时返回 [`WorkspaceError`]
    pub fn current_workspace(&self) -> Result<Workspace, WorkspaceError> {
        self.repo
            .current()?
            .map(normalize_workspace)
            .ok_or(WorkspaceError::NoCurrent)
    }

    /// 按名选择器解析：精确 → 唯一前缀 → 报错
    ///
    /// # Errors
    ///
    /// 未找到 / 歧义 / 注册表失败时返回 [`WorkspaceError`]
    pub fn resolve(&self, selector: &str) -> Result<Workspace, WorkspaceError> {
        let all = self.list()?;
        resolve_selector(&all, selector)
    }

    /// 切换当前工作区
    ///
    /// # Errors
    ///
    /// 不存在或注册表失败时返回 [`WorkspaceError`]
    pub fn set_current(&self, selector: &str) -> Result<Workspace, WorkspaceError> {
        let ws = self.resolve(selector)?;
        self.repo.set_current(&ws.id)?;
        let mut ws = ws;
        ws.is_current = true;
        Ok(ws)
    }

    /// 注销工作目录；`purge` 时一并删除磁盘目录
    ///
    /// 顺序：先删数据再注销，再清服务器登记。服务器外键引用工作目录行，
    /// 必须先 [`ServerRepository::remove_by_workspace`] 清掉服务器登记、
    /// 再注销工作目录，否则注册表删除报 `FOREIGN KEY constraint failed`。
    /// 删除失败（常见于目录被占用）时注册记录还在，退出占用后重试即可；
    /// 反过来会留下「记录已注销、目录只删了一半」的中间态。
    ///
    /// 被删的是当前工作区时，当前标记移交给剩余中按名称排序的第一个；
    /// 无剩余则当前为空，后续缺省落点操作报 [`WorkspaceError::NoCurrent`]。
    ///
    /// # Errors
    ///
    /// 不存在、IO 或注册表失败时返回 [`WorkspaceError`]
    pub fn remove(&self, selector: &str, purge: bool) -> Result<Workspace, WorkspaceError> {
        let ws = self.resolve(selector)?;
        if purge && ws.path.exists() {
            std::fs::remove_dir_all(&ws.path)
                .map_err(|err| WorkspaceError::io(&ws.path, deletion_hint(err)))?;
        }
        // 服务器登记引用工作目录（外键）：先清服务器，再注销工作目录
        self.servers.remove_by_workspace(&ws.id)?;
        self.repo.remove(&ws.id)?;
        if ws.is_current
            && let Some(next) = self.repo.list()?.first()
        {
            self.repo.set_current(&next.id)?;
        }
        Ok(ws)
    }

    fn recover(
        &self,
        dir: PathBuf,
        marker: &MarkerToml,
        now: i64,
    ) -> Result<InitOutcome, WorkspaceError> {
        let marker_path = dir.join(MARKER_FILE);
        let id = WorkspaceId::parse(&marker.id)
            .map_err(|err| WorkspaceError::marker(marker_path.clone(), err.to_string()))?;
        let name = WorkspaceName::parse(&marker.name)
            .map_err(|err| WorkspaceError::marker(marker_path, err.to_string()))?;

        if let Ok(existing) = self.repo.get(&id) {
            return Ok(InitOutcome::AlreadyRegistered(normalize_workspace(
                existing,
            )));
        }

        let ws = Workspace {
            id,
            name,
            path: dir,
            is_current: self.repo.list()?.is_empty(),
            created_at: marker.created_at,
            updated_at: now,
        };
        self.repo.create(&ws)?;
        Ok(InitOutcome::Recovered(ws))
    }

    /// 下一个默认工作区名：现存最大序号 + 1（`workspace01`、`workspace02`…）
    ///
    /// # Errors
    ///
    /// 注册表失败或生成名非法时返回 [`WorkspaceError`]
    pub fn next_default_name(&self) -> Result<WorkspaceName, WorkspaceError> {
        let max = self
            .repo
            .list()?
            .iter()
            .filter_map(|w| {
                w.name
                    .as_str()
                    .strip_prefix("workspace")
                    .and_then(|n| n.parse::<u32>().ok())
            })
            .max()
            .unwrap_or(0);
        let candidate = format!("workspace{:02}", max + 1);
        Ok(WorkspaceName::parse(&candidate)?)
    }

    fn name_taken(&self, name: &WorkspaceName) -> bool {
        self.repo.get_by_name(name).is_ok()
    }
}

/// `workspace doctor` 单项检查结果
#[derive(Debug, Clone)]
pub struct DoctorFinding {
    /// 严重程度：error 非零退出，warning 仅提示
    pub level: DoctorLevel,
    /// 所属工作区（全局检查可为空）
    pub workspace: Option<String>,
    /// 人类可读的问题描述
    pub message: String,
}

/// doctor 检查级别
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoctorLevel {
    /// 需要处理的问题，退出码非零
    Error,
    /// 提示性问题，退出码零
    Warning,
}

/// 按名选择器从候选表里解析工作区：精确 → 唯一前缀 → 报错
///
/// `WorkspaceService::resolve` 与 `ServerRegistry::resolve_workspace` 共用，
/// 选择器语义只允许有一份实现。
///
/// # Errors
///
/// 未找到 / 歧义时返回 [`WorkspaceError`]
pub(crate) fn resolve_selector(
    all: &[Workspace],
    selector: &str,
) -> Result<Workspace, WorkspaceError> {
    if let Some(exact) = all.iter().find(|w| w.name.as_str() == selector) {
        return Ok(exact.clone());
    }
    let matched: Vec<&Workspace> = all
        .iter()
        .filter(|w| w.name.as_str().starts_with(selector))
        .collect();
    match matched.as_slice() {
        [one] => Ok((*one).clone()),
        [] => Err(WorkspaceError::NotFound(selector.to_owned())),
        many => Err(WorkspaceError::Ambiguous {
            selector: selector.to_owned(),
            candidates: many.iter().map(|w| w.name.as_str().to_owned()).collect(),
        }),
    }
}

fn create_layout(dir: &Path) -> Result<(), WorkspaceError> {
    for sub in ["servers", "backups/cas", "backups/snapshots", "cache"] {
        let path = dir.join(sub);
        std::fs::create_dir_all(&path).map_err(|err| WorkspaceError::io(&path, err))?;
    }
    Ok(())
}

fn read_marker(dir: &Path) -> Result<Option<MarkerToml>, WorkspaceError> {
    let path = dir.join(MARKER_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path).map_err(|err| WorkspaceError::io(&path, err))?;
    let marker: MarkerToml = toml::from_str(&text)
        .map_err(|err| WorkspaceError::marker(path.clone(), err.to_string()))?;
    if marker.schema != MARKER_SCHEMA {
        return Err(WorkspaceError::marker(
            path,
            format!("unsupported schema {}", marker.schema),
        ));
    }
    Ok(Some(marker))
}

fn write_marker(dir: &Path, marker: &MarkerToml) -> Result<(), WorkspaceError> {
    let path = dir.join(MARKER_FILE);
    let text = toml::to_string_pretty(marker)
        .map_err(|err| WorkspaceError::marker(path.clone(), err.to_string()))?;
    std::fs::write(&path, text).map_err(|err| WorkspaceError::io(&path, err))
}

fn is_empty_dir(dir: &Path) -> Result<bool, WorkspaceError> {
    let entries = std::fs::read_dir(dir).map_err(|err| WorkspaceError::io(dir, err))?;
    for entry in entries {
        let entry = entry.map_err(|err| WorkspaceError::io(dir, err))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !NOISE_ENTRIES.contains(&name.as_ref()) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// 把用户给的目录解析为入库口径的绝对路径
///
/// 相对路径按当前工作目录展开，再去掉 `.` 组件与多余分隔符。展示与入库
/// 必须走同一口径：入口侧确认页预览也调用本函数，不复制这份解析。
#[must_use]
pub fn absolute_path(dir: &Path) -> PathBuf {
    let joined = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(dir)
    };
    normalize_path(&joined)
}

/// 去掉 `.` 组件与多余分隔符
///
/// 不规范化就会把 `D:\ws\.` 之类路径入库：展示难看，同一目录也可能被当成两个身份
fn normalize_path(path: &Path) -> PathBuf {
    path.components().collect()
}

/// 读出注册表记录时规范化路径：库里可能存着历史写入的未规范化路径
fn normalize_workspace(mut ws: Workspace) -> Workspace {
    ws.path = normalize_path(&ws.path);
    ws
}

/// Windows 上目录被占用时 `remove_dir_all` 报 `ERROR_SHARING_VIOLATION`（os error 32）
///
/// 常见于有终端把该目录当工作目录、资源管理器开着它。补一句可操作提示，
/// 否则用户只看到一个 os error 32，不知道该干什么。
#[cfg(windows)]
fn deletion_hint(err: std::io::Error) -> std::io::Error {
    const ERROR_SHARING_VIOLATION: i32 = 32;
    if err.raw_os_error() == Some(ERROR_SHARING_VIOLATION) {
        return std::io::Error::other(format!(
            "{err}（目录正被占用：常见于终端的工作目录就在其中，或资源管理器/其他程序打开着；退出占用后重试）"
        ));
    }
    err
}

/// 非 Windows 没有这个错误码
#[cfg(not(windows))]
fn deletion_hint(err: std::io::Error) -> std::io::Error {
    err
}

#[cfg(test)]
mod tests {
    use super::*;
    use port::WorkspaceRepository;
    use std::path::Path;

    struct MemRepo {
        items: std::sync::Mutex<Vec<Workspace>>,
    }

    impl MemRepo {
        fn new() -> Self {
            Self {
                items: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl WorkspaceRepository for MemRepo {
        fn create(&self, ws: &Workspace) -> Result<(), RepositoryError> {
            let mut items = self.items.lock().expect("lock");
            if items
                .iter()
                .any(|w| w.name == ws.name || w.path == ws.path || w.id == ws.id)
            {
                return Err(RepositoryError::AlreadyExists(ws.name.as_str().to_owned()));
            }
            if ws.is_current {
                for w in items.iter_mut() {
                    w.is_current = false;
                }
            }
            items.push(ws.clone());
            Ok(())
        }
        fn get(&self, id: &WorkspaceId) -> Result<Workspace, RepositoryError> {
            self.items
                .lock()
                .expect("lock")
                .iter()
                .find(|w| &w.id == id)
                .cloned()
                .ok_or_else(|| RepositoryError::NotFound(id.as_str().to_owned()))
        }
        fn get_by_name(&self, name: &WorkspaceName) -> Result<Workspace, RepositoryError> {
            self.items
                .lock()
                .expect("lock")
                .iter()
                .find(|w| &w.name == name)
                .cloned()
                .ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))
        }
        fn list(&self) -> Result<Vec<Workspace>, RepositoryError> {
            let mut items = self.items.lock().expect("lock").clone();
            items.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(items)
        }
        fn current(&self) -> Result<Option<Workspace>, RepositoryError> {
            Ok(self
                .items
                .lock()
                .expect("lock")
                .iter()
                .find(|w| w.is_current)
                .cloned())
        }
        fn set_current(&self, id: &WorkspaceId) -> Result<(), RepositoryError> {
            let mut items = self.items.lock().expect("lock");
            if !items.iter().any(|w| &w.id == id) {
                return Err(RepositoryError::NotFound(id.as_str().to_owned()));
            }
            for w in items.iter_mut() {
                w.is_current = &w.id == id;
            }
            Ok(())
        }
        fn remove(&self, id: &WorkspaceId) -> Result<(), RepositoryError> {
            let mut items = self.items.lock().expect("lock");
            let before = items.len();
            items.retain(|w| &w.id != id);
            if items.len() == before {
                return Err(RepositoryError::NotFound(id.as_str().to_owned()));
            }
            Ok(())
        }
        fn relink_path(
            &self,
            id: &WorkspaceId,
            path: &Path,
            _updated_at: i64,
        ) -> Result<(), RepositoryError> {
            let mut items = self.items.lock().expect("lock");
            let ws = items
                .iter_mut()
                .find(|w| &w.id == id)
                .ok_or_else(|| RepositoryError::NotFound(id.as_str().to_owned()))?;
            ws.path = path.to_path_buf();
            Ok(())
        }
    }

    /// 服务器登记假件：记录注销工作目录时是否被清（外键协作的回归点）
    struct MemServers {
        cleared: std::sync::atomic::AtomicBool,
    }

    impl MemServers {
        fn new() -> Self {
            Self {
                cleared: std::sync::atomic::AtomicBool::new(false),
            }
        }
    }

    impl ServerRepository for MemServers {
        fn create(&self, _meta: &domain::server::meta::ServerMeta) -> Result<(), RepositoryError> {
            unreachable!("not used in tests")
        }
        fn get(
            &self,
            _name: &domain::server::meta::ServerName,
        ) -> Result<domain::server::meta::ServerMeta, RepositoryError> {
            unreachable!("not used in tests")
        }
        fn list(&self) -> Result<Vec<domain::server::meta::ServerMeta>, RepositoryError> {
            Ok(Vec::new())
        }
        fn update_jar(
            &self,
            _name: &domain::server::meta::ServerName,
            _jar: &domain::server::meta::InstalledJar,
            _updated_at: i64,
        ) -> Result<domain::server::meta::ServerMeta, RepositoryError> {
            unreachable!("not used in tests")
        }
        fn remove(&self, _name: &domain::server::meta::ServerName) -> Result<(), RepositoryError> {
            unreachable!("not used in tests")
        }
        fn remove_by_workspace(&self, _workspace_id: &WorkspaceId) -> Result<(), RepositoryError> {
            self.cleared
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        fn relink(
            &self,
            _name: &domain::server::meta::ServerName,
            _workspace_id: &WorkspaceId,
            _rel_dir: &Path,
            _updated_at: i64,
        ) -> Result<domain::server::meta::ServerMeta, RepositoryError> {
            unreachable!("not used in tests")
        }
    }

    fn svc() -> (WorkspaceService, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tmp");
        let repo = Arc::new(MemRepo::new());
        let servers = Arc::new(MemServers::new());
        (WorkspaceService::new(repo, servers), tmp)
    }

    /// 服务器登记以工作目录为外键：注销前必须先清，否则注册表删除报
    /// `FOREIGN KEY constraint failed`（真实外键见 storage 集成测试）
    #[test]
    fn remove_clears_server_registrations_first() {
        let repo = Arc::new(MemRepo::new());
        let servers = Arc::new(MemServers::new());
        let svc = WorkspaceService::new(repo.clone(), servers.clone());
        let tmp = tempfile::tempdir().expect("tmp");
        svc.init(&tmp.path().join("ws"), Some("ws01"))
            .expect("init");

        svc.remove("ws01", false).expect("remove");

        assert!(servers.cleared.load(std::sync::atomic::Ordering::SeqCst));
        assert!(svc.resolve("ws01").is_err());
    }

    #[test]
    fn init_creates_layout_marker_and_first_current() {
        let (svc, tmp) = svc();
        let dir = tmp.path().join("ws");
        let InitOutcome::Created(ws) = svc.init(&dir, None).expect("init") else {
            unreachable!("expected Created")
        };
        assert!(ws.is_current);
        assert_eq!(ws.name.as_str(), "workspace01");
        assert!(dir.join("servers").is_dir());
        assert!(dir.join("backups/cas").is_dir());
        assert!(dir.join("workspace.toml").is_file());
    }

    /// 带 `.` 的路径入库前必须规范化，否则库里会留下 `D:\ws\.` 这种形态
    #[test]
    fn init_normalizes_dotted_path() {
        let (svc, tmp) = svc();
        let dir = tmp.path().join("ws").join(".");
        let InitOutcome::Created(ws) = svc.init(&dir, Some("ws01")).expect("init") else {
            unreachable!("expected Created")
        };
        assert_eq!(ws.path, tmp.path().join("ws"));
        assert_eq!(svc.resolve("ws01").expect("resolve").path, ws.path);
    }

    /// 历史数据可能带 `.` 组件，读出时统一规范化，同一目录不能有两种身份
    #[test]
    fn list_normalizes_legacy_dotted_path() {
        let repo = Arc::new(MemRepo::new());
        let servers = Arc::new(MemServers::new());
        let svc = WorkspaceService::new(repo.clone(), servers);
        let tmp = tempfile::tempdir().expect("tmp");
        let InitOutcome::Created(ws) = svc
            .init(&tmp.path().join("ws"), Some("ws01"))
            .expect("init")
        else {
            unreachable!("expected Created")
        };
        {
            let mut items = repo.items.lock().expect("lock");
            items[0].path = items[0].path.join(".");
        }

        assert_eq!(svc.list().expect("list")[0].path, ws.path);
        assert_eq!(svc.resolve("ws01").expect("resolve").path, ws.path);
    }

    #[test]
    fn init_rejects_non_empty_dir() {
        let (svc, tmp) = svc();
        let dir = tmp.path().join("full");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("keep.txt"), "x").expect("file");
        let err = svc.init(&dir, Some("ws")).expect_err("reject");
        assert!(matches!(err, WorkspaceError::NotEmpty(_)));
    }

    #[test]
    fn init_ignores_noise_files() {
        let (svc, tmp) = svc();
        let dir = tmp.path().join("noisy");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(".DS_Store"), "x").expect("noise");
        std::fs::write(dir.join("Thumbs.db"), "x").expect("noise");
        assert!(svc.init(&dir, Some("ws01")).is_ok());
    }

    #[test]
    fn second_workspace_is_not_current() {
        let (svc, tmp) = svc();
        svc.init(&tmp.path().join("a"), None).expect("a");
        let InitOutcome::Created(b) = svc.init(&tmp.path().join("b"), Some("ws02")).expect("b")
        else {
            unreachable!("expected Created")
        };
        assert!(!b.is_current);
        let cur = svc.current_workspace().expect("current");
        assert_eq!(cur.name.as_str(), "workspace01");
    }

    #[test]
    fn default_name_sequence_skips_deleted() {
        let (svc, tmp) = svc();
        svc.init(&tmp.path().join("a"), None).expect("a"); // workspace01
        svc.init(&tmp.path().join("b"), Some("workspace02"))
            .expect("b");
        svc.remove("workspace01", false).expect("remove");
        let InitOutcome::Created(c) = svc.init(&tmp.path().join("c"), None).expect("c") else {
            unreachable!("expected Created")
        };
        // 现存最大序号 = 2 → 新建 workspace03
        assert_eq!(c.name.as_str(), "workspace03");
    }

    #[test]
    fn init_recovers_interrupted_marker() {
        let (svc, tmp) = svc();
        let dir = tmp.path().join("half");
        std::fs::create_dir_all(dir.join("servers")).expect("mkdir");
        let id = "550e8400-e29b-41d4-a716-4466554400aa";
        let marker =
            format!("schema = 1\nid = \"{id}\"\nname = \"half\"\ncreated_at = 1700000000\n");
        std::fs::write(dir.join("workspace.toml"), marker).expect("marker");
        let outcome = svc.init(&dir, None).expect("recover");
        let InitOutcome::Recovered(ws) = outcome else {
            unreachable!("expected Recovered, got {outcome:?}")
        };
        assert_eq!(ws.name.as_str(), "half");
        assert!(ws.is_current);
    }

    #[test]
    fn init_duplicate_name_rejected() {
        let (svc, tmp) = svc();
        svc.init(&tmp.path().join("a"), Some("ws01")).expect("a");
        let err = svc
            .init(&tmp.path().join("b"), Some("ws01"))
            .expect_err("dup");
        assert!(matches!(err, WorkspaceError::AlreadyExists(_)));
    }

    #[test]
    fn init_already_registered_is_idempotent() {
        let (svc, tmp) = svc();
        let dir = tmp.path().join("ws");
        svc.init(&dir, Some("ws01")).expect("first");
        let outcome = svc.init(&dir, Some("ws01")).expect("second");
        let InitOutcome::AlreadyRegistered(ws) = outcome else {
            unreachable!("expected AlreadyRegistered")
        };
        assert_eq!(ws.name.as_str(), "ws01");
    }

    #[test]
    fn resolve_exact_then_unique_prefix() {
        let (svc, tmp) = svc();
        svc.init(&tmp.path().join("a"), Some("alpha")).expect("a");
        svc.init(&tmp.path().join("b"), Some("alp-01")).expect("b");
        assert_eq!(svc.resolve("alpha").expect("exact").name.as_str(), "alpha");
        assert_eq!(svc.resolve("alp-").expect("prefix").name.as_str(), "alp-01");
        assert!(matches!(
            svc.resolve("alp"),
            Err(WorkspaceError::Ambiguous { .. })
        ));
        assert!(matches!(
            svc.resolve("zzz"),
            Err(WorkspaceError::NotFound(_))
        ));
    }

    #[test]
    fn set_current_switches() {
        let (svc, tmp) = svc();
        svc.init(&tmp.path().join("a"), Some("ws01")).expect("a");
        svc.init(&tmp.path().join("b"), Some("ws02")).expect("b");
        let ws = svc.set_current("ws02").expect("use");
        assert!(ws.is_current);
        assert_eq!(svc.current_workspace().expect("cur").name.as_str(), "ws02");
    }

    #[test]
    fn remove_without_purge_keeps_files() {
        let (svc, tmp) = svc();
        let dir = tmp.path().join("ws");
        svc.init(&dir, Some("ws01")).expect("init");
        svc.remove("ws01", false).expect("remove");
        assert!(dir.join("workspace.toml").is_file());
    }

    /// 删除当前工作区时，当前标记必须移交给剩余中按名称排序的第一个
    #[test]
    fn remove_current_hands_over_to_next() {
        let (svc, tmp) = svc();
        svc.init(&tmp.path().join("a"), Some("ws01")).expect("a");
        svc.init(&tmp.path().join("b"), Some("ws02")).expect("b");
        svc.init(&tmp.path().join("c"), Some("ws03")).expect("c");
        assert_eq!(svc.current_workspace().expect("cur").name.as_str(), "ws01");

        svc.remove("ws01", false).expect("remove");

        assert_eq!(svc.current_workspace().expect("cur").name.as_str(), "ws02");
    }

    /// 最后一个工作区被删后无当前：缺省落点操作报 `NoCurrent`
    #[test]
    fn remove_last_workspace_clears_current() {
        let (svc, tmp) = svc();
        svc.init(&tmp.path().join("a"), Some("ws01")).expect("a");
        svc.remove("ws01", false).expect("remove");
        assert!(matches!(
            svc.current_workspace().expect_err("none"),
            WorkspaceError::NoCurrent
        ));
    }

    #[test]
    fn remove_with_purge_deletes_dir() {
        let (svc, tmp) = svc();
        let dir = tmp.path().join("ws");
        svc.init(&dir, Some("ws01")).expect("init");
        svc.remove("ws01", true).expect("remove");
        assert!(!dir.exists());
    }

    /// 删除失败时必须保留注册记录：否则「记录已注销、目录只删了一半」无从重试
    #[test]
    fn purge_failure_keeps_registration() {
        let (svc, tmp) = svc();
        let dir = tmp.path().join("ws");
        svc.init(&dir, Some("ws01")).expect("init");
        // 把目录换成同名文件：remove_dir_all 在各平台都会失败
        std::fs::remove_dir_all(&dir).expect("rmdir");
        std::fs::write(&dir, "occupied").expect("file");

        let err = svc.remove("ws01", true).expect_err("must fail");
        assert!(matches!(err, WorkspaceError::Fs(FsError::Io { .. })));
        // 记录还在，占用解除后重试即可自愈
        assert!(svc.resolve("ws01").is_ok());
    }
}
