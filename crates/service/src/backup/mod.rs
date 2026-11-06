//! 备份编排：创建快照、列表、恢复、删除、校验与清理
//!
//! 分层职责：
//!
//! - **判定**（范围、保留策略、恢复前置条件）在 `domain`
//! - **存储**（CAS 对象、快照记录、目录扫描）在 `backup` 实现层
//! - **编排**（解析服务器目录、发控制台命令、串起各步骤）在本模块
//!
//! 运行中备份的时序（`save-off` 兜底是本模块最重要的不变量）：
//!
//! ```text
//! status → 运行中? 发 save-off → 发 save-all → 等落盘 → 扫描写 CAS → 落快照
//!        → 无论成功失败，必发 save-on
//! ```
//!
//! `save-off` 之后漏发 `save-on` 会让服务器永久停止写盘——世界还在内存里，
//! 进程一退全部丢失。因此 `save-on` 由 [`SaveGuard`] 的 `Drop` 负责，
//! 覆盖所有退出路径（含 `?` 提前返回与 panic）。
//!
//! 恢复的时序（原地恢复必须先停服，见 [`domain::backup::ensure_restore_stopped`]）：
//!
//! ```text
//! 确认已停服 → 恢复前先备份当前状态 → 写到临时目录 → 原子换入
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use domain::backup::{
    BackupScope, ContentHash, Page, RelPath, RestoreTarget, RetentionInput, Snapshot, SnapshotId,
    SnapshotStatus, ensure_restore_stopped,
};
use port::{
    Backend, ContentStore, ScanError, ScanRules, SnapshotRepository, SnapshotStoreError,
    workspace_backups_root,
};
use thiserror::Error;

use crate::atomic::unix_now;
use crate::registry::{RegisteredServer, ServerRegistry, ServerRegistryError};
use crate::{FsError, LifecycleError};

/// 运行中备份前发送的关盘命令
const CMD_SAVE_OFF: &str = "save-off";
/// 运行中备份后发送的开盘命令
const CMD_SAVE_ON: &str = "save-on";
/// 让服务端把内存中的区块写盘
const CMD_SAVE_ALL: &str = "save-all flush";
/// 等待 `save-all` 落盘的默认时长
pub const DEFAULT_FLUSH_WAIT_MS: u64 = 2000;

/// 快照标签的最大字节数：标签会进日志与表格，过长会挤爆输出
pub const MAX_LABEL_LEN: usize = 64;

