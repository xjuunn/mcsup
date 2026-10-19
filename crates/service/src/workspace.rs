use domain::workspace::{InvalidWorkspaceName, Workspace, WorkspaceId, WorkspaceName};
use port::{RepositoryError, WorkspaceRepository};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

const MARKER_FILE: &str = "workspace.toml";
const MARKER_SCHEMA: u32 = 1;
/// 噪声文件：判空时忽略（macOS / Windows 产物）
const NOISE_ENTRIES: &[&str] = &[".DS_Store", "Thumbs.db", "desktop.ini", ".localized"];

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
    /// 尚无默认工作目录
    #[error("no default workspace; run `mcsup workspace init` first")]
    NoDefault,
    /// 文件系统失败
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// marker 解析 / 序列化失败
    #[error("marker file {path}: {message}")]
    Marker { path: PathBuf, message: String },
    /// 注册表失败
    #[error(transparent)]
    Repo(#[from] RepositoryError),
    /// 名称非法
    #[error(transparent)]
    InvalidName(#[from] InvalidWorkspaceName),
}

impl WorkspaceError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
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
}

impl std::fmt::Debug for WorkspaceService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceService").finish_non_exhaustive()
    }
}

impl WorkspaceService {
    /// 组合注册表
    #[must_use]
    pub fn new(repo: Arc<dyn WorkspaceRepository>) -> Self {
        Self { repo }
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
        let now = now_unix();
        let dir = absolute(dir);

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
        let id = WorkspaceId::parse(&uuid::Uuid::new_v4().to_string()).map_err(|err| {
            WorkspaceError::Marker {
                path: dir.join(MARKER_FILE),
                message: err.to_string(),
            }
        })?;
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
            is_default: is_first,
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
        Ok(self.repo.list()?)
    }

    /// 默认工作目录
    ///
    /// # Errors
    ///
    /// 无默认或注册表失败时返回 [`WorkspaceError`]
    pub fn default_workspace(&self) -> Result<Workspace, WorkspaceError> {
        self.repo.default()?.ok_or(WorkspaceError::NoDefault)
    }

    /// 按名选择器解析：精确 → 唯一前缀 → 报错
    ///
    /// # Errors
    ///
    /// 未找到 / 歧义 / 注册表失败时返回 [`WorkspaceError`]
    pub fn resolve(&self, selector: &str) -> Result<Workspace, WorkspaceError> {
        let all = self.repo.list()?;
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

    /// 切换默认工作目录
    ///
    /// # Errors
    ///
    /// 不存在或注册表失败时返回 [`WorkspaceError`]
    pub fn set_default(&self, selector: &str) -> Result<Workspace, WorkspaceError> {
        let ws = self.resolve(selector)?;
        self.repo.set_default(&ws.id)?;
        let mut ws = ws;
        ws.is_default = true;
        Ok(ws)
    }

    /// 注销工作目录；`purge` 时一并删除磁盘目录
    ///
    /// # Errors
    ///
    /// 不存在、IO 或注册表失败时返回 [`WorkspaceError`]
    pub fn remove(&self, selector: &str, purge: bool) -> Result<Workspace, WorkspaceError> {
        let ws = self.resolve(selector)?;
        self.repo.remove(&ws.id)?;
        if purge && ws.path.exists() {
            std::fs::remove_dir_all(&ws.path).map_err(|err| WorkspaceError::io(&ws.path, err))?;
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
        let id = WorkspaceId::parse(&marker.id).map_err(|err| WorkspaceError::Marker {
            path: marker_path.clone(),
            message: err.to_string(),
        })?;
        let name = WorkspaceName::parse(&marker.name).map_err(|err| WorkspaceError::Marker {
            path: marker_path,
            message: err.to_string(),
        })?;

        if let Ok(existing) = self.repo.get(&id) {
            return Ok(InitOutcome::AlreadyRegistered(existing));
        }

        let ws = Workspace {
            id,
            name,
            path: dir,
            is_default: self.repo.list()?.is_empty(),
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
    let marker: MarkerToml = toml::from_str(&text).map_err(|err| WorkspaceError::Marker {
        path: path.clone(),
        message: err.to_string(),
    })?;
    if marker.schema != MARKER_SCHEMA {
        return Err(WorkspaceError::Marker {
            path,
            message: format!("unsupported schema {}", marker.schema),
        });
    }
    Ok(Some(marker))
}

fn write_marker(dir: &Path, marker: &MarkerToml) -> Result<(), WorkspaceError> {
    let path = dir.join(MARKER_FILE);
    let text = toml::to_string_pretty(marker).map_err(|err| WorkspaceError::Marker {
        path: path.clone(),
        message: err.to_string(),
    })?;
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

fn absolute(dir: &Path) -> PathBuf {
    if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(dir)
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().cast_signed())
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
            if ws.is_default {
                for w in items.iter_mut() {
                    w.is_default = false;
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
        fn default(&self) -> Result<Option<Workspace>, RepositoryError> {
            Ok(self
                .items
                .lock()
                .expect("lock")
                .iter()
                .find(|w| w.is_default)
                .cloned())
        }
        fn set_default(&self, id: &WorkspaceId) -> Result<(), RepositoryError> {
            let mut items = self.items.lock().expect("lock");
            if !items.iter().any(|w| &w.id == id) {
                return Err(RepositoryError::NotFound(id.as_str().to_owned()));
            }
            for w in items.iter_mut() {
                w.is_default = &w.id == id;
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

    fn svc() -> (WorkspaceService, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tmp");
        let repo = Arc::new(MemRepo::new());
        (WorkspaceService::new(repo), tmp)
    }

    #[test]
    fn init_creates_layout_marker_and_first_default() {
        let (svc, tmp) = svc();
        let dir = tmp.path().join("ws");
        let InitOutcome::Created(ws) = svc.init(&dir, None).expect("init") else {
            unreachable!("expected Created")
        };
        assert!(ws.is_default);
        assert_eq!(ws.name.as_str(), "workspace01");
        assert!(dir.join("servers").is_dir());
        assert!(dir.join("backups/cas").is_dir());
        assert!(dir.join("workspace.toml").is_file());
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
    fn second_workspace_is_not_default() {
        let (svc, tmp) = svc();
        svc.init(&tmp.path().join("a"), None).expect("a");
        let InitOutcome::Created(b) = svc.init(&tmp.path().join("b"), Some("ws02")).expect("b")
        else {
            unreachable!("expected Created")
        };
        assert!(!b.is_default);
        let def = svc.default_workspace().expect("default");
        assert_eq!(def.name.as_str(), "workspace01");
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
        assert!(ws.is_default);
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
    fn set_default_switches() {
        let (svc, tmp) = svc();
        svc.init(&tmp.path().join("a"), Some("ws01")).expect("a");
        svc.init(&tmp.path().join("b"), Some("ws02")).expect("b");
        let ws = svc.set_default("ws02").expect("use");
        assert!(ws.is_default);
        assert_eq!(svc.default_workspace().expect("def").name.as_str(), "ws02");
    }

    #[test]
    fn remove_without_purge_keeps_files() {
        let (svc, tmp) = svc();
        let dir = tmp.path().join("ws");
        svc.init(&dir, Some("ws01")).expect("init");
        svc.remove("ws01", false).expect("remove");
        assert!(dir.join("workspace.toml").is_file());
    }

    #[test]
    fn remove_with_purge_deletes_dir() {
        let (svc, tmp) = svc();
        let dir = tmp.path().join("ws");
        svc.init(&dir, Some("ws01")).expect("init");
        svc.remove("ws01", true).expect("remove");
        assert!(!dir.exists());
    }
}
