//! `workspace doctor`：工作目录注册表与磁盘的一致性体检
//!
//! 只读编排：不写注册表、不碰服务器目录内容。发现问题只报告，修复由用户
//! 显式执行 `workspace remove` / `server link` / `server remove` 等命令。
//!
//! 判定收敛在这里而不是入口：诊断项枚举化，CLI / 未来 REST 只做映射，
//! 退出码与人话描述都由各处自行派生，不复制检查清单。

use crate::workspace::MarkerState;
use domain::server::meta::{ServerMeta, ServerName};
use domain::workspace::Workspace;
use port::{RepositoryError, ServerRepository, WorkspaceRepository};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;

/// 体检失败：注册表读不出来时不能给出「一致」的结论
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DoctorError {
    /// 注册表读取失败
    #[error(transparent)]
    Repo(#[from] RepositoryError),
}

/// 单项检查的结论
///
/// 每项带结构化字段而不是拼好的字符串：入口要按 kind 做机器判定
/// （退出码、JSON 字段），路径与名称必须可寻址。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CheckKind {
    /// 已注册工作目录但当前标记为空
    NoCurrentWorkspace,
    /// 工作目录在磁盘上不存在
    WorkspaceDirMissing {
        /// 记录中的目录
        path: PathBuf,
    },
    /// 工作目录在，但没有 `workspace.toml`：目录不是（或不再是）工作目录
    WorkspaceMarkerMissing {
        /// 记录中的目录
        path: PathBuf,
    },
    /// `workspace.toml` 的 id 与记录不一致：目录已被替换 / 记录被手改
    WorkspaceMarkerMismatch {
        /// 记录中的目录
        path: PathBuf,
        /// marker 里的 id
        found: String,
        /// 记录里的 id
        expected: String,
    },
    /// `servers/` 无法列举，该工作目录的服务器盘点不可信
    ServersDirUnreadable {
        /// `servers/` 目录
        path: PathBuf,
        /// 读取失败原因
        reason: String,
    },
    /// `servers/` 下的悬空符号链接
    DanglingLink {
        /// 链接本身
        path: PathBuf,
    },
    /// 磁盘上有目录 / 链接，但注册表里没有
    OnDiskNotRegistered {
        /// 未登记的目录
        path: PathBuf,
    },
    /// 登记了但磁盘上找不到
    RegisteredButMissing {
        /// 服务器名
        name: ServerName,
        /// 期望的目录
        path: PathBuf,
    },
    /// 登记的 `rel_dir` 不是磁盘上的直接子目录名（`..` / 子路径 / 绝对路径）
    RelDirNotDirectChild {
        /// 服务器名
        name: ServerName,
        /// 记录中的 `rel_dir`
        rel_dir: PathBuf,
        /// 记录里的工作目录
        workspace_path: PathBuf,
    },
    /// 服务器记录指向的工作目录不在注册表里
    OrphanServerRecord {
        /// 服务器名
        name: ServerName,
        /// 记录指向的工作目录 id
        workspace_id: String,
    },
}

impl CheckKind {
    /// 全部检查项标识，顺序即机器契约的稳定顺序（`--only` 校验与帮助文案共用）
    pub const KNOWN_KINDS: &'static [&'static str] = &[
        "no-current-workspace",
        "workspace-dir-missing",
        "workspace-marker-missing",
        "workspace-marker-mismatch",
        "servers-dir-unreadable",
        "dangling-link",
        "on-disk-not-registered",
        "registered-but-missing",
        "rel-dir-not-direct-child",
        "orphan-server-record",
    ];

    /// 稳定 kebab-case 标识：JSON 的 `kind` 与 `--only` 的取值口径
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::NoCurrentWorkspace => "no-current-workspace",
            Self::WorkspaceDirMissing { .. } => "workspace-dir-missing",
            Self::WorkspaceMarkerMissing { .. } => "workspace-marker-missing",
            Self::WorkspaceMarkerMismatch { .. } => "workspace-marker-mismatch",
            Self::ServersDirUnreadable { .. } => "servers-dir-unreadable",
            Self::DanglingLink { .. } => "dangling-link",
            Self::OnDiskNotRegistered { .. } => "on-disk-not-registered",
            Self::RegisteredButMissing { .. } => "registered-but-missing",
            Self::RelDirNotDirectChild { .. } => "rel-dir-not-direct-child",
            Self::OrphanServerRecord { .. } => "orphan-server-record",
        }
    }

    /// 可寻址位置：路径 / 名称，人类输出的第二行
    #[must_use]
    pub fn locate(&self) -> String {
        match self {
            Self::NoCurrentWorkspace => "mcsup workspace use <name>".to_owned(),
            Self::WorkspaceDirMissing { path }
            | Self::WorkspaceMarkerMissing { path }
            | Self::WorkspaceMarkerMismatch { path, .. }
            | Self::DanglingLink { path }
            | Self::OnDiskNotRegistered { path }
            | Self::ServersDirUnreadable { path, .. } => path.display().to_string(),
            Self::RegisteredButMissing { name, path } => {
                format!("{name} → {}", path.display())
            }
            Self::RelDirNotDirectChild { name, rel_dir, .. } => {
                format!("{name} → rel_dir {}", rel_dir.display())
            }
            Self::OrphanServerRecord { name, workspace_id } => {
                format!("{name} → workspace_id {workspace_id}")
            }
        }
    }
}