/// 备份操作失败
///
/// 变体若被入口直接展示，文案由入口按 [`BackupError::kind`] 渲染：本层的
/// `Display` 只用于日志与调试。
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum BackupError {
    /// 注册表失败（含服务器不存在、工作区缺失）
    #[error(transparent)]
    Registry(#[from] ServerRegistryError),
    /// 生命周期 / supervisor 交互失败（探测状态、注入命令）
    #[error(transparent)]
    Lifecycle(#[from] LifecycleError),
    /// supervisor 交互失败
    #[error(transparent)]
    Supervisor(#[from] port::SupervisorError),
    /// 快照记录读写失败
    #[error(transparent)]
    Store(#[from] SnapshotStoreError),
    /// CAS 读写失败
    #[error(transparent)]
    Cas(#[from] port::CasError),
    /// 目录扫描失败
    #[error(transparent)]
    Scan(#[from] ScanError),
    /// 文件系统失败
    #[error(transparent)]
    Fs(#[from] FsError),
    /// 快照不存在
    #[error("snapshot not found: {0}")]
    SnapshotNotFound(String),
    /// 快照不可恢复（写入中 / 已失败）
    #[error("snapshot {id} is not restorable (status: {status})")]
    NotRestorable {
        /// 快照 id
        id: String,
        /// 当前状态
        status: &'static str,
    },
    /// 原地恢复被拒绝：服务器仍在运行
    #[error("refusing to restore in place while server {name} is running")]
    RestoreWhileRunning {
        /// 服务器名
        name: String,
    },
    /// 恢复到的新服务名已被占用
    #[error("target server already exists: {0}")]
    TargetExists(String),
    /// 备份范围为空：范围内没有任何文件，备份无意义
    #[error("nothing to back up in scope")]
    EmptyScope,
    /// 校验发现损坏 / 缺失对象
    #[error("snapshot {id} is broken: {} corrupt or missing object(s)", broken.len())]
    VerifyFailed {
        /// 快照 id
        id: String,
        /// 损坏或缺失的对象
        broken: Vec<ContentHash>,
    },
    /// 标签非法
    #[error("invalid snapshot label: {raw}")]
    InvalidLabel {
        /// 原始输入
        raw: String,
    },
    /// 保留策略非法（`--keep` 与 `--older-than` 同时给出）
    #[error("keep and older-than are mutually exclusive")]
    ConflictingRetention,
    /// 恢复被用户取消（暂存区已清理，原目录不变）
    #[error("restore was cancelled")]
    RestoreCancelled,
    /// 快照内同一路径重复（生成逻辑错误）
    #[error(transparent)]
    DuplicatePath(#[from] domain::backup::DuplicatePath),
    /// 快照 id 生成失败（时间戳非法，理论不可达）
    #[error("cannot build a snapshot id: {0}")]
    InvalidSnapshotId(#[from] domain::backup::InvalidSnapshotId),
}

impl BackupError {
    /// 失败等级
    ///
    /// 包装变体**委托**给内层（`Store` / `Scan` 自带取消与未找到的区分）。
    /// 快照不可恢复 / 原地恢复撞上运行中的服务器 / 恢复目标已存在都是状态冲突；
    /// 空范围、标签与保留策略非法是用法错误；校验发现损坏是「数据确实坏了」，
    /// 归通用失败——脚本要的是「这份快照不能用来恢复」这个结论。
    #[must_use]
    pub fn category(&self) -> port::ErrorCategory {
        match self {
            Self::Registry(err) => err.category(),
            Self::Lifecycle(err) => err.category(),
            Self::Supervisor(err) => err.category(),
            Self::Store(err) => err.category(),
            Self::Cas(err) => err.category(),
            Self::Scan(err) => err.category(),
            Self::Fs(err) => err.category(),
            Self::SnapshotNotFound(_) => port::ErrorCategory::NotFound,
            Self::NotRestorable { .. }
            | Self::RestoreWhileRunning { .. }
            | Self::TargetExists(_) => port::ErrorCategory::Conflict,
            Self::EmptyScope | Self::InvalidLabel { .. } | Self::ConflictingRetention => {
                port::ErrorCategory::Usage
            }
            Self::RestoreCancelled => port::ErrorCategory::Cancelled,
            Self::VerifyFailed { .. } | Self::DuplicatePath(_) | Self::InvalidSnapshotId(_) => {
                port::ErrorCategory::Failure
            }
        }
    }

    /// 面向用户展示的分类；结构化错误不带文案，入口按此渲染提示
    ///
    /// 包装类变体（[`Self::Registry`] / [`Self::Lifecycle`] / [`Self::Store`] /
    /// [`Self::Cas`] / [`Self::Scan`] / [`Self::Fs`]）各自有自己的 `kind`，
    /// 此处返回 `None` 让入口继续沿错误链找。
    #[must_use]
    pub fn kind(&self) -> Option<BackupErrorKind<'_>> {
        let kind = match self {
            Self::Registry(_)
            | Self::Lifecycle(_)
            | Self::Supervisor(_)
            | Self::Store(_)
            | Self::Cas(_)
            | Self::Scan(_)
            | Self::Fs(_)
            | Self::DuplicatePath(_)
            | Self::InvalidSnapshotId(_) => return None,
            Self::SnapshotNotFound(id) => BackupErrorKind::SnapshotNotFound { id },
            Self::NotRestorable { id, status } => BackupErrorKind::NotRestorable { id, status },
            Self::RestoreWhileRunning { name } => BackupErrorKind::RestoreWhileRunning { name },
            Self::TargetExists(name) => BackupErrorKind::TargetExists { name },
            Self::EmptyScope => BackupErrorKind::EmptyScope,
            Self::VerifyFailed { id, broken } => BackupErrorKind::VerifyFailed { id, broken },
            Self::InvalidLabel { raw } => BackupErrorKind::InvalidLabel { raw },
            Self::ConflictingRetention => BackupErrorKind::ConflictingRetention,
            Self::RestoreCancelled => BackupErrorKind::RestoreCancelled,
        };
        Some(kind)
    }
}

/// [`BackupError::kind`] 的结果：只带判定与参数，文案由入口渲染
#[derive(Debug)]
#[non_exhaustive]
pub enum BackupErrorKind<'a> {
    /// 快照不存在
    SnapshotNotFound {
        /// 快照 id
        id: &'a str,
    },
    /// 快照不可恢复
    NotRestorable {
        /// 快照 id
        id: &'a str,
        /// 当前状态
        status: &'a str,
    },
    /// 原地恢复被拒：服务器仍在运行
    RestoreWhileRunning {
        /// 服务器名
        name: &'a str,
    },
    /// 目标服务名已被占用
    TargetExists {
        /// 服务器名
        name: &'a str,
    },
    /// 范围内没有可备份的文件
    EmptyScope,
    /// 校验失败
    VerifyFailed {
        /// 快照 id
        id: &'a str,
        /// 损坏或缺失的对象
        broken: &'a [ContentHash],
    },
    /// 标签非法
    InvalidLabel {
        /// 原始输入
        raw: &'a str,
    },
    /// 保留策略冲突
    ConflictingRetention,
    /// 恢复被取消
    RestoreCancelled,
}

/// `backup create` 的选项
#[derive(Debug, Clone, Default)]
pub struct CreateOptions {
    /// 用户标签，如 `before-upgrade`
    pub label: Option<String>,
    /// 覆盖服务属性里的备份范围；`None` = 用服务器配置
    pub scope: Option<BackupScope>,
    /// 跳过父快照复用提示（强制全量重读，用于怀疑元数据不可靠时）
    pub no_reuse: bool,
    /// 运行中等待 `save-all` 落盘的时长
    pub flush_wait_ms: Option<u64>,
}

/// `backup create` 的结果
#[derive(Debug, Clone)]
pub struct CreateOutcome {
    /// 新建的快照
    pub snapshot: Snapshot,
    /// 扫描期跳过的条目数（运行期文件、范围外、符号链接）
    pub skipped: usize,
}

/// 快照列表的一页
#[derive(Debug, Clone)]
pub struct SnapshotPage {
    /// 本页快照（按时间倒序：最新在前）
    pub items: Vec<Snapshot>,
    /// 该服务器的快照总数
    pub total: usize,
    /// 当前页码
    pub page: usize,
    /// 每页条数
    pub page_size: usize,
}

impl SnapshotPage {
    /// 总页数
    #[must_use]
    pub fn pages(&self) -> usize {
        Page::new(self.page, self.page_size).pages(self.total)
    }
}

/// `backup restore` 的结果
#[derive(Debug, Clone)]
pub struct RestoreOutcome {
    /// 被恢复的快照
    pub snapshot: Snapshot,
    /// 恢复目标目录
    pub destination: PathBuf,
    /// 写回的文件数
    pub files: usize,
    /// 写回的字节数
    pub bytes: u64,
    /// 原地恢复前自动生成的安全快照 id
    ///
    /// 恢复后发现「恢复错了」时，可从它回到恢复前状态。`--to` 恢复不动
    /// 原服务器，因此没有这一项。
    pub safety_snapshot: Option<SnapshotId>,
    /// 恢复后被删除的路径数（快照记录了墓碑的文件）
    pub deleted: usize,
}

/// `backup delete` 的结果
#[derive(Debug, Clone)]
pub struct DeleteOutcome {
    /// 被删除的快照
    pub snapshot: Snapshot,
    /// 该快照释放的 CAS 对象数（引用计数归零、可被 prune 回收的部分）
    pub orphaned: usize,
}

/// `backup verify` 的结果
#[derive(Debug, Clone)]
pub struct VerifyOutcome {
    /// 校验的快照
    pub snapshot: Snapshot,
    /// 被引用的对象数（去重后）
    pub objects: usize,
    /// 校验通过的字节数
    pub bytes: u64,
    /// 损坏或缺失的对象（空 = 完好）
    pub broken: Vec<ContentHash>,
}

impl VerifyOutcome {
    /// 是否完好
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.broken.is_empty()
    }
}

/// `backup prune` 的结果
#[derive(Debug, Clone, Default)]
pub struct PruneOutcome {
    /// 被删掉的快照
    pub removed: Vec<Snapshot>,
    /// 回收的 CAS 对象数
    pub objects_reclaimed: usize,
    /// 回收的字节数
    pub bytes_reclaimed: u64,
    /// 是否只是试运行（`--dry-run`）
    pub dry_run: bool,
}

/// 备份编排服务
///
/// 克隆即共享同一套装配（与 `ServerLifecycleService` 口径一致）。
#[derive(Clone)]
pub struct BackupService {
    registry: Arc<ServerRegistry>,
    backend: Arc<dyn Backend>,
    store: Arc<dyn port::BackupStore>,
    /// 远端备份执行器；未注入时远端条目按「不可达」拒绝
    remote: Option<Arc<dyn port::RemoteBackupPort>>,
    events: Arc<dyn port::EventSink>,
}

impl std::fmt::Debug for BackupService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackupService").finish_non_exhaustive()
    }
}

impl BackupService {
    /// 组合注册表、执行位置与备份数据能力（无事件消费者）
    ///
    /// supervisor 经 [`Backend`] 取得（探测状态 / 注入控制台命令），存储经
    /// [`port::BackupStore`] 取得。`service` 不直接依赖 `runtime` / `backup` /
    /// `storage` 任何一个实现 crate。
    #[must_use]
    pub fn new(
        registry: Arc<ServerRegistry>,
        backend: Arc<dyn Backend>,
        store: Arc<dyn port::BackupStore>,
    ) -> Self {
        Self {
            registry,
            backend,
            store,
            remote: None,
            events: Arc::new(port::NoopEventSink),
        }
    }

    /// 注入远端备份执行器
    ///
    /// 注入后，来源为远端的服务器的备份操作整段转发到对端执行。
    #[must_use]
    pub fn with_remote_backups(mut self, remote: Arc<dyn port::RemoteBackupPort>) -> Self {
        self.remote = Some(remote);
        self
    }

    /// 注入领域事件出口
    #[must_use]
    pub fn with_events(mut self, events: Arc<dyn port::EventSink>) -> Self {
        self.events = events;
        self
    }

    // ---- 内部装配 ----

    /// 解析服务器并返回（注册信息、CAS、快照索引、备份根目录）
    fn resolve(&self, name: &str) -> Result<BackupLayout, BackupError> {
        let server = self.registry.get(name)?;
        Ok(BackupLayout::for_server(&server, self.store.as_ref()))
    }

    /// 服务器是否在运行
    fn is_running(&self, name: &str) -> Result<bool, BackupError> {
        let server = self.registry.get(name)?;
        Ok(self
            .backend
            .supervisor()
            .status(&server.server_ref())?
            .is_some_and(|status| status.status.is_alive()))
    }

    /// 向服务器控制台注入一行
    fn send_line(&self, name: &str, line: &str) -> Result<(), BackupError> {
        let server = self.registry.get(name)?;
        match self
            .backend
            .supervisor()
            .send_line(&server.server_ref(), line)
        {
            Err(err) => Err(BackupError::Lifecycle(
                LifecycleError::from_supervisor_send(err),
            )),
            Ok(()) => Ok(()),
        }
    }

    // ---- create ----

    /// 创建一份快照（默认全量；有父快照时靠复用提示做增量）
    ///
    /// 服务器运行中也能备份：先 `save-off` 关盘，`save-all flush` 落盘，
    /// 扫描期间不会再有不一致写入，结束时由 `SaveGuard` 保证 `save-on`。
    ///
    /// 快照先以 `failed` 语义的 `.partial` 落盘，写完才标完成：中途崩溃
    /// 留下的是可识别的失败记录，不会被当成可恢复的版本。
    ///
    /// # Errors
    ///
    /// 服务器不存在、范围为空、CAS / 扫描 / 记录写入失败，或注入控制台
    /// 命令失败时返回 [`BackupError`]
    pub fn create(
        &self,
        name: &str,
        options: &CreateOptions,
        cancel: &port::CancellationToken,
    ) -> Result<CreateOutcome, BackupError> {
        // 远端：整段转发（扫描与 CAS 都在对端），只回传元数据
        if let Some((server_id, remote)) = self.route_remote(name)? {
            let options = port::RemoteBackupCreate {
                label: options.label.clone(),
                scope: options.scope.clone(),
                no_reuse: options.no_reuse,
                flush_wait_ms: options.flush_wait_ms,
            };
            let outcome = remote
                .create(&server_id, &options)
                .map_err(|err| Self::remote_failure(name, &err))?;
            self.events
                .publish(domain::server::event::DomainEvent::BackupCompleted {
                    name: name.to_owned(),
                    snapshot: outcome.snapshot.id.as_str().to_owned(),
                    added_bytes: outcome.snapshot.added_bytes,
                });
            return Ok(CreateOutcome {
                snapshot: outcome.snapshot,
                skipped: outcome.skipped,
            });
        }
        match self.create_inner(name, options, cancel) {
            Ok(outcome) => Ok(outcome),
            Err(err) => {
                // 失败也是事实：让消费者能统计失败率 / 告警
                self.events
                    .publish(domain::server::event::DomainEvent::BackupFailed {
                        name: name.to_owned(),
                        reason: err.to_string(),
                    });
                Err(err)
            }
        }
    }

    fn create_inner(
        &self,
        name: &str,
        options: &CreateOptions,
        cancel: &port::CancellationToken,
    ) -> Result<CreateOutcome, BackupError> {
        let layout = self.resolve(name)?;
        if !layout.dir.is_dir() {
            return Err(BackupError::Fs(FsError::marker(
                layout.dir.clone(),
                "server directory is missing",
            )));
        }
        let label = normalize_label(options.label.as_deref())?;
        let scope = options
            .scope
            .clone()
            .unwrap_or_else(|| self.scope_of(name).unwrap_or_default());

        let online = self.is_running(name)?;
        // 现场保护：从这里到函数结束，任何退出路径都会补发 save-on
        let _guard = if online {
            Some(SaveGuard::engage(self, name)?)
        } else {
            None
        };
        if online {
            self.send_line(name, CMD_SAVE_ALL)?;
            // 等待时长来自 CLI / 远端帧，可能大到让线程锁死；按领域上限夹紧
            let wait =
                domain::limits::flush_wait(options.flush_wait_ms.unwrap_or(DEFAULT_FLUSH_WAIT_MS));
            if !wait.is_zero() {
                std::thread::sleep(wait);
            }
        }

        let parent = layout
            .index
            .list_for_server(name)?
            .into_iter()
            .rfind(|snapshot| snapshot.status.is_restorable());
        let rules = ScanRules {
            scope: scope.clone(),
            reuse: if options.no_reuse {
                Vec::new()
            } else {
                reuse_hints(parent.as_ref())
            },
        };

        let now = unix_now();
        let id = next_snapshot_id(now)?;
        layout.index.begin(&id)?;
        // 占位目录建立之后的任何失败都必须清掉它：否则每失败一次就在
        // `snapshots/` 里留下一个空的 `.partial`，越积越多，且下次 list 会
        // 读到一条永远无法完成的记录。
        let outcome = match self.store.scanner().scan(
            &layout.dir,
            layout.cas.as_ref(),
            &rules,
            &mut |_| {},
            cancel,
        ) {
            Ok(outcome) => outcome,
            Err(err) => {
                let _ = layout.index.remove(&id);
                return Err(err.into());
            }
        };
        if outcome.entries.is_empty() {
            // 空快照没有恢复价值，删掉占位目录后报错
            let _ = layout.index.remove(&id);
            return Err(BackupError::EmptyScope);
        }

        let skipped = outcome.skipped.len();
        let snapshot = build_snapshot(
            name,
            id,
            label,
            parent.as_ref(),
            &scope,
            &rules,
            outcome,
            online,
            now,
            &layout.workspace,
        )?;

        layout.index.complete(&snapshot)?;
        tracing::info!(
            server = %name,
            snapshot = %snapshot.id,
            added = snapshot.added,
            reused = snapshot.reused,
            bytes = snapshot.total_bytes,
            "backup snapshot created"
        );
        self.events
            .publish(domain::server::event::DomainEvent::BackupCompleted {
                name: name.to_owned(),
                snapshot: snapshot.id.as_str().to_owned(),
                added_bytes: snapshot.added_bytes,
            });
        Ok(CreateOutcome { snapshot, skipped })
    }

    /// 服务器配置的备份范围；服务器不存在时退回「全部」
    #[must_use]
    pub fn scope_of(&self, name: &str) -> Option<BackupScope> {
        self.registry
            .get(name)
            .ok()
            .map(|server| server.meta.settings.backup_scope.clone())
    }

    // ---- 远端路由 ----

    /// 远端服务器的备份整段转发到对端；本地返回 `None`
    ///
    /// 路由依据来自组合仓库的真实来源（`get_sourced`），不按名字猜。远端
    /// 未注入执行器时按「不可达」拒绝——有远端条目却没装配连接是装配错误，
    /// 静默落到本地会在错误的磁盘上写快照。
    #[allow(clippy::type_complexity)]
    fn route_remote(
        &self,
        name: &str,
    ) -> Result<
        Option<(
            domain::server::meta::ServerId,
            Arc<dyn port::RemoteBackupPort>,
        )>,
        BackupError,
    > {
        let sourced = self.registry.get_sourced(name)?;
        if sourced.origin.is_local() {
            return Ok(None);
        }
        let Some(remote) = &self.remote else {
            return Err(BackupError::Supervisor(port::SupervisorError::Unreachable(
                format!(
                    "server '{}' is on remote '{}' but no remote connection is configured",
                    name,
                    sourced.origin.as_str()
                ),
            )));
        };
        Ok(Some((sourced.value.meta.id.clone(), remote.clone())))
    }

    /// 远端失败 → `BackupError`（远端错误文案已含上下文，原样携带）
    fn remote_failure(server: &str, err: &port::RemoteBackupError) -> BackupError {
        tracing::warn!(server = %server, error = %err, "remote backup failed");
        BackupError::Supervisor(port::SupervisorError::Remote {
            kind: match err {
                port::RemoteBackupError::NotFound(_) => port::RemoteFailureKind::NotFound,
                port::RemoteBackupError::Invalid(_) => port::RemoteFailureKind::Invalid,
                port::RemoteBackupError::Unreachable(_) => port::RemoteFailureKind::Unreachable,
                port::RemoteBackupError::Protocol(_) => port::RemoteFailureKind::Protocol,
                port::RemoteBackupError::Io(_) | _ => port::RemoteFailureKind::Io,
            },
            message: err.to_string(),
        })
    }

    // ---- list ----

    /// 快照列表（按时间倒序分页）；`digest` 返回 sha256 前 12 位便于人工核对
    ///
    /// 失败与写入中的快照也会列出：用户需要看到残留并清理它们。
    ///
    /// # Errors
    ///
    /// 服务器不存在或快照索引读取失败时返回 [`BackupError`]
    pub fn list(&self, name: &str, page: Page) -> Result<SnapshotPage, BackupError> {
        // 远端：对端分页，原样回传
        if let Some((server_id, remote)) = self.route_remote(name)? {
            let outcome = remote
                .list(&server_id, page)
                .map_err(|err| Self::remote_failure(name, &err))?;
            return Ok(SnapshotPage {
                items: outcome.items,
                total: outcome.total,
                page: outcome.page,
                page_size: outcome.page_size,
            });
        }
        let layout = self.resolve(name)?;
        let mut items = layout.index.list_for_server(name)?;
        items.sort_by(|a, b| (b.created_at, b.id.as_str()).cmp(&(a.created_at, a.id.as_str())));
        let total = items.len();
        let window = Page::new(page.number, page.size);
        Ok(SnapshotPage {
            items: window.slice(&items).to_vec(),
            total,
            page: window.number,
            page_size: window.size,
        })
    }

    /// 某台服务器的全部快照（升序；prune 的输入）
    fn all_snapshots(layout: &BackupLayout, name: &str) -> Result<Vec<Snapshot>, BackupError> {
        Ok(layout.index.list_for_server(name)?)
    }

    // ---- restore ----

    /// 从快照恢复
    ///
    /// 原地恢复要求服务器已停止：运行中的 JVM 持有世界文件句柄，换掉文件后
    /// 它继续写旧 inode（Windows 直接失败，Unix 上「恢复了但又被写回去」）。
    /// 原地恢复前会先自动备份当前状态，恢复错了还能回去。
    ///
    /// `--to` 只复制内容，不复制身份：新目录会拿到一份全新的 `mcsup.json`，
    /// 否则与源服务器撞 id，`server link` 永远注册不上。
    ///
    /// # Errors
    ///
    /// 快照不存在 / 不可恢复、原地恢复且服务器在运行、目标服务名已占用、
    /// 落点逃出服务器目录（符号链接）、CAS 对象缺失或写盘失败时返回
    /// [`BackupError`]
    pub fn restore(
        &self,
        name: &str,
        snapshot_id: &str,
        target: &RestoreTarget,
        cancel: &port::CancellationToken,
    ) -> Result<RestoreOutcome, BackupError> {
        // 远端：对端写盘，`--to` 的目标也创建在对端
        if let Some((server_id, remote)) = self.route_remote(name)? {
            let to_new = match target {
                RestoreTarget::InPlace => None,
                RestoreTarget::NewServer { name: new_name } => Some(new_name.as_str()),
            };
            let outcome = remote
                .restore(&server_id, snapshot_id, to_new)
                .map_err(|err| Self::remote_failure(name, &err))?;
            return Ok(RestoreOutcome {
                snapshot: outcome.snapshot,
                destination: PathBuf::from(&outcome.destination),
                files: outcome.files,
                bytes: outcome.bytes,
                safety_snapshot: outcome.safety_snapshot,
                deleted: outcome.deleted,
            });
        }
        let layout = self.resolve(name)?;
        let id = SnapshotId::parse(snapshot_id)
            .map_err(|_| BackupError::SnapshotNotFound(snapshot_id.to_owned()))?;
        let snapshot = layout.index.get(&id).map_err(|err| match err {
            SnapshotStoreError::NotFound(_) => {
                BackupError::SnapshotNotFound(snapshot_id.to_owned())
            }
            other => other.into(),
        })?;
        if !snapshot.status.is_restorable() {
            return Err(BackupError::NotRestorable {
                id: snapshot.id.as_str().to_owned(),
                status: snapshot.status.as_str(),
            });
        }

        let alive = self.is_running(name)?;
        ensure_restore_stopped(target, alive).map_err(|_| BackupError::RestoreWhileRunning {
            name: name.to_owned(),
        })?;

        // 恢复前先留一份现场：`--to` 不动原服务器，无需多备份一次
        let safety_snapshot = match target {
            RestoreTarget::InPlace => {
                let outcome = self.create(
                    name,
                    &CreateOptions {
                        label: Some(format!("pre-restore-{}", snapshot.id.as_str())),
                        ..CreateOptions::default()
                    },
                    cancel,
                )?;
                Some(outcome.snapshot.id)
            }
            RestoreTarget::NewServer { .. } => None,
        };

        let destination = match target {
            RestoreTarget::InPlace => layout.dir.clone(),
            RestoreTarget::NewServer { name: new_name } => {
                // 名字已由 `ServerName` 校验：`../x` / `/abs` / `C:\x` 进不来。
                if self.registry.get(new_name.as_str()).is_ok() {
                    return Err(BackupError::TargetExists(new_name.to_string()));
                }
                let dest = layout.workspace.join("servers").join(new_name.as_str());
                // 目录已存在（哪怕没注册）也拒绝：往残留目录里合并写入，
                // 产物就不再是快照的完整副本，还可能撞上里面预置的符号链接。
                if dest.symlink_metadata().is_ok() {
                    return Err(BackupError::TargetExists(new_name.to_string()));
                }
                dest
            }
        };
        let outcome = materialize(&layout, &snapshot, &destination, target, cancel)?;
        // `--to` 复制的是内容，不是身份：快照里的 `mcsup.json` 属于源服务器，
        // 沿用会让新目录与源服务器撞 id，`server link` 必然失败。恢复完成后
        // 立即写一份新身份，目录才真的能被注册。
        if let RestoreTarget::NewServer { name: new_name } = target {
            self.registry.assign_new_identity(&destination, new_name)?;
        }
        Ok(RestoreOutcome {
            snapshot,
            destination: outcome.destination,
            files: outcome.files,
            bytes: outcome.bytes,
            safety_snapshot,
            deleted: outcome.deleted,
        })
    }

    // ---- delete ----

    /// 删除一份快照记录
    ///
    /// 不立即删 CAS 对象：同一工作区的其他快照可能仍在引用它们，
    /// 交给 [`Self::prune`] 的 GC 统一回收。
    ///
    /// # Errors
    ///
    /// 快照不存在或索引写入失败时返回 [`BackupError`]
    pub fn delete(&self, name: &str, snapshot_id: &str) -> Result<DeleteOutcome, BackupError> {
        // 远端：对端删记录
        if let Some((server_id, remote)) = self.route_remote(name)? {
            let outcome = remote
                .delete(&server_id, snapshot_id)
                .map_err(|err| Self::remote_failure(name, &err))?;
            return Ok(DeleteOutcome {
                snapshot: outcome.snapshot,
                orphaned: outcome.orphaned,
            });
        }
        let layout = self.resolve(name)?;
        let id = SnapshotId::parse(snapshot_id)
            .map_err(|_| BackupError::SnapshotNotFound(snapshot_id.to_owned()))?;
        let snapshot = layout.index.get(&id).map_err(|err| match err {
            SnapshotStoreError::NotFound(_) => {
                BackupError::SnapshotNotFound(snapshot_id.to_owned())
            }
            other => other.into(),
        })?;
        let before = referenced_hashes(layout.index.as_ref())?;
        layout.index.remove(&id)?;
        let after = referenced_hashes(layout.index.as_ref())?;
        let orphaned = before
            .keys()
            .filter(|hash| !after.contains_key(*hash))
            .count();
        Ok(DeleteOutcome { snapshot, orphaned })
    }

    // ---- verify ----

    /// 校验一份快照引用的所有对象：存在且哈希一致
    ///
    /// `on_progress` 报告「已校验 / 总数」，入口据此画进度条。
    ///
    /// # Errors
    ///
    /// 快照不存在或 CAS 遍历失败时返回 [`BackupError`]；对象损坏不算错误，
    /// 记入 [`VerifyOutcome::broken`] 由入口决定退出码。
    pub fn verify(
        &self,
        name: &str,
        snapshot_id: &str,
        on_progress: &mut dyn FnMut(usize, usize),
    ) -> Result<VerifyOutcome, BackupError> {
        // 远端：对端逐对象校验（进度单次回调，跨线不流式）
        if let Some((server_id, remote)) = self.route_remote(name)? {
            let _ = on_progress;
            let outcome = remote
                .verify(&server_id, snapshot_id)
                .map_err(|err| Self::remote_failure(name, &err))?;
            return Ok(VerifyOutcome {
                snapshot: outcome.snapshot,
                objects: outcome.objects,
                bytes: outcome.bytes,
                broken: outcome.broken,
            });
        }
        let layout = self.resolve(name)?;
        let id = SnapshotId::parse(snapshot_id)
            .map_err(|_| BackupError::SnapshotNotFound(snapshot_id.to_owned()))?;
        let snapshot = layout.index.get(&id).map_err(|err| match err {
            SnapshotStoreError::NotFound(_) => {
                BackupError::SnapshotNotFound(snapshot_id.to_owned())
            }
            other => other.into(),
        })?;
        let hashes: Vec<ContentHash> = snapshot.content.referenced_hashes().cloned().collect();
        let mut bytes = 0u64;
        for hash in &hashes {
            if let Some(size) = layout.cas.stat(hash)? {
                bytes = bytes.saturating_add(size);
            }
        }
        let broken = layout.cas.verify(&hashes, on_progress)?;
        Ok(VerifyOutcome {
            snapshot,
            objects: hashes.len(),
            bytes,
            broken,
        })
    }

    // ---- prune ----

    /// 按保留策略清理快照并回收无引用的 CAS 对象
    ///
    /// `retention` 两个字段都不给时只做 GC，不删任何快照（清理陈旧对象）；
    /// 两个都给判定为冲突（[`BackupError::ConflictingRetention`]），不会被
    /// 静默地择一执行。`dry_run` 为真时只统计，不动磁盘。
    ///
    /// 引用计数跨**整个工作区**统计：CAS 按工作区共享，只看当前服务器会
    /// 删掉别的服务器正在用的对象。
    ///
    /// # Errors
    ///
    /// 保留策略冲突、服务器不存在、索引读写失败或对象删除失败时返回
    /// [`BackupError`]
    pub fn prune(
        &self,
        name: &str,
        retention: RetentionInput,
        dry_run: bool,
    ) -> Result<PruneOutcome, BackupError> {
        // 判定收在此处：本地与远端入口都只递交原始输入，冲突口径只有一份
        let policy = retention
            .resolve()
            .map_err(|_| BackupError::ConflictingRetention)?;
        // 远端：策略判定与 GC 都在对端（快照与 CAS 都在那边）
        if let Some((server_id, remote)) = self.route_remote(name)? {
            let outcome = remote
                .prune(&server_id, policy, dry_run)
                .map_err(|err| Self::remote_failure(name, &err))?;
            return Ok(PruneOutcome {
                removed: outcome.removed,
                objects_reclaimed: outcome.objects_reclaimed,
                bytes_reclaimed: outcome.bytes_reclaimed,
                dry_run: outcome.dry_run,
            });
        }
        let layout = self.resolve(name)?;
        let now = unix_now();
        let candidates = Self::all_snapshots(&layout, name)?;
        let victims: Vec<SnapshotId> = policy
            .map(|policy| {
                policy
                    .victims(&candidates, now)
                    .into_iter()
                    .map(|snapshot| snapshot.id.clone())
                    .collect()
            })
            .unwrap_or_default();

        // 删掉候选后剩下什么：这才是可以回收的对象集合
        let mut survivors = referenced_hashes(layout.index.as_ref())?;
        for id in &victims {
            if let Ok(snapshot) = layout.index.get(id) {
                for hash in snapshot.content.referenced_hashes() {
                    if let Some(count) = survivors.get_mut(hash) {
                        *count = count.saturating_sub(1);
                        if *count == 0 {
                            survivors.remove(hash);
                        }
                    }
                }
            }
        }

        let mut outcome = PruneOutcome {
            removed: Vec::new(),
            objects_reclaimed: 0,
            bytes_reclaimed: 0,
            dry_run,
        };
        for snapshot in candidates.iter().filter(|s| victims.contains(&s.id)) {
            outcome.removed.push(snapshot.clone());
        }
        if dry_run {
            let live = layout.cas.list()?;
            for hash in &live {
                if !survivors.contains_key(hash) {
                    outcome.objects_reclaimed += 1;
                    outcome.bytes_reclaimed += layout.cas.stat(hash)?.unwrap_or(0);
                }
            }
            return Ok(outcome);
        }

        for id in &victims {
            layout.index.remove(id)?;
        }
        let live = layout.cas.list()?;
        for hash in &live {
            if survivors.contains_key(hash) {
                continue;
            }
            let size = layout.cas.stat(hash)?.unwrap_or(0);
            layout.cas.remove(hash)?;
            outcome.objects_reclaimed += 1;
            outcome.bytes_reclaimed = outcome.bytes_reclaimed.saturating_add(size);
        }
        tracing::info!(
            server = %name,
            removed = outcome.removed.len(),
            objects = outcome.objects_reclaimed,
            bytes = outcome.bytes_reclaimed,
            "backup prune finished"
        );
        Ok(outcome)
    }
}

/// 一台服务器的备份布局：服务器目录 + 该工作区的 CAS / 快照索引
///
/// CAS 与快照按**工作区**共享而不是按服务器：同一工作区里两台服务器
/// 往往跑同一套 mods，内容去重收益最大；快照记录里带 `server` 字段做
/// 归属区分。
struct BackupLayout {
    /// 服务器目录（扫描与恢复的根）
    dir: PathBuf,
    /// 所属工作区目录
    workspace: PathBuf,
    /// CAS
    cas: Arc<dyn ContentStore>,
    /// 快照索引
    index: Arc<dyn SnapshotRepository>,
}

impl BackupLayout {
    /// 按产品口径装配：CAS 落在工作区的 `backups/`
    fn for_server(server: &RegisteredServer, store: &dyn port::BackupStore) -> Self {
        let workspace = server_workspace(&server.dir, &server.meta.rel_dir);
        let root = workspace_backups_root(&workspace);
        Self {
            dir: server.dir.clone(),
            workspace,
            cas: store.content_store(&root),
            index: store.snapshot_repository(&root),
        }
    }
}

/// 由服务器目录与 `rel_dir` 反推工作区目录（`{ws}/servers/{rel_dir}`）
///
/// 服务器目录恒由工作区拼出，去掉尾部两级即得工作区；层级不足时回退到
/// 服务器目录本身——此时 CAS 落在服务器内，虽不理想但不会写错位置。
fn server_workspace(dir: &Path, rel_dir: &Path) -> PathBuf {
    let depth = rel_dir.components().count() + 1; // 加 `servers` 这一级
    let mut candidate = dir;
    for _ in 0..depth {
        match candidate.parent() {
            Some(parent) => candidate = parent,
            None => return dir.to_path_buf(),
        }
    }
    candidate.to_path_buf()
}

/// `save-on` 的兜底守卫
///
/// `save-off` 之后必须无条件 `save-on`，否则服务器永久停写、世界只在内存里。
/// 用 `Drop` 而不是在每个返回点手写：`?` 提前返回、panic 展开都会经过它。
struct SaveGuard<'a> {
    service: &'a BackupService,
    name: &'a str,
    armed: bool,
}

impl<'a> SaveGuard<'a> {
    /// 发 `save-off` 并武装守卫；发送失败时守卫保持未武装（不需要 `save-on`）
    fn engage(service: &'a BackupService, name: &'a str) -> Result<Self, BackupError> {
        service.send_line(name, CMD_SAVE_OFF)?;
        Ok(Self {
            service,
            name,
            armed: true,
        })
    }
}

impl Drop for SaveGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Drop 里不能传播错误：`save-on` 失败只能记日志，但仍要尽力发送
        if let Err(err) = self.service.send_line(self.name, CMD_SAVE_ON) {
            tracing::error!(
                server = %self.name,
                error = %err,
                "failed to send save-on after backup; the server may still have saving disabled"
            );
        }
    }
}

/// 标签校验：非空、去空白后不超长、不含控制字符
fn normalize_label(raw: Option<&str>) -> Result<Option<String>, BackupError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.chars().count() > MAX_LABEL_LEN || trimmed.chars().any(char::is_control) {
        return Err(BackupError::InvalidLabel {
            raw: raw.to_owned(),
        });
    }
    Ok(Some(trimmed.to_owned()))
}

