use crate::fs_error::{FsError, deletion_hint};
use domain::workspace::{InvalidWorkspaceName, Workspace, WorkspaceId, WorkspaceName};
use port::{RepositoryError, ServerRepository, Sourced, WorkspaceRepository};
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
    /// 当前进程的工作目录位于待删目录内：先删它会把调用方终端锁死在已删除路径上
    #[error(
        "current working directory {cwd} is inside {target}; move out of it first, \
         then retry"
    )]
    CwdInsideTarget {
        /// 当前工作目录
        cwd: PathBuf,
        /// 待删除目录
        target: PathBuf,
    },
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

    /// 面向用户展示的分类；结构化错误不带文案，入口按此渲染提示
    ///
    /// 包装类变体（[`Self::Fs`] / [`Self::Repo`] / [`Self::InvalidName`]）各有
    /// 自己的 `Display`，此处返回 `None` 让入口继续沿错误链找。
    #[must_use]
    pub fn kind(&self) -> Option<WorkspaceErrorKind<'_>> {
        let kind = match self {
            Self::Fs(_) | Self::Repo(_) | Self::InvalidName(_) => return None,
            Self::NotEmpty(path) => WorkspaceErrorKind::NotEmpty { path },
            Self::NotAWorkspace(path) => WorkspaceErrorKind::NotAWorkspace { path },
            Self::AlreadyExists(name) => WorkspaceErrorKind::AlreadyExists { name },
            Self::NotFound(name) => WorkspaceErrorKind::NotFound { name },
            Self::Ambiguous {
                selector,
                candidates,
            } => WorkspaceErrorKind::Ambiguous {
                selector,
                candidates,
            },
            Self::NoCurrent => WorkspaceErrorKind::NoCurrent,
            Self::CwdInsideTarget { cwd, target } => {
                WorkspaceErrorKind::CwdInsideTarget { cwd, target }
            }
        };
        Some(kind)
    }
}

/// [`WorkspaceError::kind`] 的结果：只带判定与参数，文案由入口渲染
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum WorkspaceErrorKind<'a> {
    /// 目标目录非空
    NotEmpty {
        /// 目标路径
        path: &'a Path,
    },
    /// 目录缺少 `workspace.toml`
    NotAWorkspace {
        /// 目标路径
        path: &'a Path,
    },
    /// 名称或路径冲突
    AlreadyExists {
        /// 冲突的名称或路径
        name: &'a str,
    },
    /// 工作目录不存在
    NotFound {
        /// 名称或路径
        name: &'a str,
    },
    /// 前缀匹配到多个工作目录
    Ambiguous {
        /// 用户给的选择器
        selector: &'a str,
        /// 候选名称
        candidates: &'a [String],
    },
    /// 尚无当前工作区
    NoCurrent,
    /// 调用方工作目录位于待删目录内
    CwdInsideTarget {
        /// 调用方工作目录
        cwd: &'a Path,
        /// 待删目录
        target: &'a Path,
    },
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
    /// 目录已登记但 `workspace.toml` 丢失：按记录里的 id / 名称补写 marker
    MarkerRestored(Workspace),
}

/// 注销结果：注销了哪条工作目录，以及磁盘是否真的删了
#[derive(Debug, Clone)]
pub struct RemoveOutcome {
    /// 被注销的工作目录（注销前的快照）
    pub workspace: Workspace,
    /// 磁盘目录是否被本次调用实际删除
    ///
    /// `false` 表示本次没有删除动作：未带 `purge`，或目标目录本就不存在。
    /// 入口据此报「已删除」与「磁盘数据本就不在」的区别。
    pub disk_purged: bool,
}

impl RemoveOutcome {
    /// 磁盘目录是否真的被本次调用删掉了
    ///
    /// 与 `purge` 请求参数不同：未带 `purge` 或目标本就不存在时为 `false`。
    #[must_use]
    pub fn disk_purged(&self) -> bool {
        self.disk_purged
    }
}