impl std::fmt::Display for CheckKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 严重程度
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// 需要处理；doctor 以非零退出
    Error,
    /// 提示性问题，不影响退出码（`--strict` 下算失败）
    Warning,
}

impl Severity {
    /// 稳定小写文本（JSON 与人类输出同一口径）
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
        }
    }
}

/// 一条体检发现
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// 发现类型（含所有可寻址字段）
    pub kind: CheckKind,
    /// 所属工作区名；全局问题为 `None`
    pub workspace: Option<String>,
}

impl Finding {
    /// 严重程度由类型决定，不单独存字段：同一类型的级别永远一致，
    /// 拆成两个字段就可能出现「warning 级 dangling-link」这类非法组合
    #[must_use]
    pub const fn severity(&self) -> Severity {
        match self.kind {
            CheckKind::NoCurrentWorkspace | CheckKind::OnDiskNotRegistered { .. } => {
                Severity::Warning
            }
            CheckKind::WorkspaceDirMissing { .. }
            | CheckKind::WorkspaceMarkerMissing { .. }
            | CheckKind::WorkspaceMarkerMismatch { .. }
            | CheckKind::ServersDirUnreadable { .. }
            | CheckKind::DanglingLink { .. }
            | CheckKind::RegisteredButMissing { .. }
            | CheckKind::RelDirNotDirectChild { .. }
            | CheckKind::OrphanServerRecord { .. } => Severity::Error,
        }
    }
}

/// 一次体检的全部发现（已排序：error 在前，同级按工作区、类型）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    findings: Vec<Finding>,
}

impl Report {
    /// 全部发现
    #[must_use]
    pub fn findings(&self) -> &[Finding] {
        &self.findings
    }

    /// 是否存在 error 级发现
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.findings
            .iter()
            .any(|f| f.severity() == Severity::Error)
    }

    /// 按严重程度计数
    #[must_use]
    pub fn count(&self, severity: Severity) -> usize {
        self.findings
            .iter()
            .filter(|f| f.severity() == severity)
            .count()
    }

    /// 是否干净（无任何发现）
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.findings.is_empty()
    }
}

/// 工作目录与服务器注册表的体检服务
pub struct DoctorService {
    workspaces: Arc<dyn WorkspaceRepository>,
    servers: Arc<dyn ServerRepository>,
}

impl std::fmt::Debug for DoctorService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DoctorService").finish_non_exhaustive()
    }
}

impl DoctorService {
    /// 组合两个注册表
    #[must_use]
    pub fn new(
        workspaces: Arc<dyn WorkspaceRepository>,
        servers: Arc<dyn ServerRepository>,
    ) -> Self {
        Self {
            workspaces,
            servers,
        }
    }

    /// 跑一遍体检
    ///
    /// 检查项：无当前工作区、工作目录缺失、`workspace.toml` 缺失 / id 不一致、
    /// `servers/` 不可列举、悬空链接、磁盘有目录但未登记、登记了但磁盘缺失、
    /// `rel_dir` 不是直接子目录、服务器记录指向不存在的工作目录。
    ///
    /// 单个检查项的 IO 失败不会被吞成「无问题」：`servers/` 打不开、marker 读
    /// 不出来都各自成为一条发现，否则诊断工具会把故障报成通过。
    ///
    /// # Errors
    ///
    /// 注册表读取失败时返回 [`DoctorError`]（此时给不出可信结论）
    pub fn run(&self) -> Result<Report, DoctorError> {
        let workspaces = self.workspaces.list()?;
        let servers = self.servers.list()?;
        let mut findings = Vec::new();

        // 一个工作区都没有是合法的首次使用状态，不报
        if !workspaces.is_empty() && self.workspaces.current()?.is_none() {
            findings.push(Finding {
                kind: CheckKind::NoCurrentWorkspace,
                workspace: None,
            });
        }

        for ws in &workspaces {
            Self::check_workspace(ws, &servers, &mut findings);
        }

        for meta in &servers {
            if !workspaces.iter().any(|ws| ws.id == meta.workspace_id) {
                findings.push(Finding {
                    kind: CheckKind::OrphanServerRecord {
                        name: meta.name.clone(),
                        workspace_id: meta.workspace_id.as_str().to_owned(),
                    },
                    workspace: None,
                });
            }
        }

        findings.sort_by(|a, b| {
            a.severity()
                .cmp(&b.severity())
                .then_with(|| a.workspace.cmp(&b.workspace))
                .then_with(|| a.kind.as_str().cmp(b.kind.as_str()))
        });
        Ok(Report { findings })
    }