/// 生成快照 id：`<UTC 时间>-<随机后缀>`
///
/// 时间前缀让字典序即时间序（`list` 不另维护排序键）；后缀避免同秒并发
/// （定时备份与手动备份撞在同一秒）产生同名目录。
///
/// # Errors
///
/// 时间戳字符集与 [`SnapshotId`] 的白名单不一致时返回
/// [`InvalidSnapshotId`]——宁可报错也不 panic：字符集是两处约定（本函数的
/// 格式串 + 领域的白名单），任一处改动都应该表现为可读的错误。
fn next_snapshot_id(now: i64) -> Result<SnapshotId, domain::backup::InvalidSnapshotId> {
    let stamp = utc_stamp(now);
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    SnapshotId::parse(&format!("{stamp}-{}", &suffix[..6]))
}

/// Unix 秒 → `YYYYMMDDTHHMMSS`（UTC，小写 `t` 与 `SnapshotId` 字符集一致）
fn utc_stamp(now: i64) -> String {
    let secs = now.max(0);
    let days = secs / 86_400;
    let rest = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}{month:02}{day:02}t{:02}{:02}{:02}",
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    )
}

/// 自 1970-01-01 起的天数 → (年, 月, 日)；Howard Hinnant 的 `civil_from_days`
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (
        year,
        u32::try_from(m).unwrap_or(1),
        u32::try_from(d).unwrap_or(1),
    )
}