impl WorkspaceService {
    /// 删盘前的前置校验：该工作区现在能否执行 `remove` 的删盘部分
    ///
    /// 只做判定、不改任何状态，供入口在弹「删除磁盘数据?」之前调用：
    /// 交互入口先问再拒，用户会在确认框与手输名称之后才看到拒绝原因，白走一遍。
    /// 判定与 [`WorkspaceService::remove`] 共用同一份 cwd 包含关系检查，不会分叉。
    ///
    /// # Errors
    ///
    /// 不存在、目标不是该工作区目录（与 `remove` 的删盘分支同一判定）、
    /// `cwd` 在目标内时返回 [`WorkspaceError`]
    pub fn ensure_purgeable(&self, selector: &str, cwd: &Path) -> Result<(), WorkspaceError> {
        let ws = self.resolve(selector)?;
        let cwd = normalize_path(&absolute_path(cwd));
        // `remove` 在目录已不存在时跳过删盘，这里同样不必拦
        if !ws.path.exists() {
            return Ok(());
        }
        ensure_outside(&cwd, &ws.path)?;
        if !is_deletable(&ws) {
            return Err(WorkspaceError::NotAWorkspace(ws.path.clone()));
        }
        Ok(())
    }
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

        // 目录指向一条已登记记录、但没有 marker：身份由记录给出，补写 marker。
        // 不接管未登记的目录——那需要用户确认归属，判空闸继续拦
        if let Some(ws) = self.registered_at(&dir)? {
            create_layout(&dir)?;
            write_marker(
                &dir,
                &MarkerToml {
                    schema: MARKER_SCHEMA,
                    id: ws.id.as_str().to_owned(),
                    name: ws.name.as_str().to_owned(),
                    created_at: ws.created_at,
                },
            )?;
            return Ok(InitOutcome::MarkerRestored(ws));
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

    /// 全部工作目录并标注来源（本地 + 已配对远端）
    ///
    /// 组合仓库下每条带真实来源；纯本地实现全部是 [`port::Origin::Local`]。
    /// 入口用它在合并列表里显示 `ORIGIN`，并按 `--origin` 过滤。
    ///
    /// # Errors
    ///
    /// 注册表失败时返回 [`WorkspaceError`]
    pub fn list_sourced(&self) -> Result<Vec<Sourced<Workspace>>, WorkspaceError> {
        Ok(self
            .repo
            .list_sourced()?
            .into_iter()
            .map(|sourced| sourced.map(normalize_workspace))
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
    /// `cwd` 是调用方进程的工作目录。它在待删目录内时直接拒绝
    /// （[`WorkspaceError::CwdInsideTarget`]）：Windows 上子进程无法改变父进程
    /// 的工作目录，删掉之后调用方终端会停在一条已不存在的路径上，之后任何
    /// 子进程（包括 mcsup 自己）都拿不到工作目录，表现为命令静默不执行。
    ///
    /// # Errors
    ///
    /// 不存在、目标不是该工作区目录、`cwd` 在目标内、IO 或注册表失败时
    /// 返回 [`WorkspaceError`]
    pub fn remove(
        &self,
        selector: &str,
        purge: bool,
        cwd: &Path,
    ) -> Result<RemoveOutcome, WorkspaceError> {
        let ws = self.resolve(selector)?;
        let mut disk_purged = false;
        if purge && ws.path.exists() {
            ensure_outside(cwd, &ws.path)?;
            if !is_deletable(&ws) {
                return Err(WorkspaceError::NotAWorkspace(ws.path.clone()));
            }
            std::fs::remove_dir_all(&ws.path)
                .map_err(|err| WorkspaceError::io(&ws.path, deletion_hint(err)))?;
            disk_purged = true;
        }
        // 服务器登记引用工作目录（外键）：先清服务器，再注销工作目录
        self.servers.remove_by_workspace(&ws.id)?;
        self.repo.remove(&ws.id)?;
        if ws.is_current
            && let Some(next) = self.repo.list()?.first()
        {
            self.repo.set_current(&next.id)?;
        }
        Ok(RemoveOutcome {
            workspace: ws,
            disk_purged,
        })
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

    /// 该路径已登记的工作目录
    ///
    /// 路径按入库口径（[`absolute_path`]）比较：记录里存的就是绝对路径，
    /// 这里的 `dir` 也已绝对化，直接相等即同一目录。
    ///
    /// # Errors
    ///
    /// 注册表读取失败时返回错误；未登记返回 `Ok(None)`
    fn registered_at(&self, dir: &Path) -> Result<Option<Workspace>, WorkspaceError> {
        Ok(self
            .repo
            .list()?
            .into_iter()
            .map(normalize_workspace)
            .find(|ws| ws.path == dir))
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

/// `workspace.toml` 的读取结论：marker 是否存在、是否指向这条记录
///
/// `doctor` 需要区分「没有 marker」「marker 属于别的目录」「marker 读不懂」：
/// 前者说明目录身份不成立，后者说明文件损坏，处理方式不同。
pub(crate) enum MarkerState {
    /// marker 存在且 `id` 与记录一致
    Match,
    /// 目录里没有 marker
    Missing,
    /// marker 存在但 `id` 与记录不一致（携带 marker 里的 id）
    Mismatch(String),
    /// marker 无法解析或 schema 不支持
    Corrupt,
}

/// 读 marker 并给出身份结论；IO 失败按 `Corrupt` 处理（读不出来就无法确认身份）
pub(crate) fn read_marker_id(dir: &Path, expected: &WorkspaceId) -> MarkerState {
    match read_marker(dir) {
        Ok(Some(marker)) => marker_state(&marker, expected),
        Ok(None) => MarkerState::Missing,
        Err(_) => MarkerState::Corrupt,
    }
}

/// marker → 身份结论（已解析的 marker，供 init 复用，避免读两遍）
fn marker_state(marker: &MarkerToml, expected: &WorkspaceId) -> MarkerState {
    if marker.id == expected.as_str() {
        MarkerState::Match
    } else {
        MarkerState::Mismatch(marker.id.clone())
    }
}

/// 按名选择器从候选表里解析工作区：精确 → 唯一前缀 → 报错
///
/// `WorkspaceService::resolve` 与 `ServerRegistry::resolve_workspace` 共用，
/// 选择器语义只允许有一份实现。
///
/// # Errors
///
/// 未找到 / 歧义时返回 [`WorkspaceError`]
pub fn resolve_selector(all: &[Workspace], selector: &str) -> Result<Workspace, WorkspaceError> {
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

/// 目录是否确实是这条记录对应的工作目录（`--purge` 前置闸）
///
/// 记录里的 `path` 是唯一决定「删哪里」的字段。它被手改成 `/`、另一个磁盘
/// 根或家目录时，`remove_dir_all` 会照删不误。删除前必须拿到文件系统上的
/// 独立证据：目录里的 `workspace.toml` 与该记录同 id。
///
/// marker 缺失 / 损坏时不放行——记录对不上磁盘时宁可拒绝删除，
/// 让用户确认后手工处理，也不拿整个目录赌一次。
fn is_deletable(ws: &Workspace) -> bool {
    match read_marker(&ws.path) {
        Ok(Some(found)) => found.id == ws.id.as_str(),
        Ok(None) | Err(_) => false,
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

/// 待删目录是否包含 `cwd`：包含时拒绝删除
///
/// Windows 上子进程改不了父进程的工作目录，删掉当前目录后调用方终端会停在
/// 已不存在的路径上，之后任何子进程都拿不到工作目录（命令静默不执行）。
/// `workspace remove --purge` 与 `server remove --purge` 共用这一份判定。
///
/// # Errors
///
/// 比较前两侧都要换成物理路径：`current_dir()` 在 macOS 上返回真实路径
/// （`/private/var/...`），而入库的目录可能是符号链接形式（`/var/...`），
/// 逻辑路径直接比较会漏判。符号链接无法解析时退回逻辑规范化路径。
///
/// # Errors
///
/// `cwd` 规范化后位于 `target` 内时返回 [`WorkspaceError::CwdInsideTarget`]
pub(crate) fn ensure_outside(cwd: &Path, target: &Path) -> Result<(), WorkspaceError> {
    let cwd = normalize_path(&absolute_path(cwd));
    let target = normalize_path(&absolute_path(target));
    let same_space_cwd = physical_path(&cwd);
    let same_space_target = physical_path(&target);
    if same_space_cwd.starts_with(&same_space_target) {
        return Err(WorkspaceError::CwdInsideTarget { cwd, target });
    }
    Ok(())
}

/// 解析符号链接后的物理路径；不存在或不可解析时原样返回
fn physical_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
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
        fn update_core(
            &self,
            _name: &domain::server::meta::ServerName,
            _version: &domain::server::version::VersionId,
            _jar: &domain::server::meta::InstalledJar,
            _launch: Option<&domain::server::launch::LaunchSpec>,
            _required_java_major: Option<domain::java::JavaMajor>,
            _updated_at: i64,
        ) -> Result<domain::server::meta::ServerMeta, RepositoryError> {
            unreachable!("not used in tests")
        }
        fn update_settings(
            &self,
            _name: &domain::server::meta::ServerName,
            _settings: &domain::server::settings::ServerSettings,
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

        svc.remove("ws01", false, &tmp.path().join("elsewhere"))
            .expect("remove");

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
        svc.remove("workspace01", false, &tmp.path().join("elsewhere"))
            .expect("remove");
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

    /// 目录已登记但 marker 被删：按记录重建，沿用原 id 与名称
    ///
    /// 这是用户误删 `workspace.toml` 后的恢复路径：重建出的 marker 必须让
    /// `is_deletable` 通过，否则 `--purge` 永远被拒。
    #[test]
    fn init_restores_missing_marker_from_record() {
        let (svc, tmp) = svc();
        let dir = tmp.path().join("ws");
        svc.init(&dir, Some("ws01")).expect("init");
        let before = svc.resolve("ws01").expect("resolve");
        std::fs::remove_file(dir.join("workspace.toml")).expect("rm marker");

        let outcome = svc.init(&dir, None).expect("restore");
        let InitOutcome::MarkerRestored(ws) = outcome else {
            unreachable!("expected MarkerRestored, got {outcome:?}")
        };
        assert_eq!(ws.id, before.id, "id 必须沿用记录，否则身份对不上");
        assert_eq!(ws.name.as_str(), "ws01");
        assert!(dir.join("workspace.toml").is_file());

        // 重建后删除闸重新放行
        let outside = tmp.path().join("elsewhere");
        let out = svc.remove("ws01", true, &outside).expect("purge");
        assert!(out.disk_purged());
    }

    /// 未登记的目录不因 init 被接管：判空闸仍然拦着，用户得先确认归属
    #[test]
    fn init_still_rejects_unregistered_non_empty_dir() {
        let (svc, tmp) = svc();
        let dir = tmp.path().join("foreign");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("keep.txt"), "x").expect("file");

        let err = svc.init(&dir, None).expect_err("必须拒绝");
        assert!(matches!(err, WorkspaceError::NotEmpty(_)), "got {err:?}");
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
        svc.remove("ws01", false, &tmp.path().join("elsewhere"))
            .expect("remove");
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

        svc.remove("ws01", false, &tmp.path().join("elsewhere"))
            .expect("remove");

        assert_eq!(svc.current_workspace().expect("cur").name.as_str(), "ws02");
    }

    /// 最后一个工作区被删后无当前：缺省落点操作报 `NoCurrent`
    #[test]
    fn remove_last_workspace_clears_current() {
        let (svc, tmp) = svc();
        svc.init(&tmp.path().join("a"), Some("ws01")).expect("a");
        svc.remove("ws01", false, &tmp.path().join("elsewhere"))
            .expect("remove");
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
        let out = svc
            .remove("ws01", true, &tmp.path().join("elsewhere"))
            .expect("remove");
        assert!(!dir.exists());
        assert!(out.disk_purged(), "目录确实被删了，结果必须如实上报");
    }

    /// `--purge` 时目录本就不存在：注销照常成功，但不得上报成「删了磁盘」
    ///
    /// 上报错了用户会以为存档被删（实际根本没删），脚本也会误判磁盘状态。
    #[test]
    fn purge_reports_not_deleted_when_dir_missing() {
        let (svc, tmp) = svc();
        let dir = tmp.path().join("ws");
        svc.init(&dir, Some("ws01")).expect("init");
        std::fs::remove_dir_all(&dir).expect("rmdir");

        let out = svc
            .remove("ws01", true, &tmp.path().join("elsewhere"))
            .expect("remove");
        assert!(!out.disk_purged(), "没有可删的对象时必须是 false");
        assert!(svc.resolve("ws01").is_err(), "登记仍要清理");
    }

    /// 仅注销不上报删盘
    #[test]
    fn unregister_reports_not_deleted() {
        let (svc, tmp) = svc();
        let dir = tmp.path().join("ws");
        svc.init(&dir, Some("ws01")).expect("init");

        let out = svc
            .remove("ws01", false, &tmp.path().join("elsewhere"))
            .expect("remove");
        assert!(!out.disk_purged());
        assert!(dir.join("workspace.toml").is_file());
    }

    /// 调用方工作目录就在待删目录内时拒绝删除
    ///
    /// Windows 上子进程改不了父进程的工作目录：真删掉之后调用方终端会停在
    /// 已不存在的路径上，之后任何子进程都拿不到工作目录（命令静默不执行）。
    /// 拒绝时目录与注册记录都必须原样保留，用户 cd 出去后重试即可。
    #[test]
    fn purge_rejects_cwd_inside_target() {
        let (svc, tmp) = svc();
        let dir = tmp.path().join("ws");
        svc.init(&dir, Some("ws01")).expect("init");
        // 当前目录在目标之内（含目标本身与子目录两种深度）
        for cwd in [dir.clone(), dir.join("servers")] {
            let err = svc.remove("ws01", true, &cwd).expect_err("必须拒绝");
            let WorkspaceError::CwdInsideTarget { cwd: at, target } = &err else {
                panic!("unexpected error: {err:?}");
            };
            assert_eq!(at, &cwd);
            assert_eq!(target, &dir);
            assert!(dir.join("workspace.toml").is_file(), "目录不能被删");
            assert!(svc.resolve("ws01").is_ok(), "记录必须保留");
        }
    }

    /// 不在目标内的目录（前缀相同但不是子目录）不算包含：`.../ws2` 不能挡住 `.../ws`
    #[test]
    fn purge_allows_cwd_sharing_name_prefix() {
        let (svc, tmp) = svc();
        let dir = tmp.path().join("ws");
        svc.init(&dir, Some("ws01")).expect("init");
        let sibling = tmp.path().join("ws2");
        std::fs::create_dir_all(&sibling).expect("mkdir");

        svc.remove("ws01", true, &sibling).expect("remove");
        assert!(!dir.exists());
    }

    /// 目标经符号链接给出、cwd 是物理路径时同样要拒绝
    ///
    /// macOS 的 `/var` 指向 `/private/var`：临时目录登记成 `/var/...` 后，
    /// 子进程 `current_dir()` 拿到的是 `/private/var/...`。只比逻辑路径会
    /// 漏判，把正在使用的目录删掉。
    #[cfg(unix)]
    #[test]
    fn purge_rejects_cwd_inside_target_through_symlink() {
        let (svc, tmp) = svc();
        let real = tmp.path().join("real");
        let link = tmp.path().join("link");
        svc.init(&real, Some("ws01")).expect("init");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        std::fs::create_dir_all(real.join("servers")).expect("mkdir");
        let physical = std::fs::canonicalize(real.join("servers")).expect("physical");
        let err = svc.remove("ws01", true, &physical).expect_err("必须拒绝");
        assert!(
            matches!(err, WorkspaceError::CwdInsideTarget { .. }),
            "unexpected error: {err:?}"
        );
        assert!(real.join("workspace.toml").is_file(), "目录不能被删");
    }

    /// 删盘前置校验与 `remove` 判同一件事，且不改任何状态
    ///
    /// 入口靠它把「cwd 在待删目录内」提前到确认框之前报出来，因此它必须
    /// 在拒绝时什么都不动：目录、marker、注册记录都保持原样。
    #[test]
    fn ensure_purgeable_rejects_before_any_change() {
        let (svc, tmp) = svc();
        let dir = tmp.path().join("ws");
        svc.init(&dir, Some("ws01")).expect("init");

        svc.ensure_purgeable("ws01", &dir)
            .expect_err("cwd 在待删目录内必须拒绝");
        assert!(dir.join("workspace.toml").is_file(), "目录不能被删");
        assert!(svc.resolve("ws01").is_ok(), "记录必须保留");

        // 不在目标内时放行，同样不改状态
        svc.ensure_purgeable("ws01", tmp.path()).expect("放行");
        assert!(dir.join("workspace.toml").is_file());
        assert!(svc.resolve("ws01").is_ok());
    }

    /// 目录里没有本记录之外的证据时不放行，免得 `--purge` 猜一个目录删掉
    #[test]
    fn ensure_purgeable_rejects_marker_mismatch() {
        let (svc, tmp) = svc();
        let dir = tmp.path().join("ws");
        svc.init(&dir, Some("ws01")).expect("init");
        std::fs::remove_file(dir.join("workspace.toml")).expect("rm marker");

        let err = svc
            .ensure_purgeable("ws01", tmp.path())
            .expect_err("缺 marker 必须拒绝");
        assert!(
            matches!(err, WorkspaceError::NotAWorkspace { .. }),
            "{err:?}"
        );
        assert!(dir.is_dir());
    }

    /// 仅注销（不带 `--purge`）不删目录，工作目录在不在里面都放行
    #[test]
    fn unregister_ignores_cwd_location() {
        let (svc, tmp) = svc();
        let dir = tmp.path().join("ws");
        svc.init(&dir, Some("ws01")).expect("init");

        svc.remove("ws01", false, &dir).expect("remove");
        assert!(dir.join("workspace.toml").is_file());
    }

    /// 删除失败时必须保留注册记录：否则「记录已注销、目录只删了一半」无从重试
    #[test]
    fn purge_failure_keeps_registration() {
        let (svc, tmp) = svc();
        let dir = tmp.path().join("ws");
        svc.init(&dir, Some("ws01")).expect("init");
        // 换成同名普通文件：不是目录也不像工作区，删除闸先拒绝；记录必须保留，
        // 用户删除该文件后重试即可
        std::fs::remove_dir_all(&dir).expect("rmdir");
        std::fs::write(&dir, "occupied").expect("file");

        let err = svc
            .remove("ws01", true, &tmp.path().join("elsewhere"))
            .expect_err("must fail");
        assert!(
            matches!(err, WorkspaceError::Fs(FsError::Io { .. }))
                || matches!(err, WorkspaceError::NotAWorkspace(_)),
            "got {err:?}"
        );
        // 记录还在，占用解除后重试即可自愈
        assert!(svc.resolve("ws01").is_ok());
    }

    /// `path` 被改成别处（根目录 / 家目录等）时，`--purge` 不得跟着删
    ///
    /// 记录里的 path 是唯一决定删哪里的字段；目标目录没有对应 marker 时
    /// 必须拒绝，而不是把整棵目录树当真。
    #[test]
    fn purge_rejects_path_without_matching_marker() {
        let repo = Arc::new(MemRepo::new());
        let servers = Arc::new(MemServers::new());
        let svc = WorkspaceService::new(repo.clone(), servers);
        let tmp = tempfile::tempdir().expect("tmp");
        svc.init(&tmp.path().join("ws"), Some("ws01"))
            .expect("init");
        // 目录名对不上记录（模拟 path 被手改成某个别的目录）
        let foreign = tmp.path().join("other");
        std::fs::create_dir_all(&foreign).expect("mkdir");
        repo.items.lock().expect("lock")[0].path = foreign.clone();

        let err = svc
            .remove("ws01", true, &tmp.path().join("elsewhere"))
            .expect_err("必须拒绝");
        assert!(
            matches!(err, WorkspaceError::NotAWorkspace(_)),
            "got {err:?}"
        );
        assert!(foreign.is_dir(), "目标目录不能被删");
        assert!(svc.resolve("ws01").is_ok(), "记录必须保留");
    }

    /// `list_sourced` 在纯本地仓库上给每条标 `Origin::Local`
    #[test]
    fn list_sourced_marks_local_origin() {
        let tmp = tempfile::tempdir().expect("tmp");
        let repo = Arc::new(MemRepo::new());
        let servers: Arc<dyn ServerRepository> = Arc::new(MemServers::new());
        let svc = WorkspaceService::new(repo, servers);
        svc.init(tmp.path(), Some("alpha")).expect("init");
        let sourced = svc.list_sourced().expect("sourced");
        assert_eq!(sourced.len(), 1);
        assert!(sourced[0].origin.is_local());
        assert_eq!(sourced[0].value.name.as_str(), "alpha");
    }
}