    fn check_workspace(ws: &Workspace, servers: &[ServerMeta], out: &mut Vec<Finding>) {
        let scope = Some(ws.name.as_str().to_owned());
        let push = |out: &mut Vec<Finding>, kind: CheckKind| {
            out.push(Finding {
                kind,
                workspace: scope.clone(),
            });
        };

        // workspace.toml 的存在与身份先查：它是 remove --purge 的前置闸，
        // 也对齐「目录是不是这条记录的那个目录」
        match crate::workspace::read_marker_id(&ws.path, &ws.id) {
            MarkerState::Missing => push(
                out,
                CheckKind::WorkspaceMarkerMissing {
                    path: ws.path.clone(),
                },
            ),
            MarkerState::Mismatch(found) => push(
                out,
                CheckKind::WorkspaceMarkerMismatch {
                    path: ws.path.clone(),
                    found,
                    expected: ws.id.as_str().to_owned(),
                },
            ),
            // marker 存在且 id 一致：目录身份成立，但不代表目录仍然是目录
            MarkerState::Match | MarkerState::Corrupt => {}
        }

        if !ws.path.is_dir() {
            push(
                out,
                CheckKind::WorkspaceDirMissing {
                    path: ws.path.clone(),
                },
            );
            return;
        }

        let servers_dir = ws.path.join("servers");
        let entries = match std::fs::read_dir(&servers_dir) {
            Ok(entries) => entries,
            Err(err) => {
                push(
                    out,
                    CheckKind::ServersDirUnreadable {
                        path: servers_dir,
                        reason: err.to_string(),
                    },
                );
                return;
            }
        };

        for meta in servers {
            let owner = meta.workspace_id == ws.id;
            // rel_dir 越界先判：既会影响未登记盘点的比较基准，也影响删除路径
            if owner && !is_direct_child(&meta.rel_dir) {
                push(
                    out,
                    CheckKind::RelDirNotDirectChild {
                        name: meta.name.clone(),
                        rel_dir: meta.rel_dir.clone(),
                        workspace_path: ws.path.clone(),
                    },
                );
                continue;
            }
            if owner && servers_dir.join(&meta.rel_dir).symlink_metadata().is_err() {
                push(
                    out,
                    CheckKind::RegisteredButMissing {
                        name: meta.name.clone(),
                        path: servers_dir.join(&meta.rel_dir),
                    },
                );
            }
        }

        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                // file_type 读不出来时仍按名字比对，不用吞掉整项
                continue;
            };
            if file_type.is_symlink() && std::fs::metadata(&path).is_err() {
                push(out, CheckKind::DanglingLink { path });
                continue;
            }
            let fname = entry.file_name();
            let name = Path::new(&fname);
            if !servers
                .iter()
                .any(|m| m.workspace_id == ws.id && m.rel_dir == name)
            {
                push(out, CheckKind::OnDiskNotRegistered { path });
            }
        }
    }
}

/// `rel_dir` 必须是单层相对路径：`..`、分隔符、绝对路径都会把路径拼接到
/// 工作区之外，`servers/` 下的直接子目录比对也就失去意义
fn is_direct_child(rel_dir: &Path) -> bool {
    let mut components = rel_dir.components();
    matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(_)), None)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_wire_names_are_stable_kebab_case() {
        assert_eq!(
            CheckKind::NoCurrentWorkspace.as_str(),
            "no-current-workspace"
        );
        assert_eq!(
            CheckKind::WorkspaceMarkerMismatch {
                path: PathBuf::from("/w"),
                found: "a".into(),
                expected: "b".into(),
            }
            .as_str(),
            "workspace-marker-mismatch"
        );
        assert_eq!(
            CheckKind::OnDiskNotRegistered {
                path: PathBuf::from("/w/servers/x"),
            }
            .to_string(),
            "on-disk-not-registered"
        );
    }

    #[test]
    fn severity_is_derived_from_kind() {
        let warning = Finding {
            kind: CheckKind::NoCurrentWorkspace,
            workspace: None,
        };
        assert_eq!(warning.severity(), Severity::Warning);
        let error = Finding {
            kind: CheckKind::DanglingLink {
                path: PathBuf::from("/w/servers/x"),
            },
            workspace: Some("ws".into()),
        };
        assert_eq!(error.severity(), Severity::Error);
    }

    #[test]
    fn rel_dir_must_be_single_normal_component() {
        assert!(is_direct_child(Path::new("alpha")));
        assert!(!is_direct_child(Path::new(".")));
        assert!(!is_direct_child(Path::new("..")));
        assert!(!is_direct_child(Path::new("a/b")));
        assert!(!is_direct_child(Path::new("/abs")));
        assert!(!is_direct_child(Path::new("")));
    }

    #[test]
    fn empty_report_is_clean() {
        let report = Report::default();
        assert!(report.is_empty());
        assert!(!report.has_errors());
        assert_eq!(report.count(Severity::Error), 0);
    }
}