/// 由扫描结果与父快照构造要落盘的快照对象
///
/// 计算删除墓碑（父快照有、本次没有的路径）与复用计数，补齐工作区标识。
/// 从 [`BackupService::create`] 拆出，保持后者在行数上限内、职责单一。
#[allow(clippy::too_many_arguments)]
fn build_snapshot(
    name: &str,
    id: SnapshotId,
    label: Option<String>,
    parent: Option<&Snapshot>,
    scope: &BackupScope,
    rules: &ScanRules,
    outcome: port::ScanOutcome,
    online: bool,
    now: i64,
    workspace: &Path,
) -> Result<Snapshot, BackupError> {
    let deleted: Vec<RelPath> = parent
        .map(|parent| {
            parent
                .content
                .entries()
                .iter()
                .filter(|(path, _)| {
                    scope.contains(path) && !outcome.entries.iter().any(|(kept, _)| kept == path)
                })
                .map(|(path, _)| path.clone())
                .collect()
        })
        .unwrap_or_default();

    let reused = rules.reuse.len().saturating_sub(outcome.added);
    Ok(Snapshot {
        id,
        workspace_id: String::new(),
        server: name.to_owned(),
        label,
        parent: parent.map(|parent| parent.id.clone()),
        scope: scope.clone(),
        status: SnapshotStatus::Complete,
        content: domain::backup::SnapshotContent::new(outcome.entries, deleted)?
            .with_stamps(outcome.stamps),
        added: outcome.added,
        reused,
        total_bytes: outcome.total_bytes,
        added_bytes: outcome.written_bytes,
        server_online: online,
        created_at: now,
    }
    .with_workspace(workspace))
}

/// 父快照的复用提示：`(路径, 哈希, 大小, mtime)`
///
/// 携带的是**父快照记录时的** `(size, mtime)`，扫描侧拿它比当前磁盘：
/// 一致才复用，任何一项不同都必须重算。用当前磁盘的值去比当前磁盘会
/// 永远命中，增量就退化成「永不重算」。
fn reuse_hints(parent: Option<&Snapshot>) -> Vec<(RelPath, ContentHash, u64, i64)> {
    let Some(parent) = parent else {
        return Vec::new();
    };
    let mut hints = Vec::with_capacity(parent.content.stamps().len());
    for (path, size, mtime) in parent.content.stamps() {
        if let Some((_, hash)) = parent
            .content
            .entries()
            .iter()
            .find(|(known, _)| known == path)
        {
            hints.push((path.clone(), hash.clone(), *size, *mtime));
        }
    }
    hints
}

/// 全工作区被引用的哈希集合（GC 的依据）
///
/// 必须看**全部服务器**的快照：CAS 按工作区共享，只统计当前服务器的
/// 引用会删掉别的服务器正在用的对象。
fn referenced_hashes(
    index: &dyn SnapshotRepository,
) -> Result<BTreeMap<ContentHash, usize>, BackupError> {
    let mut counts: BTreeMap<ContentHash, usize> = BTreeMap::new();
    for snapshot in index.list_all()? {
        for hash in snapshot.content.referenced_hashes() {
            *counts.entry(hash.clone()).or_insert(0) += 1;
        }
    }
    Ok(counts)
}

/// 快照落盘的附加信息：所属工作区的稳定标识
///
/// `Snapshot` 的字段是公开的，`workspace_id` 在编排层补齐（领域层不知道
/// 工作区标识怎么取）。
trait SnapshotWorkspaceExt {
    /// 用工作区路径补齐 `workspace_id`
    fn with_workspace(self, workspace: &Path) -> Self;
}

impl SnapshotWorkspaceExt for Snapshot {
    fn with_workspace(mut self, workspace: &Path) -> Self {
        self.workspace_id = workspace
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        self
    }
}

/// 把快照内容写到目标目录的中间结果
struct Materialized {
    /// 实际写入的目录
    destination: PathBuf,
    /// 写入的文件数
    files: usize,
    /// 写入的字节数
    bytes: u64,
    /// 按墓碑删除的路径数
    deleted: usize,
}

/// 把快照内容落到 `destination`
///
/// 原地恢复走「临时目录 + 逐文件换入」：先把全部内容写到服务器目录下的
/// `.mcsup-restore/` 暂存区（CAS 对象都已校验过存在性），确认全部就绪后
/// 再按路径逐个 rename 覆盖。这样中途失败时原服务器目录仍可用，不会出现
/// 「删了一半旧文件、写了一半新文件」的混合状态。
///
/// `--to` 恢复直接写到新目录，不走暂存区：新目录本就是空的。快照里的
/// `mcsup.json` 会原样落盘，调用方随后用新身份覆盖（见
/// [`BackupService::restore`]）——内容可以复制，身份不能。
fn materialize(
    layout: &BackupLayout,
    snapshot: &Snapshot,
    destination: &Path,
    target: &RestoreTarget,
    cancel: &port::CancellationToken,
) -> Result<Materialized, BackupError> {
    match target {
        RestoreTarget::NewServer { .. } => {
            // 先在同级暂存目录里写全，再整体 rename 到落点。
            //
            // 直接写最终目录时，中途失败会留下半成品，而 `restore` 的
            // `TargetExists` 检查会拒绝重试——用户必须手工删目录才能再来一次。
            // 暂存目录放在同一父目录下，保证 rename 不跨设备。
            let parent = destination.parent().unwrap_or(destination);
            crate::fs::create_dir_all(parent).map_err(|err| FsError::io(parent, err))?;
            let staging = parent.join(format!(".mcsup-restore-{}", uuid::Uuid::new_v4().simple()));
            crate::fs::create_dir_all(&staging).map_err(|err| FsError::io(&staging, err))?;
            // 暂存目录是「未完成的恢复」的墓碑：任何退出路径都要清掉它
            let _guard = StagingGuard::new(&staging);

            // `--to` 也不信任落点：暂存目录里可能有（继承来的）指向目录外的
            // 符号链接，逐个条目仍走 `Containment`。
            let mut guard = Containment::new(&staging)?;
            let mut files = 0;
            let mut bytes = 0u64;
            for (path, hash) in snapshot.content.entries() {
                // 逐文件检查：几个 G 的恢复要几分钟，不可取消等于 Ctrl-C 无效
                if cancel.is_cancelled() {
                    return Err(BackupError::RestoreCancelled);
                }
                let sink = guard.writable(path)?;
                bytes += layout.cas.get(hash, &sink)?;
                files += 1;
            }
            drop(guard);
            // 内容齐全后才出现在落点上：要么没有目录，要么是完整副本
            crate::fs::rename(&staging, destination)
                .map_err(|err| FsError::io(destination, err))?;
            Ok(Materialized {
                destination: destination.to_path_buf(),
                files,
                bytes,
                deleted: 0,
            })
        }
        RestoreTarget::InPlace => {
            let staging = destination.join(".mcsup-restore");
            // 残留的暂存区（上次恢复中断）先清掉，避免写进旧内容
            match crate::fs::remove_dir_all(&staging) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(FsError::io(&staging, err).into()),
            }
            crate::fs::create_dir_all(&staging).map_err(|err| FsError::io(&staging, err))?;
            // 暂存区是「未完成的恢复」的墓碑：从这里到函数结束的**任何**退出
            // 路径都必须清掉它，否则下次恢复会把上次的半成品当成草稿。
            let _guard = StagingGuard::new(&staging);

            let mut files = 0;
            let mut bytes = 0u64;
            for (path, hash) in snapshot.content.entries() {
                if cancel.is_cancelled() {
                    return Err(BackupError::RestoreCancelled);
                }
                bytes += layout
                    .cas
                    .get(hash, &staging.join(path.to_platform_path()))?;
                files += 1;
            }
            // 内容就绪后再动原目录：逐个 rename 覆盖
            let mut dest_guard = Containment::new(destination)?;
            for (path, _) in snapshot.content.entries() {
                let from = staging.join(path.to_platform_path());
                let to = dest_guard.writable(path)?;
                // Windows 上目标存在时 rename 会失败：先删再换。用
                // symlink_metadata 判断「存在」，悬空链接也算占用。
                if to.symlink_metadata().is_ok() {
                    crate::fs::remove_file(&to).map_err(|err| FsError::io(&to, err))?;
                }
                crate::fs::rename(&from, &to).map_err(|err| FsError::io(&to, err))?;
            }
            // 墓碑：快照记录「这个文件当时不存在」，必须删掉它，否则复活
            let mut deleted = 0;
            for path in snapshot.content.deleted() {
                let victim = dest_guard.existing(path)?;
                match crate::fs::remove_file(&victim) {
                    Ok(()) => deleted += 1,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => return Err(FsError::io(&victim, err).into()),
                }
            }
            // 原地恢复要把目录真正还原成快照状态：快照范围内、磁盘上存在但
            // 快照里没有的文件（快照之后新建的）也必须删掉。只依赖墓碑不够——
            // 老快照（无父快照）根本没有墓碑记录。
            deleted += remove_untracked(destination, snapshot)?;
            // 暂存区由 `_guard` 在离开作用域时清理
            Ok(Materialized {
                destination: destination.to_path_buf(),
                files,
                bytes,
                deleted,
            })
        }
    }
}

/// 恢复落点守卫：每个路径都必须真的落在服务器目录之内
///
/// 领域层的 [`RelPath`] 已挡掉 `..`、盘符、ADS 与设备名，但磁盘上的**符号
/// 链接**仍能把 `world/level.dat` 指到目录外。父目录建好后 `canonicalize`
/// 出真实位置，确认仍在根内；链接指向目录内部是允许的（共享世界是常见用法）。
/// 校验过的父目录缓存下来，避免每个文件都做两次系统调用。
struct Containment {
    root: PathBuf,
    verified: std::collections::HashSet<PathBuf>,
}

impl Containment {
    fn new(root: &Path) -> Result<Self, BackupError> {
        let root = crate::fs::canonicalize(root).map_err(|err| FsError::io(root, err))?;
        Ok(Self {
            root,
            verified: std::collections::HashSet::new(),
        })
    }

    /// 写入落点：缺的父目录直接建出来
    fn writable(&mut self, rel: &RelPath) -> Result<PathBuf, BackupError> {
        self.checked(rel, true)
    }

    /// 删除落点：不建目录；父目录不存在说明没有可删的东西
    fn existing(&mut self, rel: &RelPath) -> Result<PathBuf, BackupError> {
        self.checked(rel, false)
    }

    fn checked(&mut self, rel: &RelPath, create: bool) -> Result<PathBuf, BackupError> {
        let target = self.root.join(rel.to_platform_path());
        let parent = target
            .parent()
            .ok_or_else(|| FsError::marker(target.clone(), "snapshot path has no parent"))?;
        if !self.verified.contains(parent) {
            let resolved = if create {
                crate::fs::create_dir_all(parent).map_err(|err| FsError::io(parent, err))?;
                Some(crate::fs::canonicalize(parent).map_err(|err| FsError::io(parent, err))?)
            } else {
                match crate::fs::canonicalize(parent) {
                    Ok(resolved) => Some(resolved),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
                    Err(err) => return Err(FsError::io(parent, err).into()),
                }
            };
            if let Some(resolved) = resolved {
                if !resolved.starts_with(&self.root) {
                    return Err(FsError::marker(
                        &target,
                        format!(
                            "snapshot path escapes the server directory: {}",
                            rel.as_str()
                        ),
                    )
                    .into());
                }
                self.verified.insert(parent.to_path_buf());
            }
        }
        Ok(target)
    }
}

/// 原地恢复的暂存区守卫：离开作用域时删掉 `.mcsup-restore/`
///
/// 成功路径与失败路径都要清：成功时内容已换入，失败时半成品没有保留价值。
/// 手动在每个 `?` 前面加清理代码是错的写法——漏一处就留一堆垃圾。
struct StagingGuard<'a> {
    path: &'a Path,
}

impl<'a> StagingGuard<'a> {
    fn new(path: &'a Path) -> Self {
        Self { path }
    }
}

impl Drop for StagingGuard<'_> {
    fn drop(&mut self) {
        // Drop 里不能传播错误：删不掉只记日志（通常是杀毒软件占用句柄）；
        // 目录本来就不存在是正常路径，不记日志。
        if let Err(err) = crate::fs::remove_dir_all(self.path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %self.path.display(),
                error = %err,
                "failed to clean up the restore staging directory"
            );
        }
    }
}

/// 删除目标目录里「不在快照中」的文件，返回删除数量
///
/// 只处理快照范围内、且非运行期的普通文件：`run/`、`console.log` 是运行期
/// 产物，不该因为恢复而消失；符号链接不跟随（与扫描口径一致）。
fn remove_untracked(destination: &Path, snapshot: &Snapshot) -> Result<usize, BackupError> {
    let tracked: std::collections::BTreeSet<&str> = snapshot
        .content
        .entries()
        .iter()
        .map(|(path, _)| path.as_str())
        .collect();
    let mut victims: Vec<RelPath> = Vec::new();
    let mut stack: Vec<(PathBuf, String)> = vec![(destination.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        let entries = match crate::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(FsError::io(&dir, err).into()),
        };
        for entry in entries {
            let entry = entry.map_err(|err| FsError::io(&dir, err))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let rel = if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}/{name}")
            };
            // 顶层运行期条目整体跳过
            if prefix.is_empty() && (name == "run" || name == "console.log") {
                continue;
            }
            let file_type = entry
                .file_type()
                .map_err(|err| FsError::io(&entry.path(), err))?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                stack.push((entry.path(), rel));
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let Ok(path) = RelPath::parse(&rel) else {
                continue;
            };
            if !snapshot.scope.contains(&path) {
                continue;
            }
            if !tracked.contains(path.as_str()) {
                victims.push(path);
            }
        }
    }
    let mut removed = 0;
    for path in &victims {
        let victim = destination.join(path.to_platform_path());
        match crate::fs::remove_file(&victim) {
            Ok(()) => removed += 1,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(FsError::io(&victim, err).into()),
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests;
