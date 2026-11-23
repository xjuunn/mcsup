//! 插件业务编排：安装记录、授权、启停与宿主能力实现
//!
//! 本模块只做编排：状态迁移、能力交集与 manifest / descriptor 校验都在
//! `domain::plugin`；进程与帧在 `crates/plugin`；文件后端在 `storage`。
//!
//! # 方向
//!
//! [`HostApiImpl`] 把宿主需要的业务能力实现成 [`port::plugin`] 的窄 trait。
//! 依赖全部是 `Arc<dyn ...>` 或同层 service，不反向依赖宿主运行时。

use crate::backup::{BackupError, BackupService, CreateOptions};
use crate::fs_error::FsError;
use crate::java::{JavaError, JavaService};
use crate::lifecycle::{LifecycleError, ServerLifecycleService, StartOptions};
use crate::mod_::{ApplyOptions, InstallSpec, ListOptions, ModError, ModService};
use crate::props::ServerPropsService;
use crate::registry::{ServerRegistry, ServerRegistryError};
use domain::backup::{Page, RestoreTarget, RetentionInput};
use domain::java::{JavaMajor, JavaPackageType, JavaRuntime};
use domain::path::RelPath;
use domain::plugin::capability::{self, Capability};
use domain::plugin::descriptor::Descriptor;
use domain::plugin::error::PluginErrorCode;
use domain::plugin::manifest::Manifest;
use domain::plugin::revocation::{RevocationList, revoked_message};
use domain::plugin::state::{PluginEvent, PluginState};
use domain::plugin::{PluginId, PluginScope};
use domain::server::artifact::{Checksum, ChecksumAlgorithm};
use domain::server::content::ContentKind;
use domain::server::layout::console_log_path;
use domain::server::meta::ServerName;
use domain::server::status::ServerStatus;
use port::plugin::{
    ArtifactOrigin, ArtifactSource, ArtifactVerifier, AuditEvent, AuditResult, AuditSink,
    BackupEntry, BackupOps, ConfigOps, ConsoleIo, ConsoleLine, ConsolePage, ContentEntry,
    ContentInstallRequest, ContentOps, FileEntry, FileOps, HostError, JavaOps, KvStore,
    MAX_CONSOLE_LINE_BYTES, MAX_CONSOLE_LINES, MAX_FILE_BYTES, NetPolicy, PluginArtifact,
    PluginConfigStore, PluginRecord, PluginRevocations, PluginRuntimeStatus, PluginStore,
    PluginSupervisor, SecretStore, ServerLifecycle, ServerRead, ServerStatusView, ServerSummary,
    SessionInfo, StoredConfig,
};
use port::{ErrorCategory, RepositoryError, StopOutcome, StopSpec};
use serde_json::Value;
use std::collections::BTreeSet;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

/// 清理安装临时目录时使用的固定前缀
const TEMP_PREFIX: &str = ".install-";

/// 安装临时目录的年龄阈值：超过即视为崩溃残留
///
/// 正常安装（含下载）远小于此值。残留目录只在同一个 `plugins_root` 下才有
/// 意义，所以按年龄而不是 pid 判定——pid 会被复用，判断不了任何事。
const STALE_TEMP_AGE: Duration = Duration::from_secs(3600);

/// 抢安装锁的最长等待时间
///
/// 真正的并发安装应当等待而不是立刻失败（用户双击了两次），但等太久只会让
/// 第二个进程挂在那里；30 秒足够覆盖下载之外的全部安装步骤。
const INSTALL_LOCK_TIMEOUT: Duration = Duration::from_secs(30);

/// 插件业务失败
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PluginError {
    /// 记录不存在
    #[error("plugin not found: {0}")]
    NotFound(String),
    /// 记录或产物已经存在
    #[error("plugin already exists: {0}")]
    AlreadyExists(String),
    /// 当前状态不允许该操作
    #[error("{0}")]
    Conflict(String),
    /// 输入不合法（能力名、来源、版本等）
    #[error("{0}")]
    Usage(String),
    /// 能力未授予
    #[error("{0}")]
    Denied(String),
    /// 宿主未运行 / 崩溃
    #[error("{0}")]
    Host(String),
    /// 存储失败
    #[error(transparent)]
    Repo(#[from] RepositoryError),
    /// 文件系统失败（无具体路径的创建 / 删除）
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// 文件系统失败
    #[error(transparent)]
    Fs(#[from] FsError),
    /// 宿主能力返回的错误
    #[error(transparent)]
    HostError(#[from] HostError),
}

impl PluginError {
    /// 失败等级
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        match self {
            Self::NotFound(_) => ErrorCategory::NotFound,
            Self::AlreadyExists(_) | Self::Conflict(_) => ErrorCategory::Conflict,
            Self::Usage(_) => ErrorCategory::Usage,
            Self::Denied(_) => ErrorCategory::Unauthorized,
            Self::Host(_) | Self::Io(_) => ErrorCategory::Backend,
            Self::HostError(err) => err.category,
            Self::Repo(err) => err.category(),
            Self::Fs(err) => err.category(),
        }
    }

    fn from_repo(err: RepositoryError, id: &PluginId) -> Self {
        match err {
            RepositoryError::NotFound(_) => Self::NotFound(id.to_string()),
            other => Self::Repo(other),
        }
    }
}

/// 安装结果
#[derive(Debug, Clone)]
pub struct InstallOutcome {
    /// 写入的安装记录
    pub record: PluginRecord,
    /// 安装后需要用户确认的能力
    pub pending: BTreeSet<Capability>,
}

/// 启停后的状态
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartOutcome {
    /// 会话已建立
    Started(SessionInfo),
    /// 宿主未连接，仅更新了期望状态
    Offline,
}

/// 插件业务服务
///
/// 所有字段都是 `Arc` / `PathBuf`，克隆只共享同一份存储与宿主连接：让同时
/// 需要生命周期与插件的命令拿一份就走，不必层层传引用。
#[derive(Clone)]
pub struct PluginService {
    store: Arc<dyn PluginStore>,
    supervisor: Option<Arc<dyn PluginSupervisor>>,
    source: Option<Arc<dyn ArtifactSource>>,
    verifier: Option<Arc<dyn ArtifactVerifier>>,
    audit: Arc<dyn AuditSink>,
    /// 配置读写（与宿主控制通道共用同一份实现）
    config: Arc<PluginConfig>,
    /// `{data_dir}/mcsup/plugins`：不可变产物根
    plugins_root: PathBuf,
    /// `{data_dir}/mcsup/plugin-data`：插件可写目录
    plugin_data: PathBuf,
    /// `{data_dir}/mcsup/logs/plugins`
    log_root: PathBuf,
    /// 撤销检查；`None` 表示不检查撤销状态
    revocations: Option<Arc<dyn PluginRevocations>>,
}

impl std::fmt::Debug for PluginService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginService")
            .field("plugins_root", &self.plugins_root)
            .field("supervisor", &self.supervisor.is_some())
            .finish_non_exhaustive()
    }
}

impl PluginService {
    /// 构造服务
    ///
    /// 参数就是装配清单：存储、宿主、来源、校验、审计、三类根目录。拆成多个
    /// 结构体只会把同一份清单换个地方摆放。
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        store: Arc<dyn PluginStore>,
        config_store: Arc<dyn PluginConfigStore>,
        supervisor: Option<Arc<dyn PluginSupervisor>>,
        source: Option<Arc<dyn ArtifactSource>>,
        verifier: Option<Arc<dyn ArtifactVerifier>>,
        audit: Arc<dyn AuditSink>,
        plugins_root: PathBuf,
        plugin_data: PathBuf,
        log_root: PathBuf,
    ) -> Self {
        let config = Arc::new(PluginConfig::new(
            Arc::clone(&store),
            config_store,
            plugins_root.clone(),
            supervisor.clone(),
            Arc::clone(&audit),
        ));
        Self {
            store,
            supervisor,
            source,
            verifier,
            audit,
            config,
            plugins_root,
            plugin_data,
            log_root,
            revocations: None,
        }
    }

    /// 启用撤销检查
    ///
    /// 构建器缺省不检查：撤销列表是外部设施的产物，没有它时插件照常运行，
    /// 见 `docs/plugins.md` §16.4 的「不可用时不得阻断」。缓存格式与权限在
    /// 实现层（`provider`），判定只在这里。
    #[must_use]
    pub fn with_revocations(mut self, revocations: Arc<dyn PluginRevocations>) -> Self {
        self.revocations = Some(revocations);
        self
    }

    /// 配置读写句柄（宿主控制通道与 CLI 共用）
    #[must_use]
    pub fn config(&self) -> Arc<dyn ConfigOps> {
        self.config.clone()
    }

    /// 宿主是否可达
    #[must_use]
    pub fn host_available(&self) -> bool {
        self.supervisor.is_some()
    }

    /// 写入撤销列表
    ///
    /// 新列表整体替换旧列表，不合并：撤销可以撤回，合并会让已解除的记录永远
    /// 留在本地。
    ///
    /// # Errors
    ///
    /// 未启用撤销检查时返回 [`PluginError::Usage`]；写入失败返回宿主错误
    pub fn store_revocations(&self, list: &RevocationList) -> Result<(), PluginError> {
        let store = self.revocation_store()?;
        store.store(list)?;
        self.audit.record(&AuditEvent {
            at: now_secs(),
            plugin: "*".to_owned(),
            action: "security:revocations:update".to_owned(),
            capability: None,
            result: AuditResult::Allowed,
            detail: Some(format!("{} entries", list.entries.len())),
        });
        Ok(())
    }

    /// 读取撤销列表（未启用检查时返回空表）
    ///
    /// # Errors
    ///
    /// 缓存存在但读不懂时返回错误
    pub fn list_revocations(&self) -> Result<RevocationList, PluginError> {
        match &self.revocations {
            Some(store) => Ok(store.load()?),
            None => Ok(RevocationList::default()),
        }
    }

    /// 撤销列表能否被读取（`doctor` 用）
    ///
    /// # Errors
    ///
    /// 缓存存在但读不懂时返回错误；未启用检查视为可读。
    pub fn revocation_list_check(&self) -> Result<(), PluginError> {
        self.list_revocations().map(|_| ())
    }

    /// 记录的版本是否被撤销
    ///
    /// 返回弹给用户的说明文本，未命中返回 `None`。
    ///
    /// # Errors
    ///
    /// 撤销列表存在但读不懂时返回错误；未启用检查返回 `Ok(None)`
    pub fn revocation(&self, record: &PluginRecord) -> Result<Option<String>, PluginError> {
        let Some(list) = self.revocations.as_ref().map(|store| store.load()) else {
            return Ok(None);
        };
        Ok(Some(list?).and_then(|list| {
            list.hit(record.id.as_str(), &record.version)
                .map(|entry| revoked_message(entry, &record.version))
        }))
    }

    /// 拒绝启动时统一套用的撤销判定
    fn ensure_not_revoked(&self, record: &PluginRecord) -> Result<(), PluginError> {
        match self.revocation(record) {
            Ok(None) => Ok(()),
            Ok(Some(message)) => Err(PluginError::Denied(message)),
            // 列表读不懂只记录，不阻断：安全设施不可用不等于插件有问题。
            Err(err) => {
                tracing::warn!(
                    plugin = %record.id,
                    error = %err,
                    "revocation list unavailable; skipping revocation check"
                );
                Ok(())
            }
        }
    }

    fn revocation_store(&self) -> Result<&Arc<dyn PluginRevocations>, PluginError> {
        self.revocations.as_ref().ok_or_else(|| {
            PluginError::Usage("plugin revocation checking is not configured".to_owned())
        })
    }

    /// 已安装版本的 descriptor（`ops` 命令与调用的参数校验共用）
    ///
    /// # Errors
    ///
    /// 记录不存在或 `descriptor.json` 非法时返回 [`PluginError`]
    pub fn descriptor(
        &self,
        id: &PluginId,
    ) -> Result<domain::plugin::descriptor::ValidatedDescriptor, PluginError> {
        let record = self.record(id)?;
        let path = record.artifact_dir.join("descriptor.json");
        let bytes = std::fs::read(&path).map_err(|err| PluginError::Fs(FsError::io(&path, err)))?;
        Descriptor::from_json(&bytes, &record.id)
            .map_err(|err| PluginError::Usage(format!("invalid descriptor: {err}")))
    }

    /// 读取插件日志
    ///
    /// # Errors
    ///
    /// 宿主未连接时返回 [`PluginError::Host`]
    pub fn logs(
        &self,
        id: &PluginId,
        offset: Option<u64>,
        max: usize,
    ) -> Result<Value, PluginError> {
        // 日志文件不存在在底层按「空窗口」处理，于是 `plugin logs <未安装 id>`
        // 会静默成功、`--follow` 永远空转。先确认插件存在。
        self.record(id)?;
        Ok(self.supervisor()?.logs(id, offset, max)?)
    }

    /// 安装记录根
    #[must_use]
    pub fn plugins_root(&self) -> &Path {
        &self.plugins_root
    }

    /// 插件数据根
    #[must_use]
    pub fn plugin_data(&self) -> &Path {
        &self.plugin_data
    }

    /// 插件进程此刻是否在运行
    ///
    /// 宿主不可达时按「没在跑」处理：判定只用于决定要不要先停，猜错的方向
    /// 只能是多停一次，不会把进程留在那里。
    #[must_use]
    pub fn is_running(&self, id: &PluginId) -> bool {
        self.supervisor
            .as_ref()
            .and_then(|supervisor| supervisor.status(id).ok())
            .is_some_and(|status| status.session.is_some())
    }

    /// 抢某个插件 id 的安装 / 切换 / 卸载锁
    ///
    /// 三条路径都要持有：它们都在改 `plugins/<id>/` 与记录，彼此并发会互相
    /// 删目录、覆盖记录。锁是内核文件锁，进程崩溃即释放，不需要清理。
    fn plugin_lock(&self, id: &PluginId) -> Result<crate::fs::FileLock, PluginError> {
        std::fs::create_dir_all(&self.plugins_root)?;
        let path = self.plugins_root.join(format!(".{id}.install.lock"));
        crate::fs::FileLock::acquire(&path, INSTALL_LOCK_TIMEOUT).map_err(|err| {
            if err.kind() == std::io::ErrorKind::WouldBlock {
                PluginError::Conflict(format!("another install for {id} is in progress"))
            } else {
                PluginError::Fs(FsError::io(&path, err))
            }
        })
    }

    /// 建一个只属于本次调用的临时解包目录
    ///
    /// 名字带 pid 与自增序号：同一 pid 同一秒的两次安装（`install` 与
    /// `verify` 并发）不能撞到同一个目录，否则会互相 `remove_dir_all`。
    fn temp_dir(&self, tag: &str) -> Result<TempDir, PluginError> {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::fs::create_dir_all(&self.plugins_root)?;
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = self.plugins_root.join(format!(
            "{TEMP_PREFIX}{tag}-{}-{}-{seq}",
            std::process::id(),
            now_secs()
        ));
        std::fs::create_dir_all(&path)?;
        Ok(TempDir { path })
    }

    /// 清掉上次崩溃留下的临时解包目录
    ///
    /// 崩溃路径不会执行 `Drop`，残留目录能长到下载上限那么大。年龄从**目录名**
    /// 里取（尾部就是创建时刻 `…-<pid>-<secs>-<seq>`），不用 mtime：解包过程
    /// 会不断刷新 mtime，崩溃残留的 mtime 也可能因为时钟漂移看起来在未来。
    /// 认不出创建时刻的目录一律不动——宁可留一个目录，也不误删正在解包的。
    fn sweep_stale_temp_dirs(&self) {
        let Ok(entries) = std::fs::read_dir(&self.plugins_root) else {
            return;
        };
        let now = now_secs();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !name.starts_with(TEMP_PREFIX) || !entry.path().is_dir() {
                continue;
            }
            let Some(created) = name
                .rsplit('-')
                .nth(1)
                .and_then(|secs| secs.parse::<i64>().ok())
            else {
                continue;
            };
            if now.saturating_sub(created) >= i64::try_from(STALE_TEMP_AGE.as_secs()).unwrap_or(0) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    /// 全部记录，按 id 升序
    ///
    /// # Errors
    ///
    /// 记录读取失败时返回 [`PluginError`]
    pub fn list(&self) -> Result<Vec<PluginRecord>, PluginError> {
        Ok(self.store.list()?)
    }

    /// 单条记录
    ///
    /// # Errors
    ///
    /// 不存在返回 [`PluginError::NotFound`]
    pub fn record(&self, id: &PluginId) -> Result<PluginRecord, PluginError> {
        self.store
            .get(id)
            .map_err(|err| PluginError::from_repo(err, id))
    }

    /// 实时状态；宿主未连接时回退到记录里的离线状态
    ///
    /// # Errors
    ///
    /// 记录不存在时返回 [`PluginError::NotFound`]
    pub fn status(&self, id: &PluginId) -> Result<PluginRuntimeStatus, PluginError> {
        if let Some(supervisor) = &self.supervisor {
            match supervisor.status(id) {
                Ok(status) => return Ok(status),
                // 宿主未运行：回退到记录，不把「宿主不在」当成「插件不存在」
                Err(err) if err.code == port::plugin::ErrorCode::Unavailable => {}
                Err(err) => return Err(err.into()),
            }
        }
        let record = self.record(id)?;
        Ok(PluginRuntimeStatus {
            id: id.clone(),
            state: offline_state(&record),
            session: None,
            last_error: record.last_error,
        })
    }

    /// 读取配置（密钥字段打码）
    ///
    /// # Errors
    ///
    /// 记录不存在或 descriptor 非法时返回 [`PluginError`]
    pub fn config_get(&self, id: &PluginId) -> Result<Value, PluginError> {
        Ok(self.config.get(id)?)
    }

    /// 写入配置：顶层合并、整体校验、落盘并下发
    ///
    /// # Errors
    ///
    /// schema 校验失败返回 [`PluginError::Usage`]；插件应用失败时保留上一版本
    pub fn config_set(&self, id: &PluginId, values: &Value) -> Result<Value, PluginError> {
        Ok(self.config.set(id, values)?)
    }

    /// 深度合并配置：嵌套对象逐层并入，数组与标量整体替换
    ///
    /// 安装向导只传单个字段（如 `{"qq":{"appId":"..."}}`），不会覆盖同级的
    /// 其它配置，也不会把打码后的密钥写回去。
    ///
    /// # Errors
    ///
    /// schema 校验失败返回 [`PluginError::Usage`]；插件应用失败时保留上一版本
    pub fn config_merge(&self, id: &PluginId, values: &Value) -> Result<Value, PluginError> {
        Ok(self.config.merge(id, values)?)
    }

    /// 本机拉黑：立即停用且不再自动启动
    ///
    /// # Errors
    ///
    /// 记录不存在时返回 [`PluginError::NotFound`]
    pub fn block(&self, id: &PluginId) -> Result<PluginRecord, PluginError> {
        self.set_flag(id, Flag::Blocked, true)
    }

    /// 解除拉黑；不自动启用
    ///
    /// # Errors
    ///
    /// 记录不存在时返回 [`PluginError::NotFound`]
    pub fn unblock(&self, id: &PluginId) -> Result<PluginRecord, PluginError> {
        self.set_flag(id, Flag::Blocked, false)
    }

    /// 隔离：保留产物但禁止运行
    ///
    /// # Errors
    ///
    /// 记录不存在时返回 [`PluginError::NotFound`]
    pub fn quarantine(&self, id: &PluginId) -> Result<PluginRecord, PluginError> {
        self.set_flag(id, Flag::Quarantined, true)
    }

    /// 解除隔离；不自动启用
    ///
    /// # Errors
    ///
    /// 记录不存在时返回 [`PluginError::NotFound`]
    pub fn release(&self, id: &PluginId) -> Result<PluginRecord, PluginError> {
        self.set_flag(id, Flag::Quarantined, false)
    }

    fn set_flag(&self, id: &PluginId, flag: Flag, on: bool) -> Result<PluginRecord, PluginError> {
        let mut record = self.record(id)?;
        let already = match flag {
            Flag::Blocked => record.blocked,
            Flag::Quarantined => record.quarantined,
        };
        if already != on {
            if on {
                // 拉黑 / 隔离立即生效：先停进程，再改记录。
                if let Some(supervisor) = &self.supervisor {
                    match supervisor.status(id) {
                        Ok(status) if status.session.is_some() => {
                            supervisor.stop(id, Duration::from_secs(10))?;
                        }
                        Ok(_) => {}
                        Err(err) if err.code == port::plugin::ErrorCode::Unavailable => {}
                        Err(err) => return Err(err.into()),
                    }
                }
                record.enabled = false;
                record.last_state = PluginState::Disabled;
            }
            match flag {
                Flag::Blocked => record.blocked = on,
                Flag::Quarantined => record.quarantined = on,
            }
            record.updated_at = now_secs();
            self.store.put(&record)?;
            self.audit.record(&AuditEvent {
                at: now_secs(),
                plugin: id.to_string(),
                action: match (flag, on) {
                    (Flag::Blocked, true) => "security:block",
                    (Flag::Blocked, false) => "security:unblock",
                    (Flag::Quarantined, true) => "security:quarantine",
                    (Flag::Quarantined, false) => "security:release",
                }
                .to_owned(),
                capability: None,
                result: AuditResult::Allowed,
                detail: None,
            });
        }
        Ok(record)
    }

    /// 启用：只改期望状态，不拉起进程
    ///
    /// # Errors
    ///
    /// 已启用、仍有待确认能力、被拉黑或隔离时返回 [`PluginError::Conflict`]
    pub fn enable(&self, id: &PluginId) -> Result<PluginRecord, PluginError> {
        let mut record = self.record(id)?;
        if record.blocked {
            return Err(PluginError::Conflict(format!(
                "plugin {id} is blocked; run `mcsup plugin unblock {id}` first"
            )));
        }
        if record.quarantined {
            return Err(PluginError::Conflict(format!(
                "plugin {id} is quarantined; run `mcsup plugin release {id}` first"
            )));
        }
        if record.enabled {
            return Err(PluginError::Conflict(format!(
                "plugin {id} is already enabled"
            )));
        }
        if record.needs_approval() {
            let pending = pending_capabilities(&record);
            return Err(PluginError::Conflict(format!(
                "plugin {id} still needs approval: {pending}; run `mcsup plugin grant {id} <capability>`"
            )));
        }
        self.ensure_not_revoked(&record)?;
        // 待确认状态先走 `Approved`，其余从离线状态直接 `Enabled`。
        let from = record.last_state;
        let next = if from == PluginState::PendingApproval {
            PluginState::PendingApproval
                .transition(PluginEvent::Approved)
                .and_then(|state| state.transition(PluginEvent::Enabled))
        } else {
            from.transition(PluginEvent::Enabled)
        }
        .map_err(|err| PluginError::Conflict(err.to_string()))?;
        record.enabled = true;
        record.last_state = next;
        record.last_error = None;
        record.updated_at = now_secs();
        self.store.put(&record)?;
        self.audit.record(&AuditEvent {
            at: now_secs(),
            plugin: id.to_string(),
            action: "lifecycle:enable".to_owned(),
            capability: None,
            result: AuditResult::Allowed,
            detail: None,
        });
        Ok(record)
    }

    /// 停用：先停进程，再改期望状态
    ///
    /// # Errors
    ///
    /// 记录不存在或停止失败时返回 [`PluginError`]
    pub fn disable(&self, id: &PluginId) -> Result<PluginRecord, PluginError> {
        let mut record = self.record(id)?;
        if let Some(supervisor) = &self.supervisor {
            match supervisor.status(id) {
                Ok(status) if status.session.is_some() => {
                    supervisor.stop(id, Duration::from_secs(10))?;
                }
                Ok(_) => {}
                Err(err) if err.code == port::plugin::ErrorCode::Unavailable => {}
                Err(err) => return Err(err.into()),
            }
        }
        record.enabled = false;
        record.last_state = record
            .last_state
            .transition(PluginEvent::Disabled)
            .unwrap_or(PluginState::Disabled);
        record.updated_at = now_secs();
        self.store.put(&record)?;
        self.audit.record(&AuditEvent {
            at: now_secs(),
            plugin: id.to_string(),
            action: "lifecycle:disable".to_owned(),
            capability: None,
            result: AuditResult::Allowed,
            detail: None,
        });
        Ok(record)
    }

    /// 追加授权
    ///
    /// # Errors
    ///
    /// 能力不在 manifest 内返回 [`PluginError::Usage`]
    pub fn grant(
        &self,
        id: &PluginId,
        capability: Capability,
    ) -> Result<PluginRecord, PluginError> {
        let mut record = self.record(id)?;
        record
            .grant_capability(capability)
            .map_err(|err| PluginError::Usage(format!("{err} for plugin {id}")))?;
        record.updated_at = now_secs();
        self.store.put(&record)?;
        self.audit.record(&AuditEvent {
            at: now_secs(),
            plugin: id.to_string(),
            action: "capability:grant".to_owned(),
            capability: Some(capability),
            result: AuditResult::Allowed,
            detail: None,
        });
        Ok(record)
    }

    /// 撤销授权：立即生效，不重启进程
    ///
    /// # Errors
    ///
    /// 能力尚未授予返回 [`PluginError::NotFound`]
    pub fn revoke(
        &self,
        id: &PluginId,
        capability: Capability,
    ) -> Result<PluginRecord, PluginError> {
        let mut record = self.record(id)?;
        record
            .revoke_capability(capability)
            .map_err(|err| PluginError::NotFound(format!("{err} for plugin {id}")))?;
        if let Some(supervisor) = &self.supervisor {
            // 会话内的授予集合同步收缩；宿主不在时忽略（下次握手自然按记录计算）。
            match supervisor.revoke(id, capability) {
                Ok(()) => {}
                Err(err) if err.code == port::plugin::ErrorCode::Unavailable => {}
                Err(err) => return Err(err.into()),
            }
        }
        record.updated_at = now_secs();
        self.store.put(&record)?;
        self.audit.record(&AuditEvent {
            at: now_secs(),
            plugin: id.to_string(),
            action: "capability:revoke".to_owned(),
            capability: Some(capability),
            result: AuditResult::Denied,
            detail: None,
        });
        Ok(record)
    }

    /// 拉起插件；宿主未连接时返回 [`PluginError::Host`]
    ///
    /// # Errors
    ///
    /// 未启用、握手失败时返回 [`PluginError`]
    pub fn start(&self, id: &PluginId) -> Result<SessionInfo, PluginError> {
        let record = self.record(id)?;
        if record.blocked {
            return Err(PluginError::Conflict(format!(
                "plugin {id} is blocked; run `mcsup plugin unblock {id}` first"
            )));
        }
        if record.quarantined {
            return Err(PluginError::Conflict(format!(
                "plugin {id} is quarantined; run `mcsup plugin release {id}` first"
            )));
        }
        if !record.enabled {
            return Err(PluginError::Conflict(format!(
                "plugin {id} is not enabled; run `mcsup plugin enable {id}`"
            )));
        }
        self.ensure_not_revoked(&record)?;
        let supervisor = self.supervisor()?;
        Ok(supervisor.start(id)?)
    }

    /// 停止插件
    ///
    /// # Errors
    ///
    /// 未运行或宿主未连接时返回 [`PluginError`]
    pub fn stop(&self, id: &PluginId, grace: Duration) -> Result<(), PluginError> {
        Ok(self.supervisor()?.stop(id, grace)?)
    }

    /// 重启插件
    ///
    /// # Errors
    ///
    /// 同 [`Self::start`]
    pub fn restart(&self, id: &PluginId, grace: Duration) -> Result<SessionInfo, PluginError> {
        if let Some(supervisor) = &self.supervisor
            && let Ok(status) = supervisor.status(id)
            && status.session.is_some()
        {
            supervisor.stop(id, grace)?;
        }
        self.start(id)
    }

    /// 调用插件 op
    ///
    /// # Errors
    ///
    /// 能力未授予、超时、宿主未连接时返回 [`PluginError`]
    pub fn call(&self, id: &PluginId, op: &str, input: Value) -> Result<Value, PluginError> {
        Ok(self.supervisor()?.call(id, op, input)?)
    }

    /// 订阅插件 stream
    ///
    /// # Errors
    ///
    /// 同 [`Self::call`]
    pub fn stream(
        &self,
        id: &PluginId,
        op: &str,
        input: Value,
        sink: &mut dyn port::plugin::StreamSink,
    ) -> Result<(), PluginError> {
        Ok(self.supervisor()?.stream(id, op, input, sink)?)
    }

    /// 安装插件
    ///
    /// 流水线：拉取 → 摘要校验 → 解包（由来源实现）→ manifest / descriptor →
    /// 原子移动到 `plugins/<id>/<version>` → 写记录。安装期间不启动进程。
    ///
    /// 升级已存在的插件时：进程先停；`enabled` 与仍声明且已确认的授权保留；
    /// 始终保留当前版本与回滚目标（`previous_version`），`prune` 只清理更旧
    /// 的目录——回滚目标任何情况下都不能删。新版本起不来时自动切回旧版本并
    /// 重新拉起。
    ///
    /// # Errors
    ///
    /// 来源不可用、校验失败、manifest / descriptor 非法、同版本已存在、
    /// 已有同 id 安装进行中时返回 [`PluginError`]
    pub fn install(
        &self,
        origin: &ArtifactOrigin,
        expected_sha256: Option<&Checksum>,
        prune: bool,
    ) -> Result<InstallOutcome, PluginError> {
        self.install_checked(origin, expected_sha256, prune, None)
    }

    /// 按来源升级：产物声明的 id 必须仍是 `expected`
    ///
    /// `plugin update` 不带 `--to` 时按记录里的来源重新拉取。本地来源被手工
    /// 换成别的插件、git 仓库换了 id 之后，装进来的就是**另一个**插件；不校验
    /// 的话命令会以「已切换 <另一个 id>」成功退出，被请求的插件却一点没变。
    ///
    /// # Errors
    ///
    /// 同 [`Self::install`]；另外来源里的 id 与 `expected` 不一致时返回
    /// [`PluginError::Usage`]
    pub fn install_as(
        &self,
        expected: &PluginId,
        origin: &ArtifactOrigin,
        prune: bool,
    ) -> Result<InstallOutcome, PluginError> {
        self.install_checked(origin, None, prune, Some(expected))
    }

    fn install_checked(
        &self,
        origin: &ArtifactOrigin,
        expected_sha256: Option<&Checksum>,
        prune: bool,
        expected_id: Option<&PluginId>,
    ) -> Result<InstallOutcome, PluginError> {
        let source = self.source.as_ref().ok_or_else(|| {
            PluginError::Usage("no plugin artifact source is configured".to_owned())
        })?;
        let source = Arc::clone(source);
        self.sweep_stale_temp_dirs();
        let temp = self.temp_dir("fetch")?;
        self.install_into(
            origin,
            expected_sha256,
            &source,
            temp.path(),
            prune,
            expected_id,
        )
    }

    /// 拉取并校验产物，但不安装
    ///
    /// `verify` 命令用它对未安装的压缩包重新跑一遍来源校验。返回值在存活
    /// 期间持有解包目录，丢弃时删除临时目录。
    ///
    /// # Errors
    ///
    /// 来源未装配、拉取失败或摘要不匹配时返回 [`PluginError`]
    pub fn fetch_artifact(
        &self,
        origin: &ArtifactOrigin,
    ) -> Result<FetchedPluginArtifact, PluginError> {
        let source = self.source.as_ref().ok_or_else(|| {
            PluginError::Usage("no plugin artifact source is configured".to_owned())
        })?;
        self.sweep_stale_temp_dirs();
        let temp = self.temp_dir("verify")?;
        let artifact = source
            .fetch(origin, temp.path())
            .and_then(|fetched| match &self.verifier {
                Some(verifier) => verifier.verify(&fetched, None),
                None => Ok(fetched),
            })
            .map_err(PluginError::from)?;
        Ok(FetchedPluginArtifact {
            artifact,
            _temp: temp,
        })
    }

    // 安装流水线单函数承载：fetch → 校验 → 加锁 → rename → 登记 → prune →
    // 审计 → 恢复运行。拆开只会把同一份状态变更切成两个互相引用的一半。
    #[allow(clippy::too_many_lines)]
    fn install_into(
        &self,
        origin: &ArtifactOrigin,
        expected_sha256: Option<&Checksum>,
        source: &Arc<dyn ArtifactSource>,
        temp: &Path,
        prune: bool,
        expected_id: Option<&PluginId>,
    ) -> Result<InstallOutcome, PluginError> {
        let fetched = source.fetch(origin, temp)?;
        let artifact = if let Some(verifier) = &self.verifier {
            verifier.verify(&fetched, expected_sha256)?
        } else {
            check_source_checksum(expected_sha256, &fetched.sha256)?;
            fetched
        };
        let manifest = Manifest::from_json(
            &serde_json::to_vec(&artifact.manifest)
                .map_err(|err| PluginError::Usage(format!("serialize manifest: {err}")))?,
        )
        .map_err(|err| PluginError::Usage(format!("invalid manifest: {err}")))?;
        if artifact.manifest.id != manifest.id.as_str() {
            return Err(PluginError::Usage(format!(
                "manifest id {} does not match artifact",
                artifact.manifest.id
            )));
        }
        Descriptor::from_json(&artifact.descriptor, &manifest.id)
            .map_err(|err| PluginError::Usage(format!("invalid descriptor: {err}")))?;
        // 入口文件存在且形态合法（docs 10.3 第 5 条）；产物已解包，正是检查点。
        manifest
            .raw
            .entry
            .check_entry_file(&artifact.root, &domain::plugin::platform_key())
            .map_err(|err| PluginError::Usage(format!("entry check failed: {err}")))?;
        let id = manifest.id.clone();
        let version = manifest.raw.version.clone();
        if let Some(expected) = expected_id
            && manifest.id != *expected
        {
            return Err(PluginError::Usage(format!(
                "the source provides plugin {}, not {expected}; nothing was installed",
                manifest.id
            )));
        }
        if manifest.scope == PluginScope::Server {
            // 记录里的 `server` 字段目前没有写入路径（CLI 没有 `--server`），
            // 装了也无法绑定。提示必须说实话，否则用户会去找一个不存在的开关。
            return Err(PluginError::Usage(
                "server-scoped plugins are not supported yet; publish a machine-scoped plugin"
                    .to_owned(),
            ));
        }
        // 按 id 加锁：并发 install / update / remove 会互相删目录、覆盖记录。
        // 抢锁放在解包与 manifest 校验之后——那些步骤不碰 `plugins/<id>/`，
        // 没必要串行（下载可能要几分钟）。
        let _lock = self.plugin_lock(&id)?;
        // 只有 `NotFound` 才等于「没装过」。记录损坏 / IO 错误降级成全新安装
        // 会清零 `enabled`、授权与 `previous_version`，还会顺手删掉旧版本目录。
        let previous = match self.store.get(&id) {
            Ok(record) => Some(record),
            Err(RepositoryError::NotFound(_)) => None,
            Err(err) => return Err(PluginError::from_repo(err, &id)),
        };
        // 判重放在停机之前：同版本重装是必然失败的请求，不能先把正在运行的
        // 进程停掉再告诉用户「已安装」——插件会一直停在那里，用户还得手动
        // 拉起来。
        let target = self.plugins_root.join(id.as_str()).join(&version);
        // 同版本判重只对「记录正在用的那一版」成立。目录存在但记录不指向它，
        // 说明上一次升级在这个版本上失败过（`restore_after_failed_start` 把记录
        // 切回旧版本，新版本目录留在盘上）：不清掉的话这一版再也装不回来——
        // `install` 报 already exists，`rollback` 又没有回滚目标。
        if target.exists() {
            let active = previous
                .as_ref()
                .is_some_and(|record| record.version == version);
            if active {
                return Err(PluginError::AlreadyExists(format!(
                    "{id} {version} is already installed"
                )));
            }
            if let Err(err) = std::fs::remove_dir_all(&target) {
                return Err(PluginError::Fs(FsError::io(&target, err)));
            }
            tracing::warn!(
                plugin = %id,
                version = %version,
                "removed a version directory that no record references"
            );
        }
        // 升级时进程必须先停：旧进程还在用旧产物目录，rename 后它写的是
        // 已被移走的目录。旧状态走一次 UpdateStarted 迁移，留下审计痕迹。
        let was_running = previous.is_some() && self.is_running(&id);
        if was_running && let Some(supervisor) = &self.supervisor {
            supervisor.stop(&id, Duration::from_secs(10))?;
        }
        // 停机之后任何失败都必须把旧版本拉回来（`docs/plugins.md` §15.6）：
        // rename 在 Windows 上会因目录被占用失败，记录写失败也可能是
        // `db/.lock` 超时。失败后撒手不管，插件就静悄悄停在那里了。
        if let Some(parent) = target.parent()
            && let Err(err) = std::fs::create_dir_all(parent)
        {
            self.recover_aborted_install(&id, &target, was_running);
            return Err(err.into());
        }
        // 同文件系统内 rename 才是原子的：临时目录建在 `plugins_root` 下。
        if let Err(err) = std::fs::rename(&artifact.root, &target) {
            self.recover_aborted_install(&id, &target, was_running);
            return Err(PluginError::Fs(FsError::io(&artifact.root, err)));
        }
        let _ = std::fs::remove_dir_all(temp);

        // 升级：只保留新版本仍声明、且此前已确认的能力；低风险照常自动通过。
        let mut approved: BTreeSet<Capability> = manifest
            .capabilities
            .iter()
            .copied()
            .filter(|cap| cap.default_policy() == capability::DefaultPolicy::Allow)
            .collect();
        if let Some(old) = &previous {
            approved.extend(
                old.approved
                    .iter()
                    .copied()
                    .filter(|cap| manifest.capabilities.contains(cap)),
            );
        }
        let pending: BTreeSet<Capability> = manifest
            .capabilities
            .iter()
            .copied()
            .filter(|cap| !approved.contains(cap))
            .collect();
        let now = now_secs();
        // 升级保留启用状态；scope 变了则视为新插件，回默认关闭。
        //
        // 新版本引入待授权能力时必须落 `enabled = false`：`docs/plugins.md`
        // 要求 `pending_approval` 与 `enabled` 互斥，否则授权之后
        // `PendingApproval` 只能迁到 `Disabled`，插件卡在「已启用但既不能
        // `start` 也不能 `enable`」的死角里。
        let enabled = pending.is_empty()
            && previous
                .as_ref()
                .filter(|old| old.scope == manifest.scope)
                .is_some_and(|old| old.enabled);
        let record = PluginRecord {
            id: id.clone(),
            version: version.clone(),
            api: manifest.raw.api,
            scope: manifest.scope,
            server: None,
            enabled,
            approved,
            manifest: manifest.raw.clone(),
            artifact_dir: target.clone(),
            installed_at: now,
            updated_at: now,
            last_state: state_after_install(enabled, !pending.is_empty()),
            last_error: None,
            previous_version: previous.as_ref().map(|record| record.version.clone()),
            source: artifact
                .resolved_source
                .clone()
                .or_else(|| Some(origin_text(origin))),
            sha256: Some(artifact.sha256),
            blocked: previous.as_ref().is_some_and(|old| old.blocked),
            quarantined: previous.as_ref().is_some_and(|old| old.quarantined),
        };
        if let Err(err) = self.store.put(&record) {
            // 记录没写进去：新版本目录成了无主孤儿（再次安装会 `AlreadyExists`），
            // 旧版本记录与目录都还在，把进程拉回旧版本即可。
            self.recover_aborted_install(&id, &target, was_running);
            return Err(err.into());
        }
        // 升级自运行状态：恢复运行。启动（含握手）成功之前不动旧版本目
        // 录——「旧版本已删、新版本起不来」是必须避免的状态
        // （`docs/plugins.md` §15.6）。
        if was_running
            && record.enabled
            && let Err(err) = self.start(&id)
        {
            self.restore_after_failed_start(&id, previous.as_ref(), &version, &err)?;
            return Err(err);
        }
        // 版本保留：始终保留当前版本与回滚目标（`previous_version`）。`--prune`
        // 只清理更旧的目录，不再把回滚目标一起删掉。
        let mut keep = vec![version.as_str()];
        if let Some(old) = &previous {
            keep.push(old.version.as_str());
        }
        self.prune_versions(&id, &keep)?;
        self.audit.record(&AuditEvent {
            at: now,
            plugin: id.to_string(),
            action: "install".to_owned(),
            capability: None,
            result: AuditResult::Allowed,
            detail: Some(if prune {
                format!("version {version} (pruned older versions)")
            } else {
                format!("version {version}")
            }),
        });
        Ok(InstallOutcome { record, pending })
    }

    /// 停机之后、新版本接管之前失败：删掉半成品目录，把旧版本重新拉起来
    ///
    /// 记录此时还指向旧版本（`store.put` 没成功），旧产物目录也还在盘上，
    /// 所以恢复只是「删孤儿 + 启动」。启动失败只记日志：调用方要看到的是
    /// 原始错误，不能被第二个错误盖掉。
    fn recover_aborted_install(&self, id: &PluginId, target: &Path, was_running: bool) {
        if target.is_dir() {
            let _ = std::fs::remove_dir_all(target);
        }
        if !was_running {
            return;
        }
        if let Err(err) = self.start(id) {
            tracing::warn!(plugin = %id, error = %err, "previous version failed to restart");
        }
    }

    /// 新版本起不来：把记录切回旧版本并重新拉起
    ///
    /// 产物目录此时还完好在盘上（清理在启动成功之后），所以回滚只是把记录写
    /// 回旧版本再启动。`[禁止]` 让用户停在「旧版本已删、新版本起不来」的状
    /// 态里手工恢复（`docs/plugins.md` §15.6）。
    fn restore_after_failed_start(
        &self,
        id: &PluginId,
        previous: Option<&PluginRecord>,
        failed_version: &str,
        cause: &PluginError,
    ) -> Result<(), PluginError> {
        let reason = format!("version {failed_version} failed to start: {cause}");
        let Some(old) = previous else {
            // 全新安装：没有可回滚的版本，只留一条失败记录
            let mut record = self.record(id)?;
            record.last_state = PluginState::Failed;
            record.last_error = Some(reason.clone());
            record.updated_at = now_secs();
            self.store.put(&record)?;
            self.audit.record(&AuditEvent {
                at: now_secs(),
                plugin: id.to_string(),
                action: "install:failed".to_owned(),
                capability: None,
                result: AuditResult::Failed,
                detail: Some(reason),
            });
            return Ok(());
        };
        let mut restored = old.clone();
        restored.last_state = PluginState::Failed;
        restored.last_error = Some(reason.clone());
        restored.updated_at = now_secs();
        self.store.put(&restored)?;
        self.audit.record(&AuditEvent {
            at: now_secs(),
            plugin: id.to_string(),
            action: "install:rollback".to_owned(),
            capability: None,
            result: AuditResult::Failed,
            detail: Some(format!("{reason}; restored version {}", restored.version)),
        });
        // 失败版本的目录此刻没人引用（记录已切回旧版本）：留在盘上会让这一版
        // 永远装不回来，`install` 只会报 already exists。
        let failed_dir = self.plugins_root.join(id.as_str()).join(failed_version);
        if failed_dir.is_dir()
            && let Err(err) = std::fs::remove_dir_all(&failed_dir)
        {
            tracing::warn!(
                plugin = %id,
                version = %failed_version,
                error = %err,
                "failed to remove the version directory that could not start"
            );
        }
        // 旧版本重新拉起；起不来时保留失败记录，由调用方看到首个错误
        if let Err(err) = self.start(id) {
            tracing::warn!(plugin = %id, error = %err, "restored version failed to start");
        }
        Ok(())
    }

    /// 已安装的版本（按目录枚举，不信任记录）
    ///
    /// 记录只记当前版本与回滚目标，盘上还可能留着更旧的目录。`update --to`
    /// 要回答「到底能切到哪些版本」，只能看目录；不是 `SemVer` 的目录名（残留、
    /// 手工改名）不列出来。
    ///
    /// # Errors
    ///
    /// 目录读取失败时返回 [`PluginError`]
    pub fn installed_versions(&self, id: &PluginId) -> Result<Vec<String>, PluginError> {
        let root = self.plugins_root.join(id.as_str());
        let Ok(entries) = std::fs::read_dir(&root) else {
            return Ok(Vec::new());
        };
        let mut versions = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|err| PluginError::Fs(FsError::io(&root, err)))?;
            if !entry.path().is_dir() {
                continue;
            }
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if domain::plugin::manifest::semver_is_valid(&name) {
                versions.push(name);
            }
        }
        versions.sort_by_key(|version| version_key(version));
        Ok(versions)
    }

    /// 清理同 id 的旧版本目录，只保留 `keep` 里的版本
    fn prune_versions(&self, id: &PluginId, keep: &[&str]) -> Result<(), PluginError> {
        let root = self.plugins_root.join(id.as_str());
        let Ok(entries) = std::fs::read_dir(&root) else {
            return Ok(());
        };
        for entry in entries {
            let entry = entry.map_err(|err| PluginError::Fs(FsError::io(&root, err)))?;
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if keep.contains(&name.as_str()) {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                std::fs::remove_dir_all(&path)?;
            }
        }
        Ok(())
    }

    /// 切换到已安装的版本（`update --to <version>`、rollback 的底层操作）
    ///
    /// 目标版本目录必须存在且通过校验。进程运行中先停，切换后恢复运行。
    /// 授权按目标 manifest 重算：仍声明且已确认的保留，新出现的进
    /// `pending_approval`。`prune` 清掉除目标版本外的全部版本目录。
    ///
    /// # Errors
    ///
    /// 记录不存在、目标版本缺失、manifest / descriptor 非法、停机失败时返回
    /// [`PluginError`]
    pub fn activate_version(
        &self,
        id: &PluginId,
        version: &str,
        prune: bool,
    ) -> Result<PluginRecord, PluginError> {
        // 版本号会拼进路径：先按 SemVer 校验，`--to ../../..` 这类输入既不是
        // 合法版本，也不该被当成目录名。
        let version = version.trim();
        if !domain::plugin::manifest::semver_is_valid(version) {
            return Err(PluginError::Usage(format!(
                "`{version}` is not a SemVer version"
            )));
        }
        let mut record = self.record(id)?;
        let current = record.version.clone();
        if current == version {
            // 目标就是当前版本：写 `previous_version = 当前` 会把真正的回滚
            // 目标覆盖成同一个版本，`plugin rollback` 之后什么也没退。直接
            // 拒绝，顺带省掉一次无意义的停机 / 重启。
            return Err(PluginError::Conflict(format!(
                "{id} is already on version {version}"
            )));
        }
        let InstalledVersion {
            manifest,
            artifact,
            sha256,
        } = self.inspect_installed_version(id, version)?;
        let was_running = self.is_running(id);
        // 切换在改 `plugins/<id>/` 与记录，必须和 install / remove 串行。
        // 抢锁放在校验之后：校验只读，没必要串行。
        let _lock = self.plugin_lock(id)?;
        if was_running && let Some(supervisor) = &self.supervisor {
            supervisor.stop(id, Duration::from_secs(10))?;
        }
        // 授权重算：新版本仍声明且已确认的保留，低风险自动通过。
        let mut approved: BTreeSet<Capability> = manifest
            .capabilities
            .iter()
            .copied()
            .filter(|cap| cap.default_policy() == capability::DefaultPolicy::Allow)
            .collect();
        approved.extend(
            record
                .approved
                .iter()
                .copied()
                .filter(|cap| manifest.capabilities.contains(cap)),
        );
        let pending: BTreeSet<Capability> = manifest
            .capabilities
            .iter()
            .copied()
            .filter(|cap| !approved.contains(cap))
            .collect();
        // 待授权能力与 `enabled` 互斥（同 `install_into`）；随后 `last_state`
        // 按「是否启用 + 是否待授权」重算。原来无条件写 `Stopped` 会让
        // 已停用的插件切版本后既 `enable` 不了（`stopped + Enabled` 非法）
        // 又能绕过待审批门禁。
        let enabled = pending.is_empty() && record.enabled;
        // 回滚用：切换前的记录（版本、产物目录、授权都指回旧版本）
        let original = record.clone();
        version.clone_into(&mut record.version);
        record.previous_version = Some(current.clone());
        record.api = manifest.raw.api;
        record.manifest = manifest.raw.clone();
        record.artifact_dir = artifact;
        record.sha256 = Some(sha256);
        record.approved = approved;
        record.enabled = enabled;
        record.last_state = state_after_install(enabled, !pending.is_empty());
        record.last_error = None;
        record.updated_at = now_secs();
        self.store.put(&record)?;
        // 启动成功之前不清理任何版本目录：切换失败必须还能切回去
        if was_running
            && record.enabled
            && let Err(err) = self.start(id)
        {
            self.restore_after_failed_start(id, Some(&original), version, &err)?;
            return Err(err);
        }
        if prune {
            // 回滚目标是切换前的版本，任何情况下都不能删
            self.prune_versions(id, &[version, current.as_str()])?;
        }
        self.audit.record(&AuditEvent {
            at: now_secs(),
            plugin: id.to_string(),
            action: "activate".to_owned(),
            capability: None,
            result: AuditResult::Allowed,
            detail: Some(format!("version {}", record.version)),
        });
        Ok(record)
    }

    /// 校验盘上某个已安装版本可用：目录存在、摘要可重算、manifest 与入口合法
    fn inspect_installed_version(
        &self,
        id: &PluginId,
        version: &str,
    ) -> Result<InstalledVersion, PluginError> {
        let artifact = self.plugins_root.join(id.as_str()).join(version);
        if !artifact.is_dir() {
            let installed = self.installed_versions(id).unwrap_or_default();
            return Err(PluginError::Conflict(format!(
                "version {version} of {id} is not installed; installed: {}",
                if installed.is_empty() {
                    "none".to_owned()
                } else {
                    installed.join(", ")
                }
            )));
        }
        let verifier = self.verifier.as_ref().ok_or_else(|| {
            PluginError::Usage("no plugin artifact verifier is configured".to_owned())
        })?;
        let checked = verifier
            .verify_installed(&artifact, None)
            .map_err(|err| PluginError::Usage(format!("version {version} is not usable: {err}")))?;
        if checked.manifest.id != id.as_str() {
            return Err(PluginError::Usage(format!(
                "artifact in version {version} declares id {}",
                checked.manifest.id
            )));
        }
        let manifest = Manifest::from_json(
            &serde_json::to_vec(&checked.manifest)
                .map_err(|err| PluginError::Usage(format!("serialize manifest: {err}")))?,
        )
        .map_err(|err| PluginError::Usage(format!("invalid manifest: {err}")))?;
        // 目标版本的入口文件同样要过形态检查
        manifest
            .raw
            .entry
            .check_entry_file(&artifact, &domain::plugin::platform_key())
            .map_err(|err| PluginError::Usage(format!("entry check failed: {err}")))?;
        Ok(InstalledVersion {
            manifest,
            artifact,
            sha256: checked.sha256,
        })
    }

    /// 回滚到上一版本
    ///
    /// 与 [`Self::activate_version`] 是同一条路径：版本号、`artifact_dir`、
    /// manifest 入口与授权必须一起切——宿主按记录里的 manifest 找入口文件、
    /// 按 `approved` 算授权，只改版本号等于让宿主用新版本的入口跑旧产物。
    ///
    /// # Errors
    ///
    /// 没有上一版本、上一版本产物缺失时返回 [`PluginError`]
    pub fn rollback(&self, id: &PluginId) -> Result<PluginRecord, PluginError> {
        let record = self.record(id)?;
        let previous = record
            .previous_version
            .clone()
            .ok_or_else(|| PluginError::Conflict(format!("plugin {id} has no previous version")))?;
        let record = self.activate_version(id, &previous, false)?;
        self.audit.record(&AuditEvent {
            at: now_secs(),
            plugin: id.to_string(),
            action: "rollback".to_owned(),
            capability: None,
            result: AuditResult::Allowed,
            detail: Some(format!("active version {}", record.version)),
        });
        Ok(record)
    }

    /// 卸载：要求已停用且进程退出
    ///
    /// 默认连 `plugin-data/<id>/` 与日志一起删：卸载的语义是「这台机器上不再
    /// 有这个名字」，留下配置与键值只会在重装时变成「上一份数据还在」的意外
    /// 状态（配置指向的服务器、扫到的会话都过期了）。要重装后接着用时走
    /// [`Self::remove_with`] 的 [`RemoveData::Keep`]。
    ///
    /// # Errors
    ///
    /// 仍启用或进程仍在运行时返回 [`PluginError::Conflict`]
    pub fn remove(&self, id: &PluginId) -> Result<(), PluginError> {
        self.remove_with(id, RemoveData::Delete)
    }

    /// 卸载，并显式指定数据目录的去留
    ///
    /// 产物、记录、锁永远删；`data` 只决定 `plugin-data/<id>/` 与日志。
    ///
    /// # Errors
    ///
    /// 仍启用或进程仍在运行时返回 [`PluginError::Conflict`]
    pub fn remove_with(&self, id: &PluginId, data: RemoveData) -> Result<(), PluginError> {
        let record = self.record(id)?;
        if self.is_running(id) {
            return Err(PluginError::Conflict(format!(
                "plugin {id} is running; run `mcsup plugin disable {id}` first"
            )));
        }
        if record.enabled {
            return Err(PluginError::Conflict(format!(
                "plugin {id} is enabled; run `mcsup plugin disable {id}` first"
            )));
        }
        // 卸载在删 `plugins/<id>/` 与记录，必须和 install / update 串行。
        let _lock = self.plugin_lock(id)?;
        // 先删产物 / 数据 / 密钥，最后删记录。反过来一旦删除失败，记录已经
        // 没了而产物还在盘上：再装同版本永远 `AlreadyExists`、再 `remove` 又是
        // `NotFound`，只能手工删目录；密钥残留还会让重装的同名插件继承上一份
        // 凭据（`docs/plugins.md` §15.5）。保留记录才能重试。
        let artifact = self.plugins_root.join(id.as_str());
        if artifact.exists() {
            std::fs::remove_dir_all(&artifact)?;
        }
        if data.deletes_data() {
            let dir = self.plugin_data.join(id.as_str());
            if dir.exists() {
                std::fs::remove_dir_all(&dir)?;
            }
            let log = self.log_root.join(format!("{id}.log"));
            if log.exists() {
                std::fs::remove_file(&log)?;
            }
        } else if !data.keeps_secrets() {
            remove_secrets(&self.plugin_data, id)?;
        }
        self.store.remove(id)?;
        self.audit.record(&AuditEvent {
            at: now_secs(),
            plugin: id.to_string(),
            action: "remove".to_owned(),
            capability: None,
            result: AuditResult::Allowed,
            detail: Some(format!("data={data:?}")),
        });
        Ok(())
    }

    fn supervisor(&self) -> Result<&Arc<dyn PluginSupervisor>, PluginError> {
        self.supervisor.as_ref().ok_or_else(|| {
            PluginError::Host(
                "plugin host is not connected; run `mcsup plugin start <id>` on the host machine"
                    .to_owned(),
            )
        })
    }
}

/// 卸载时 `plugin-data/<id>/` 与日志的去留
///
/// 用枚举而不是两个独立布尔，是为了让「保留目录、却删掉目录里的密钥」这类
/// 组合显式表达出来：密钥本来就在数据目录里，`keep_secrets` 只有在保留目录时
/// 才说得通。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RemoveData {
    /// 连数据目录与日志一起删（默认）
    #[default]
    Delete,
    /// 保留数据目录与日志，只删产物与记录
    Keep {
        /// 是否连 `secrets.json` 一起留
        ///
        /// 默认 `false`：凭据不随同名插件重装继承。要重装后继续用同一凭据才
        /// 置 `true`。
        secrets: bool,
    },
}

impl RemoveData {
    /// 是否连 `plugin-data/<id>/` 与日志一起删
    #[must_use]
    pub const fn deletes_data(self) -> bool {
        matches!(self, Self::Delete)
    }

    /// 是否保留 `secrets.json`
    #[must_use]
    pub const fn keeps_secrets(self) -> bool {
        matches!(self, Self::Keep { secrets: true })
    }
}

/// 删除某个插件的密钥文件
///
/// 保留 `plugin-data/<id>/` 里的键值与配置，只删 `secrets.json`；目录因此变空
/// 时一并移除，免得留下空壳。密钥是凭据，卸载即失效——同名插件（可能是别人
/// 发布的同名包）重装后不该继承上一份凭据。
fn remove_secrets(plugin_data: &Path, id: &PluginId) -> Result<(), PluginError> {
    let dir = plugin_data.join(id.as_str());
    match std::fs::remove_file(dir.join("secrets.json")) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    // 目录里还有键值 / 配置时删不掉，`NotFound` 之外也不该报错：密钥已经删了。
    let _ = std::fs::remove_dir(&dir);
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flag {
    Blocked,
    Quarantined,
}

/// 配置读写：校验、合并、落盘、下发
///
/// 入口（CLI）与宿主控制通道共用这一份实现，避免「CLI 校验一遍、宿主再
/// 校验一遍」后两边规则分叉。
pub(crate) struct PluginConfig {
    store: Arc<dyn PluginStore>,
    config_store: Arc<dyn PluginConfigStore>,
    plugins_root: PathBuf,
    supervisor: Option<Arc<dyn PluginSupervisor>>,
    audit: Arc<dyn AuditSink>,
}

impl std::fmt::Debug for PluginConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginConfig")
            .field("plugins_root", &self.plugins_root)
            .finish_non_exhaustive()
    }
}

impl PluginConfig {
    /// 构造
    #[must_use]
    pub(crate) fn new(
        store: Arc<dyn PluginStore>,
        config_store: Arc<dyn PluginConfigStore>,
        plugins_root: PathBuf,
        supervisor: Option<Arc<dyn PluginSupervisor>>,
        audit: Arc<dyn AuditSink>,
    ) -> Self {
        Self {
            store,
            config_store,
            plugins_root,
            supervisor,
            audit,
        }
    }

    fn record(&self, id: &PluginId) -> Result<PluginRecord, HostError> {
        self.store.get(id).map_err(|err| match err {
            RepositoryError::NotFound(_) => HostError::not_found(format!("plugin {id} not found")),
            other => HostError::internal(format!("read plugin {id}: {other}")),
        })
    }

    /// 已安装版本的配置 schema
    fn schema(record: &PluginRecord) -> Result<Option<Value>, HostError> {
        let path = record.artifact_dir.join("descriptor.json");
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            // artifact 被手工删掉时按「无配置」处理；调用方会在写配置时拒绝。
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(HostError::internal(format!(
                    "read descriptor {}: {err}",
                    path.display()
                )));
            }
        };
        Descriptor::from_json(&bytes, &record.id)
            .map(|descriptor| descriptor.config)
            .map_err(|err| HostError::internal(format!("invalid descriptor: {err}")))
    }

    /// 当前值：schema 默认值打底，叠加已保存的值
    fn current(&self, id: &PluginId, schema: &Value) -> Result<StoredConfig, HostError> {
        let stored = self
            .config_store
            .get(id)
            .map_err(|err| HostError::internal(format!("read plugin config: {err}")))?
            .unwrap_or_default();
        let mut values = domain::plugin::schema::defaults(schema);
        if let (Some(base), Some(saved)) = (values.as_object_mut(), stored.values.as_object()) {
            for (key, value) in saved {
                base.insert(key.clone(), value.clone());
            }
        }
        Ok(StoredConfig {
            revision: stored.revision,
            values,
        })
    }
}

impl ConfigOps for PluginConfig {
    fn get(&self, id: &PluginId) -> Result<Value, HostError> {
        let record = self.record(id)?;
        let Some(schema) = Self::schema(&record)? else {
            return Ok(serde_json::json!({ "revision": 0, "values": {} }));
        };
        let current = self.current(id, &schema)?;
        Ok(serde_json::json!({
            "revision": current.revision,
            "values": domain::plugin::schema::mask_secrets(&schema, &current.values),
        }))
    }

    fn set(&self, id: &PluginId, values: &Value) -> Result<Value, HostError> {
        self.write(id, values, false)
    }

    fn merge(&self, id: &PluginId, values: &Value) -> Result<Value, HostError> {
        self.write(id, values, true)
    }
}

impl PluginConfig {
    /// 读—合并—校验—落盘—下发；`deep` 为 true 时递归合并对象
    fn write(&self, id: &PluginId, values: &Value, deep: bool) -> Result<Value, HostError> {
        let record = self.record(id)?;
        let schema = Self::schema(&record)?.ok_or_else(|| {
            HostError::unsupported(format!("plugin {id} does not declare a config schema"))
        })?;
        let patch = values
            .as_object()
            .ok_or_else(|| HostError::invalid_input("config values must be an object"))?;
        let previous = self.current(id, &schema)?;
        let mut merged = previous.values.clone();
        if merged.as_object().is_none() {
            return Err(HostError::internal("stored config is not an object"));
        }
        if deep {
            merge_value(&mut merged, values);
        } else {
            let object = merged
                .as_object_mut()
                .ok_or_else(|| HostError::internal("stored config is not an object"))?;
            for (key, value) in patch {
                object.insert(key.clone(), value.clone());
            }
        }
        domain::plugin::schema::validate_instance(&schema, &merged, "config")
            .map_err(|err| HostError::invalid_input(err.to_string()))?;
        let next = StoredConfig {
            revision: previous.revision + 1,
            values: merged.clone(),
        };
        self.config_store
            .put(id, &next)
            .map_err(|err| HostError::internal(format!("write plugin config: {err}")))?;
        if let Some(supervisor) = &self.supervisor {
            match supervisor.apply_config(id, next.revision, &merged) {
                Ok(()) => {}
                // 宿主不在：配置已落盘，下次握手经 `welcome.config` 下发。
                Err(err) if err.code == port::plugin::ErrorCode::Unavailable => {}
                Err(err) => {
                    self.rollback(id, &previous);
                    self.mark_degraded(id, &err);
                    return Err(err);
                }
            }
        }
        let changed = patch.keys().cloned().collect::<Vec<_>>().join(", ");
        let action = if deep { "config:merge" } else { "config:set" };
        self.audit.record(&AuditEvent {
            at: now_secs(),
            plugin: id.to_string(),
            action: action.to_owned(),
            capability: None,
            result: AuditResult::Allowed,
            // 审计只记键名，绝不记值。
            detail: Some(format!("revision {}, keys: {changed}", next.revision)),
        });
        Ok(serde_json::json!({
            "revision": next.revision,
            "values": domain::plugin::schema::mask_secrets(&schema, &next.values),
        }))
    }
}

impl PluginConfig {
    fn rollback(&self, id: &PluginId, previous: &StoredConfig) {
        // 写回失败只影响「保留上一版本」的强度，不能再把原始错误覆盖掉。
        let _ = self.config_store.put(id, previous);
    }

    fn mark_degraded(&self, id: &PluginId, err: &HostError) {
        let Ok(mut record) = self.store.get(id) else {
            return;
        };
        record.last_state = PluginState::Degraded;
        record.last_error = Some(format!("config apply failed: {}", err.message));
        record.updated_at = now_secs();
        let _ = self.store.put(&record);
    }
}

/// 深度合并：对象递归并入，数组与标量整体替换
fn merge_value(target: &mut Value, patch: &Value) {
    match (target, patch) {
        (Value::Object(base), Value::Object(patch)) => {
            for (key, value) in patch {
                match base.get_mut(key) {
                    Some(existing) => merge_value(existing, value),
                    None => {
                        base.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (target, patch) => *target = patch.clone(),
    }
}

/// 安装 / 升级后的记录状态
///
/// 升级会保留 `enabled`，所以仍启用的插件必须落到 `stopped`：`disabled` 与
/// `enabled=true` 同时存在时，`enable` 会因「已启用」报冲突、`start` 又会因
/// 「已停用」被状态机拒绝，插件再也起不来。
fn state_after_install(enabled: bool, needs_approval: bool) -> PluginState {
    if needs_approval {
        PluginState::PendingApproval
    } else if enabled {
        PluginState::Stopped
    } else {
        PluginState::Disabled
    }
}

/// 安装记录里的离线状态
fn offline_state(record: &PluginRecord) -> PluginState {
    if !record.enabled {
        return match record.last_state {
            PluginState::PendingApproval | PluginState::Failed => record.last_state,
            _ => PluginState::Disabled,
        };
    }
    match record.last_state {
        PluginState::Running
        | PluginState::Starting
        | PluginState::Stopping
        | PluginState::Degraded => PluginState::Stopped,
        other => other,
    }
}

fn pending_capabilities(record: &PluginRecord) -> String {
    record
        .declared_capabilities()
        .into_iter()
        .filter(|cap| cap.risk().needs_approval() && !record.approved.contains(cap))
        .map(|cap| cap.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// 盘上某个已安装版本的校验结果
#[derive(Debug)]
struct InstalledVersion {
    /// 解析通过的 manifest
    manifest: domain::plugin::manifest::ValidatedManifest,
    /// 产物目录
    artifact: PathBuf,
    /// 目录树摘要
    sha256: String,
}

/// 本次调用的临时解包目录：丢弃即删除
///
/// 只覆盖「正常返回」路径；`Drop` 走不到时（崩溃、断电）由
/// [`PluginService::sweep_stale_temp_dirs`] 按年龄兜底。
#[derive(Debug)]
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// `fetch_artifact` 的返回值：持有临时解包目录，丢弃时一并删除
#[derive(Debug)]
pub struct FetchedPluginArtifact {
    /// 已校验的产物
    pub artifact: PluginArtifact,
    /// 解包所在的临时目录（`Drop` 时删除）
    _temp: TempDir,
}

fn check_source_checksum(expected: Option<&Checksum>, actual: &str) -> Result<(), PluginError> {
    if let Some(expected) = expected
        && (expected.algorithm() != ChecksumAlgorithm::Sha256 || !expected.matches(actual))
    {
        return Err(PluginError::Usage(format!(
            "sha256 mismatch: expected {}, got {actual}",
            expected.hex()
        )));
    }
    Ok(())
}

fn origin_text(origin: &ArtifactOrigin) -> String {
    // 记录文本的格式只由 domain 的 `PluginSource` 定义：写入与读取共用一份，
    // 避免再次出现「写得进去、读不出来」（旧版 `git:` 记录写 `@` 读 `|`）。
    origin.record_text()
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}

/// 版本排序键：按 `major.minor.patch` 数值比较，其余部分退回字符串
///
/// 直接按字符串排会把 `0.10.0` 排到 `0.9.0` 前面。预发布/构建元数据只影响
/// 同核心版本内部的相对顺序，按原文比较即可。
fn version_key(version: &str) -> (u64, u64, u64, String) {
    let core = version
        .split(['-', '+'])
        .next()
        .unwrap_or(version)
        .to_owned();
    let mut parts = core.split('.').map(|part| part.parse::<u64>().unwrap_or(0));
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        version.to_owned(),
    )
}

// ---------------------------------------------------------------- 宿主能力

/// `server:read` / `console:read` 的宿主实现
///
/// 只读能力直接复用注册表、生命周期与属性服务；控制台写、备份、文件等能力
/// 在后续阶段按同一形状追加，未装配时宿主返回 `unsupported`。
pub struct HostApiImpl {
    registry: Arc<ServerRegistry>,
    lifecycle: Arc<ServerLifecycleService>,
    backup: BackupService,
    mods: ModService,
    java: JavaService,
    supervisor_exe: PathBuf,
    props: ServerPropsService,
    kv: Arc<dyn KvStore>,
    secrets: Arc<dyn SecretStore>,
    store: Arc<dyn PluginStore>,
}

impl std::fmt::Debug for HostApiImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostApiImpl").finish_non_exhaustive()
    }
}

/// 宿主能力实现的装配依赖
///
/// 单独成结构而不是一长串参数：每加一项能力都会再加一个入参，位置参数
/// 迟早会接错（全部是 `Arc`，编译器帮不上忙）。
pub struct HostApiDeps {
    /// 服务器注册表
    pub registry: Arc<ServerRegistry>,
    /// 生命周期编排
    pub lifecycle: Arc<ServerLifecycleService>,
    /// 备份编排
    pub backup: BackupService,
    /// mod / 内容编排
    pub mods: ModService,
    /// Java 运行时编排
    pub java: JavaService,
    /// 拉起 supervisor 用的可执行文件（宿主 `current_exe()`）
    pub supervisor_exe: PathBuf,
    /// 插件键值空间
    pub kv: Arc<dyn KvStore>,
    /// 插件密钥空间
    pub secrets: Arc<dyn SecretStore>,
    /// 插件记录
    pub store: Arc<dyn PluginStore>,
}

impl std::fmt::Debug for HostApiDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostApiDeps")
            .field("supervisor_exe", &self.supervisor_exe)
            .finish_non_exhaustive()
    }
}

impl HostApiImpl {
    /// 构造宿主能力实现
    #[must_use]
    pub fn new(deps: HostApiDeps) -> Self {
        let HostApiDeps {
            registry,
            lifecycle,
            backup,
            mods,
            java,
            supervisor_exe,
            kv,
            secrets,
            store,
        } = deps;
        Self {
            props: ServerPropsService::new(Arc::clone(&registry)),
            registry,
            lifecycle,
            backup,
            mods,
            java,
            supervisor_exe,
            kv,
            secrets,
            store,
        }
    }

    /// 键值实现（装配到 `HostServices::kv`）
    #[must_use]
    pub fn kv(&self) -> Arc<dyn KvStore> {
        Arc::clone(&self.kv)
    }

    /// 密钥实现（装配到 `HostServices::secrets`）
    #[must_use]
    pub fn secrets(&self) -> Arc<dyn SecretStore> {
        Arc::clone(&self.secrets)
    }
}

impl NetPolicy for HostApiImpl {
    fn allowed_hosts(&self, plugin: &PluginId) -> Result<Vec<String>, HostError> {
        let record = self.store.get(plugin).map_err(|err| match err {
            RepositoryError::NotFound(message) => {
                HostError::not_found(format!("plugin {plugin} not found: {message}"))
            }
            other => HostError::internal(format!("read plugin {plugin}: {other}")),
        })?;
        Ok(record
            .manifest
            .net
            .as_ref()
            .map(|net| net.allow.clone())
            .unwrap_or_default())
    }
}

impl ServerRead for HostApiImpl {
    fn list(&self) -> Result<Vec<ServerSummary>, HostError> {
        let servers = self
            .registry
            .list()
            .map_err(|err| HostError::internal(format!("list servers: {err}")))?;
        let statuses = self
            .lifecycle
            .statuses()
            .map_err(|err| HostError::internal(format!("server statuses: {err}")))?;
        Ok(servers
            .into_iter()
            .map(|server| {
                let status = statuses
                    .get(server.meta.name.as_str())
                    .and_then(|result| result.as_ref().ok())
                    .and_then(Option::as_ref)
                    .map_or(ServerStatus::Stopped, |status| status.status);
                ServerSummary {
                    name: server.meta.name.clone(),
                    status,
                    version: Some(server.meta.version.clone()),
                }
            })
            .collect())
    }

    fn status(&self, server: &ServerName) -> Result<ServerStatusView, HostError> {
        let registered = self
            .registry
            .get(server.as_str())
            .map_err(|err| match err {
                ServerRegistryError::NotFound(_) => {
                    HostError::not_found(format!("server {server} not found"))
                }
                other => HostError::internal(format!("read server {server}: {other}")),
            })?;
        let status = self
            .lifecycle
            .status(server.as_str())
            .map_err(|err| HostError::internal(format!("server {server} status: {err}")))?
            .map_or(ServerStatus::Stopped, |status| status.status);
        Ok(ServerStatusView {
            status,
            version: Some(registered.meta.version.clone()),
            players: None,
            uptime: None,
        })
    }

    fn version(
        &self,
        server: &ServerName,
    ) -> Result<Option<domain::server::version::VersionId>, HostError> {
        let registered = self
            .registry
            .get(server.as_str())
            .map_err(|err| match err {
                ServerRegistryError::NotFound(_) => {
                    HostError::not_found(format!("server {server} not found"))
                }
                other => HostError::internal(format!("read server {server}: {other}")),
            })?;
        Ok(Some(registered.meta.version.clone()))
    }

    fn properties(
        &self,
        server: &ServerName,
    ) -> Result<std::collections::BTreeMap<String, String>, HostError> {
        let report = self
            .props
            .read(server.as_str())
            .map_err(|err| HostError::internal(format!("read properties: {err}")))?;
        Ok(report
            .properties
            .entries()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect())
    }
}

impl ConsoleIo for HostApiImpl {
    fn tail(
        &self,
        server: &ServerName,
        cursor: Option<u64>,
        max_lines: usize,
    ) -> Result<ConsolePage, HostError> {
        if max_lines == 0 || max_lines > MAX_CONSOLE_LINES {
            return Err(HostError::limit_exceeded(format!(
                "max_lines must be 1..={MAX_CONSOLE_LINES}"
            )));
        }
        let registered = self
            .registry
            .get(server.as_str())
            .map_err(|err| match err {
                ServerRegistryError::NotFound(_) => {
                    HostError::not_found(format!("server {server} not found"))
                }
                other => HostError::internal(format!("read server {server}: {other}")),
            })?;
        let path = console_log_path(&registered.dir);
        // 只读一页所需的窗口：`max_lines` 行长上限之和，再夹到日志尾部读取
        // 的总上限。整文件读在繁忙服务器上会把宿主内存打满（一行不换行就
        // 能写到几百 MB），而调用方要的只是最后几百行。
        let want = max_lines
            .saturating_mul(MAX_CONSOLE_LINE_BYTES)
            .min(domain::limits::MAX_LOG_TAIL_BYTES);
        // 游标是字节偏移：单调递增、可跨进程复用，且无需额外状态文件。
        let window = match cursor {
            Some(start) => domain::log::Window::from(&path, start, want),
            None => domain::log::Window::tail(&path, want),
        }
        .map_err(|err| HostError::internal(format!("read console log: {err}")))?;
        let mut lines = window.lines();
        // 从文件中间起读时首行是半行；整行都不完整（超长行没有换行）时保留
        // 截断内容，不能因为「半行」把它丢光。
        if cursor.is_none() && window.truncated() && lines.len() > 1 {
            lines.remove(0);
        }
        if lines.len() > max_lines {
            let excess = lines.len() - max_lines;
            lines.drain(..excess);
        }
        let next = lines.last().map_or_else(|| window.end(), |(_, end)| *end);
        Ok(ConsolePage {
            lines: lines
                .into_iter()
                .map(|(text, cursor)| ConsoleLine { cursor, text })
                .collect(),
            next,
        })
    }

    fn send_line(&self, server: &ServerName, line: &str) -> Result<(), HostError> {
        if line.len() > MAX_CONSOLE_LINE_BYTES {
            return Err(HostError::limit_exceeded(format!(
                "console line is {} bytes; limit is {MAX_CONSOLE_LINE_BYTES}",
                line.len()
            )));
        }
        self.lifecycle
            .send_line(server.as_str(), line)
            .map_err(|err| HostError::conflict(format!("inject console line: {err}")))
    }
}

/// 文件能力的服务器根：canonical 根 + 运行期保护信息
///
/// 所有落点都先解析符号链接再与根比较；比较用 canonical 路径，Windows 的
/// 大小写与实际目录名由系统解析，字符串前缀判断不承担归一化职责。
struct ServerFiles {
    root: PathBuf,
    /// 进程是否仍持有启动文件（`Starting` / `Running` / `Stopping`）
    running: bool,
    /// 运行期不可覆盖的启动文件，相对服务器目录
    launch: Option<PathBuf>,
}

impl ServerFiles {
    fn new(dir: &Path, running: bool, launch: Option<PathBuf>) -> Result<Self, HostError> {
        let root = crate::fs::canonicalize(dir).map_err(|err| io_error(dir, &err))?;
        Ok(Self {
            root,
            running,
            launch,
        })
    }

    fn relative(rel: &Path) -> Result<RelPath, HostError> {
        // 插件传来的路径是文本（帧里的 POSIX 路径），不是本机路径对象：
        // `from_path` 在 Windows 上会把 `\` 当分隔符切开，`world\level.dat`
        // 就静默变成了 `world/level.dat`。文本一律按 POSIX 规则解析，
        // `\` 在任何平台都是非法字符。
        RelPath::parse(&rel.to_string_lossy()).map_err(|err| HostError::denied(err.to_string()))
    }

    /// 已存在路径：解析符号链接后必须仍在根内
    fn checked_existing(&self, rel: &RelPath) -> Result<PathBuf, HostError> {
        let target = self.root.join(rel.to_platform_path());
        let resolved = crate::fs::canonicalize(&target).map_err(|err| io_error(&target, &err))?;
        self.ensure_inside(&resolved, rel)?;
        Ok(target)
    }

    fn ensure_inside(&self, resolved: &Path, rel: &RelPath) -> Result<(), HostError> {
        if resolved.starts_with(&self.root) {
            Ok(())
        } else {
            Err(HostError::denied(format!(
                "path escapes the server directory: {rel}"
            )))
        }
    }

    /// 写入落点：缺的父目录逐级建出来
    ///
    /// 先找到最深的已存在祖先并确认它在根内，再创建缺失目录——顺序反过来
    /// 会在检测到逃逸之前就把目录建到根外。
    fn writable(&self, rel: &RelPath) -> Result<PathBuf, HostError> {
        let target = self.root.join(rel.to_platform_path());
        let parent = target
            .parent()
            .ok_or_else(|| HostError::denied(format!("path has no parent: {rel}")))?;
        let mut ancestor = parent;
        let mut missing = Vec::new();
        loop {
            match crate::fs::symlink_metadata(ancestor) {
                Ok(_) => break,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    let name = ancestor.file_name().ok_or_else(|| {
                        HostError::denied(format!("path escapes the root: {rel}"))
                    })?;
                    missing.push(name.to_owned());
                    ancestor = ancestor.parent().ok_or_else(|| {
                        HostError::denied(format!("path escapes the root: {rel}"))
                    })?;
                }
                Err(err) => return Err(io_error(ancestor, &err)),
            }
        }
        let resolved = crate::fs::canonicalize(ancestor).map_err(|err| io_error(ancestor, &err))?;
        self.ensure_inside(&resolved, rel)?;
        let mut dir = resolved;
        for name in missing.iter().rev() {
            dir.push(name);
            crate::fs::create_dir_all(&dir).map_err(|err| io_error(&dir, &err))?;
        }
        // 目标本身可能是符号链接：写入会跟随它，落点同样要校验。
        match crate::fs::symlink_metadata(&target) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let resolved = crate::fs::canonicalize(&target).map_err(|err| {
                    if err.kind() == std::io::ErrorKind::NotFound {
                        HostError::denied(format!(
                            "refusing to write through a dangling symbolic link: {rel}"
                        ))
                    } else {
                        io_error(&target, &err)
                    }
                })?;
                self.ensure_inside(&resolved, rel)?;
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(io_error(&target, &err)),
        }
        Ok(target)
    }

    /// 写 / 删前的保留名与运行期检查
    fn check_mutation(&self, rel: &RelPath) -> Result<(), HostError> {
        if rel.as_str().eq_ignore_ascii_case(MARKER_FILE) {
            return Err(HostError::denied(format!(
                "{MARKER_FILE} is reserved for server metadata"
            )));
        }
        if self.running
            && self
                .launch
                .as_deref()
                .is_some_and(|launch| same_path(rel, launch))
        {
            return Err(HostError::conflict(format!(
                "cannot modify the running server file: {rel}"
            )));
        }
        Ok(())
    }

    fn read(&self, rel: &Path) -> Result<Vec<u8>, HostError> {
        let rel = Self::relative(rel)?;
        let target = self.checked_existing(&rel)?;
        let meta = crate::fs::metadata(&target).map_err(|err| io_error(&target, &err))?;
        if !meta.is_file() {
            return Err(HostError::conflict(format!("not a regular file: {rel}")));
        }
        if meta.len() > MAX_FILE_BYTES as u64 {
            return Err(limit_error(rel.as_str(), meta.len(), MAX_FILE_BYTES));
        }
        let file = crate::fs::open(&target).map_err(|err| io_error(&target, &err))?;
        let mut bytes = Vec::new();
        file.take(MAX_FILE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|err| io_error(&target, &err))?;
        if bytes.len() > MAX_FILE_BYTES {
            return Err(limit_error(
                rel.as_str(),
                bytes.len() as u64,
                MAX_FILE_BYTES,
            ));
        }
        Ok(bytes)
    }

    fn write(&self, rel: &Path, bytes: &[u8], overwrite: bool) -> Result<(), HostError> {
        if bytes.len() > MAX_FILE_BYTES {
            return Err(limit_error("write", bytes.len() as u64, MAX_FILE_BYTES));
        }
        let rel = Self::relative(rel)?;
        self.check_mutation(&rel)?;
        let target = self.writable(&rel)?;
        if !overwrite && crate::fs::symlink_metadata(&target).is_ok() {
            return Err(HostError::conflict(format!("file already exists: {rel}")));
        }
        crate::fs::write(&target, bytes).map_err(|err| io_error(&target, &err))
    }

    fn list(&self, rel_dir: &Path) -> Result<Vec<FileEntry>, HostError> {
        let (dir, prefix) = if rel_dir.as_os_str().is_empty() || rel_dir == Path::new(".") {
            (self.root.clone(), String::new())
        } else {
            let rel = Self::relative(rel_dir)?;
            (self.checked_existing(&rel)?, rel.as_str().to_owned())
        };
        let meta = crate::fs::metadata(&dir).map_err(|err| io_error(&dir, &err))?;
        if !meta.is_dir() {
            return Err(HostError::conflict(format!(
                "not a directory: {}",
                dir.display()
            )));
        }
        let entries = crate::fs::read_dir(&dir).map_err(|err| io_error(&dir, &err))?;
        let mut out = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|err| io_error(&dir, &err))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let text = if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}/{name}")
            };
            // 跨平台不可表示的条目直接跳过：返回给插件的路径必须能再传回来。
            let Ok(path) = RelPath::parse(&text) else {
                continue;
            };
            let path_on_disk = entry.path();
            let meta = crate::fs::symlink_metadata(&path_on_disk)
                .map_err(|err| io_error(&path_on_disk, &err))?;
            let is_dir = meta.file_type().is_dir();
            let size = if meta.file_type().is_file() {
                meta.len()
            } else {
                0
            };
            out.push(FileEntry {
                path: path.to_platform_path(),
                is_dir,
                size,
            });
        }
        out.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(out)
    }

    fn remove(&self, rel: &Path) -> Result<bool, HostError> {
        let rel = Self::relative(rel)?;
        self.check_mutation(&rel)?;
        let target = self.root.join(rel.to_platform_path());
        let meta = match crate::fs::symlink_metadata(&target) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(io_error(&target, &err)),
        };
        // 父目录仍要校验：符号链接目录能把条目指到根外。
        let parent = target
            .parent()
            .ok_or_else(|| HostError::denied(format!("path has no parent: {rel}")))?;
        let resolved = crate::fs::canonicalize(parent).map_err(|err| io_error(parent, &err))?;
        self.ensure_inside(&resolved, &rel)?;
        if meta.file_type().is_symlink() {
            remove_link(&target).map_err(|err| io_error(&target, &err))?;
        } else if meta.is_dir() {
            crate::fs::remove_dir(&target).map_err(|err| io_error(&target, &err))?;
        } else {
            crate::fs::remove_file(&target).map_err(|err| io_error(&target, &err))?;
        }
        Ok(true)
    }
}

/// 服务端身份文件：插件不得覆盖或删除
const MARKER_FILE: &str = "mcsup.json";

fn same_path(rel: &RelPath, raw: &Path) -> bool {
    RelPath::from_path(raw).is_ok_and(|path| path.as_str().eq_ignore_ascii_case(rel.as_str()))
}

fn limit_error(what: &str, actual: u64, limit: usize) -> HostError {
    HostError::limit_exceeded(format!("{what} is {actual} bytes; limit is {limit}"))
}

fn io_error(path: &Path, err: &std::io::Error) -> HostError {
    use std::io::ErrorKind;
    let message = format!("{}: {err}", path.display());
    match err.kind() {
        ErrorKind::NotFound => HostError::not_found(message),
        ErrorKind::PermissionDenied => HostError::denied(message),
        ErrorKind::AlreadyExists
        | ErrorKind::IsADirectory
        | ErrorKind::NotADirectory
        | ErrorKind::DirectoryNotEmpty => HostError::conflict(message),
        _ => HostError::internal(message),
    }
}

/// 删除符号链接本身；Windows 的目录链接必须走 `remove_dir`
fn remove_link(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    if crate::fs::metadata(path).is_ok_and(|meta| meta.is_dir()) {
        return crate::fs::remove_dir(path);
    }
    crate::fs::remove_file(path)
}

impl FileOps for HostApiImpl {
    fn read(&self, server: &ServerName, rel: &Path) -> Result<Vec<u8>, HostError> {
        self.files(server)?.read(rel)
    }

    fn write(
        &self,
        server: &ServerName,
        rel: &Path,
        bytes: &[u8],
        overwrite: bool,
    ) -> Result<(), HostError> {
        self.mutable_files(server)?.write(rel, bytes, overwrite)
    }

    fn list(&self, server: &ServerName, rel_dir: &Path) -> Result<Vec<FileEntry>, HostError> {
        self.files(server)?.list(rel_dir)
    }

    fn remove(&self, server: &ServerName, rel: &Path) -> Result<bool, HostError> {
        self.mutable_files(server)?.remove(rel)
    }
}

impl HostApiImpl {
    /// 只读文件视图；不探测进程状态，读文件不该依赖 supervisor 可达
    fn files(&self, server: &ServerName) -> Result<ServerFiles, HostError> {
        self.build_files(server, false)
    }

    /// 写 / 删文件视图；需要进程状态来判断启动文件是否被占用
    fn mutable_files(&self, server: &ServerName) -> Result<ServerFiles, HostError> {
        self.build_files(server, true)
    }

    fn build_files(
        &self,
        server: &ServerName,
        protect_running: bool,
    ) -> Result<ServerFiles, HostError> {
        let registered = self
            .registry
            .get(server.as_str())
            .map_err(|err| match err {
                ServerRegistryError::NotFound(_) => {
                    HostError::not_found(format!("server {server} not found"))
                }
                other => HostError::internal(format!("read server {server}: {other}")),
            })?;
        let running = if protect_running {
            let status = self
                .lifecycle
                .status(server.as_str())
                .map_err(|err| HostError::internal(format!("server {server} status: {err}")))?
                .map(|status| status.status);
            matches!(
                status,
                Some(ServerStatus::Starting | ServerStatus::Running | ServerStatus::Stopping)
            )
        } else {
            false
        };
        let launch = match registered.meta.launch.as_ref() {
            Some(spec) => Some(spec.primary_file().to_path_buf()),
            None => Some(
                registered
                    .meta
                    .jar
                    .as_ref()
                    .map_or_else(|| PathBuf::from("server.jar"), |jar| jar.path.clone()),
            ),
        };
        ServerFiles::new(&registered.dir, running, launch)
    }
}

impl ServerLifecycle for HostApiImpl {
    fn start(&self, server: &ServerName) -> Result<(), HostError> {
        let options = plugin_start_options(&self.supervisor_exe);
        self.lifecycle
            .start(server.as_str(), &options, &mut || {}, &mut |_| {})
            .map(|_| ())
            .map_err(|err| lifecycle_host_error(&err))
    }

    fn stop(&self, server: &ServerName, grace: Duration) -> Result<StopOutcome, HostError> {
        let spec = StopSpec {
            grace,
            force: false,
        };
        self.lifecycle
            .stop(server.as_str(), spec)
            .map_err(|err| lifecycle_host_error(&err))
    }

    fn restart(&self, server: &ServerName, grace: Duration) -> Result<(), HostError> {
        let options = plugin_start_options(&self.supervisor_exe);
        let spec = StopSpec {
            grace,
            force: false,
        };
        self.lifecycle
            .restart(server.as_str(), &options, spec, &mut || {}, &mut |_| {})
            .map(|_| ())
            .map_err(|err| lifecycle_host_error(&err))
    }
}

/// 插件触发的启动不接受 EULA 条款：那必须由管理员显式确认
fn plugin_start_options(supervisor_exe: &Path) -> StartOptions {
    StartOptions {
        accept_eula: false,
        skip_mod_check: false,
        ram_mib: None,
        supervisor_exe: supervisor_exe.to_path_buf(),
        supervisor_args: vec![std::ffi::OsString::from("__supervise")],
    }
}

/// 生命周期错误 → 宿主错误
///
/// 插件无法修复的服务器状态（缺核心、EULA 未确认、已在运行）一律是
/// `conflict`：调用方要改的是服务器状态，不是重试或改参数。
fn lifecycle_host_error(err: &LifecycleError) -> HostError {
    use LifecycleError as E;
    let message = err.to_string();
    match err {
        E::Registry(ServerRegistryError::NotFound(_)) => HostError::not_found(message),
        E::NoJar { .. }
        | E::EulaRequired
        | E::AlreadyRunning
        | E::NotRunning
        | E::Busy { .. }
        | E::ModsBlocked { .. } => HostError::conflict(message),
        E::NoJava | E::UnknownJavaMajor { .. } => {
            HostError::new(PluginErrorCode::Unavailable, message)
        }
        E::StopTimedOut { .. } => HostError::new(PluginErrorCode::Timeout, message),
        E::RemoteLogStream(_) => HostError::unsupported(message),
        E::Registry(_)
        | E::Java(_)
        | E::Supervisor(_)
        | E::InstallJar(_)
        | E::Io(_)
        | E::StatusCountMismatch { .. } => HostError::internal(message),
    }
}

impl BackupOps for HostApiImpl {
    fn create(&self, server: &ServerName, label: Option<&str>) -> Result<String, HostError> {
        let options = CreateOptions {
            label: label.map(str::to_owned),
            ..CreateOptions::default()
        };
        let cancel = port::CancellationToken::new();
        let outcome = self
            .backup
            .create(server.as_str(), &options, &cancel)
            .map_err(|err| backup_host_error(&err))?;
        Ok(outcome.snapshot.id.to_string())
    }

    fn list(&self, server: &ServerName) -> Result<Vec<BackupEntry>, HostError> {
        let page = self
            .backup
            .list(server.as_str(), Page::new(1, usize::MAX))
            .map_err(|err| backup_host_error(&err))?;
        Ok(page
            .items
            .into_iter()
            .map(|snapshot| BackupEntry {
                id: snapshot.id.to_string(),
                created_at: snapshot.created_at,
                added_bytes: snapshot.added_bytes,
            })
            .collect())
    }

    fn restore(&self, server: &ServerName, snapshot: &str) -> Result<(), HostError> {
        let cancel = port::CancellationToken::new();
        self.backup
            .restore(server.as_str(), snapshot, &RestoreTarget::InPlace, &cancel)
            .map(|_| ())
            .map_err(|err| backup_host_error(&err))
    }

    fn verify(&self, server: &ServerName, snapshot: &str) -> Result<bool, HostError> {
        self.backup
            .verify(server.as_str(), snapshot, &mut |_, _| {})
            .map(|outcome| outcome.is_healthy())
            .map_err(|err| backup_host_error(&err))
    }

    fn prune(&self, server: &ServerName, keep: usize) -> Result<usize, HostError> {
        self.backup
            .prune(
                server.as_str(),
                RetentionInput {
                    keep: Some(keep),
                    older_than_secs: None,
                },
                false,
            )
            .map(|outcome| outcome.removed.len())
            .map_err(|err| backup_host_error(&err))
    }

    fn delete(&self, server: &ServerName, snapshot: &str) -> Result<bool, HostError> {
        self.backup
            .delete(server.as_str(), snapshot)
            .map(|_| true)
            .map_err(|err| backup_host_error(&err))
    }
}

/// 备份错误 → 宿主错误：分类由 service 决定，这里只翻译成稳定码
fn backup_host_error(err: &BackupError) -> HostError {
    let message = err.to_string();
    match err.category() {
        ErrorCategory::NotFound => HostError::not_found(message),
        ErrorCategory::Conflict => HostError::conflict(message),
        ErrorCategory::Cancelled => HostError::new(PluginErrorCode::Cancelled, message),
        ErrorCategory::Usage => HostError::invalid_input(message),
        ErrorCategory::Backend => HostError::new(PluginErrorCode::Unavailable, message),
        ErrorCategory::Unauthorized => HostError::denied(message),
        ErrorCategory::Failure => HostError::internal(message),
    }
}

impl ContentOps for HostApiImpl {
    fn list(
        &self,
        server: &ServerName,
        kind: ContentKind,
        identify: bool,
    ) -> Result<Vec<ContentEntry>, HostError> {
        let list = with_content_runtime(|runtime| {
            runtime.block_on(self.mods.list(
                server.as_str(),
                ListOptions {
                    kind,
                    identify,
                    source: None,
                    updates: false,
                },
            ))
        })?
        .map_err(|err| content_host_error(&err))?;
        let dir = list.dir;
        Ok(list
            .mods
            .into_iter()
            .map(|entry| {
                // 大小只用于展示；读不到时留 0，不因单个文件阻断整次列表
                //（读不出来的文件已在 `unreadable` 里单独报告）。
                let size = std::fs::metadata(dir.join(&entry.file)).map_or(0, |meta| meta.len());
                ContentEntry {
                    identity: entry.identity,
                    file_name: entry.file,
                    size,
                }
            })
            .collect())
    }

    fn install(
        &self,
        server: &ServerName,
        request: &ContentInstallRequest,
    ) -> Result<ContentEntry, HostError> {
        // 计划与执行都走 `ModService`：运行态判定、依赖检查与落点规则不在
        // 插件层重写，插件拿到的结果与 CLI 完全一致。
        let plan = with_content_runtime(|runtime| {
            runtime.block_on(self.mods.plan_install(
                server.as_str(),
                &request.project,
                request.kind,
                InstallSpec {
                    version: request.version.as_deref(),
                    source: None,
                    allow_prerelease: false,
                },
            ))
        })?
        .map_err(|err| content_host_error(&err))?;
        let cancel = port::CancellationToken::new();
        let installed = with_content_runtime(|runtime| {
            runtime.block_on(self.mods.install(
                server.as_str(),
                &plan,
                ApplyOptions {
                    allow_blocking: false,
                    progress: None,
                    cancel: &cancel,
                },
            ))
        })?
        .map_err(|err| content_host_error(&err))?;
        Ok(ContentEntry {
            // 安装结果只承诺落盘事实，不额外访问上游反查身份：那属于
            // `list(identify=true)` 的职责。
            identity: None,
            file_name: installed.file,
            size: installed.size,
        })
    }

    fn remove(
        &self,
        server: &ServerName,
        selector: &str,
        kind: ContentKind,
    ) -> Result<usize, HostError> {
        self.mods
            .remove(server.as_str(), selector, kind, false)
            .map(|removed| removed.files.len())
            .map_err(|err| content_host_error(&err))
    }
}

/// 同步宿主线程上跑 `ModService` 的异步方法
///
/// 插件会话的每次宿主调用都在自己的线程上执行，这里按线程复用 runtime：
/// 它不是业务状态，只是 IO / 定时器驱动。`enable_all` 必须带上 tokio 的
/// `net` feature，否则下载用的 IO driver 不会被启用。
fn with_content_runtime<T>(
    work: impl FnOnce(&tokio::runtime::Runtime) -> T,
) -> Result<T, HostError> {
    thread_local! {
        static CONTENT_RUNTIME: std::cell::OnceCell<tokio::runtime::Runtime> =
            const { std::cell::OnceCell::new() };
    }
    CONTENT_RUNTIME.with(|cell| {
        if cell.get().is_none() {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|err| HostError::internal(format!("init content runtime: {err}")))?;
            let _ = cell.set(runtime);
        }
        cell.get()
            .map(work)
            .ok_or_else(|| HostError::internal("content runtime was not initialized"))
    })
}

/// mod 编排错误 → 宿主错误
///
/// 分类仍由 `ModError::category()` 决定；只有「服务器在远端」需要单独翻成
/// `unsupported`：插件连不上另一台主机，重试或改参数都没用。
fn content_host_error(err: &ModError) -> HostError {
    let message = err.to_string();
    if matches!(err, ModError::Remote { .. }) {
        return HostError::unsupported(message);
    }
    match err.category() {
        ErrorCategory::NotFound => HostError::not_found(message),
        ErrorCategory::Conflict => HostError::conflict(message),
        ErrorCategory::Cancelled => HostError::new(PluginErrorCode::Cancelled, message),
        ErrorCategory::Usage => HostError::invalid_input(message),
        ErrorCategory::Backend => HostError::new(PluginErrorCode::Unavailable, message),
        ErrorCategory::Unauthorized => HostError::denied(message),
        ErrorCategory::Failure => HostError::internal(message),
    }
}

impl JavaOps for HostApiImpl {
    fn list(&self) -> Result<Vec<JavaRuntime>, HostError> {
        self.java.list().map_err(|err| java_host_error(&err))
    }

    fn install(&self, major: JavaMajor) -> Result<JavaRuntime, HostError> {
        // 插件协议只给大版本：包类型固定 JDK（托管目录名与内容由它决定），
        // 厂商交给来源排序挑最优，与 CLI `java install` 的默认路径一致。
        let cancel = port::CancellationToken::new();
        with_content_runtime(|runtime| {
            runtime.block_on(
                self.java
                    .install(major, JavaPackageType::Jdk, None, None, &cancel),
            )
        })?
        .map(|outcome| outcome.runtime)
        .map_err(|err| java_host_error(&err))
    }

    fn remove(&self, path: &Path) -> Result<bool, HostError> {
        self.java
            .uninstall(path)
            .map(|_| true)
            .map_err(|err| java_host_error(&err))
    }
}

/// Java 编排错误 → 宿主错误
///
/// 分类由 [`JavaError::category`] 决定：未登记 → `not_found`，上游 / 下载 /
/// 磁盘 → `unavailable`（可重试），参数问题 → `invalid_input`。
fn java_host_error(err: &JavaError) -> HostError {
    let message = err.to_string();
    match err.category() {
        ErrorCategory::NotFound => HostError::not_found(message),
        ErrorCategory::Conflict => HostError::conflict(message),
        ErrorCategory::Cancelled => HostError::new(PluginErrorCode::Cancelled, message),
        ErrorCategory::Usage => HostError::invalid_input(message),
        ErrorCategory::Backend => HostError::new(PluginErrorCode::Unavailable, message),
        ErrorCategory::Unauthorized => HostError::denied(message),
        ErrorCategory::Failure => HostError::internal(message),
    }
}

/// 后续阶段的能力实现占位：装配到的能力若阶段未开放，宿主返回 `unsupported`
///
/// 用空实现而不是 `None` 是为了让装配方一眼看出「已显式关闭」，不是漏配。
#[derive(Debug, Clone, Copy, Default)]
pub struct UnavailableLifecycle;

impl ServerLifecycle for UnavailableLifecycle {}

/// 备份能力占位
#[derive(Debug, Clone, Copy, Default)]
pub struct UnavailableBackup;

impl BackupOps for UnavailableBackup {}

/// 内容能力占位
#[derive(Debug, Clone, Copy, Default)]
pub struct UnavailableContent;

impl ContentOps for UnavailableContent {}

/// Java 能力占位
#[derive(Debug, Clone, Copy, Default)]
pub struct UnavailableJava;

impl JavaOps for UnavailableJava {}

/// 文件能力占位
#[derive(Debug, Clone, Copy, Default)]
pub struct UnavailableFiles;

impl FileOps for UnavailableFiles {}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::plugin::PluginScope;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// 安装记录里的来源文本与 domain 的解析必须成对
    ///
    /// 旧版 provider 写 `git:<url>@<commit>`、CLI 读 `git:<url>|<rev>`，写进去
    /// 的记录永远读不出来，`plugin update` 因此对 git 来源彻底失效。这里锁住
    /// 「写出来的文本能被 `PluginSource::parse_record` 原样读回」。
    #[test]
    fn recorded_source_text_round_trips_through_the_domain_parser() {
        use domain::plugin::source::PluginSource;
        for origin in [
            ArtifactOrigin::Local(PathBuf::from("/opt/plugins/demo")),
            ArtifactOrigin::Url("https://example.com/plugin.zip".to_owned()),
            ArtifactOrigin::Github {
                owner: "octocat".to_owned(),
                repo: "Hello-World".to_owned(),
                rev: None,
            },
            ArtifactOrigin::Github {
                owner: "octocat".to_owned(),
                repo: "Hello-World".to_owned(),
                rev: Some("dist".to_owned()),
            },
        ] {
            let text = origin_text(&origin);
            let parsed = PluginSource::parse_record(&text).expect(&text);
            assert_eq!(ArtifactOrigin::from(parsed), origin, "{text}");
        }
    }

    #[derive(Default)]
    struct MemoryStore {
        records: Mutex<BTreeMap<String, PluginRecord>>,
    }

    /// 测试用配置存储：内存
    #[derive(Default)]
    struct MemoryConfigStore {
        configs: Mutex<BTreeMap<String, StoredConfig>>,
    }

    impl PluginConfigStore for MemoryConfigStore {
        fn get(&self, id: &PluginId) -> Result<Option<StoredConfig>, RepositoryError> {
            Ok(self.configs.lock().expect("lock").get(id.as_str()).cloned())
        }

        fn put(&self, id: &PluginId, config: &StoredConfig) -> Result<(), RepositoryError> {
            self.configs
                .lock()
                .expect("lock")
                .insert(id.to_string(), config.clone());
            Ok(())
        }
    }

    impl PluginStore for MemoryStore {
        fn list(&self) -> Result<Vec<PluginRecord>, RepositoryError> {
            Ok(self
                .records
                .lock()
                .expect("lock")
                .values()
                .cloned()
                .collect())
        }

        fn get(&self, id: &PluginId) -> Result<PluginRecord, RepositoryError> {
            self.records
                .lock()
                .expect("lock")
                .get(id.as_str())
                .cloned()
                .ok_or_else(|| RepositoryError::NotFound(id.to_string()))
        }

        fn put(&self, record: &PluginRecord) -> Result<(), RepositoryError> {
            self.records
                .lock()
                .expect("lock")
                .insert(record.id.to_string(), record.clone());
            Ok(())
        }

        fn remove(&self, id: &PluginId) -> Result<(), RepositoryError> {
            self.records
                .lock()
                .expect("lock")
                .remove(id.as_str())
                .map(|_| ())
                .ok_or_else(|| RepositoryError::NotFound(id.to_string()))
        }
    }

    fn record(id: &str, enabled: bool, approved: &[Capability]) -> PluginRecord {
        let manifest = Manifest::from_json(
            format!(
                r#"{{"schema":1,"id":"{id}","name":"Demo","version":"0.1.0","api":1,
                    "entry":{{"type":"process","cmd":"bin/demo"}},
                    "capabilities":["events","storage:kv","server:read","console:read"],
                    "license":"MIT"}}"#
            )
            .as_bytes(),
        )
        .expect("manifest");
        PluginRecord {
            id: PluginId::parse(id).expect("id"),
            version: "0.1.0".to_owned(),
            api: 1,
            scope: PluginScope::Machine,
            server: None,
            enabled,
            approved: approved.iter().copied().collect(),
            manifest: manifest.raw,
            artifact_dir: PathBuf::from("/tmp/demo"),
            installed_at: 0,
            updated_at: 0,
            last_state: PluginState::Disabled,
            last_error: None,
            previous_version: None,
            source: None,
            sha256: None,
            blocked: false,
            quarantined: false,
        }
    }

    fn service(store: Arc<MemoryStore>) -> PluginService {
        PluginService::new(
            store,
            Arc::new(MemoryConfigStore::default()),
            None,
            None,
            None,
            Arc::new(port::plugin::NoopAuditSink),
            PathBuf::from("/tmp/plugins"),
            PathBuf::from("/tmp/plugin-data"),
            PathBuf::from("/tmp/logs"),
        )
    }

    #[test]
    fn install_state_follows_enabled_flag() {
        // 升级已启用的插件：状态必须可直接 `start`，不能再落回 `disabled`
        assert_eq!(state_after_install(true, false), PluginState::Stopped);
        assert_eq!(
            state_after_install(true, true),
            PluginState::PendingApproval
        );
        assert_eq!(state_after_install(false, false), PluginState::Disabled);
        assert_eq!(
            state_after_install(false, true),
            PluginState::PendingApproval
        );
    }

    #[test]
    fn enable_requires_approval_for_medium_risk() {
        let store = Arc::new(MemoryStore::default());
        let mut pending = record("demo", false, &[]);
        pending.last_state = PluginState::PendingApproval;
        // console:read 是中风险，未授权时启用必须被拒
        pending.approved.insert(Capability::ServerRead);
        store.put(&pending).expect("put");
        let service = service(store.clone());
        let id = PluginId::parse("demo").expect("id");
        let err = service.enable(&id).expect_err("pending approval");
        assert_eq!(err.category(), ErrorCategory::Conflict);
        service
            .grant(&id, Capability::Events)
            .expect("grant events");
        service.grant(&id, Capability::StorageKv).expect("grant kv");
        service
            .grant(&id, Capability::ConsoleRead)
            .expect("grant console");
        let enabled = service.enable(&id).expect("enable");
        assert!(enabled.enabled);
        assert_eq!(enabled.last_state, PluginState::Stopped);
    }

    #[test]
    fn revoke_returns_to_pending_approval() {
        let store = Arc::new(MemoryStore::default());
        let mut ready = record(
            "demo",
            true,
            &[
                Capability::Events,
                Capability::StorageKv,
                Capability::ServerRead,
                Capability::ConsoleRead,
            ],
        );
        ready.last_state = PluginState::Stopped;
        store.put(&ready).expect("put");
        let service = service(store);
        let id = PluginId::parse("demo").expect("id");
        let record = service
            .revoke(&id, Capability::ConsoleRead)
            .expect("revoke");
        assert_eq!(record.last_state, PluginState::PendingApproval);
        assert!(record.needs_approval());
    }

    #[test]
    fn grant_rejects_undeclared_capability() {
        let store = Arc::new(MemoryStore::default());
        store.put(&record("demo", false, &[])).expect("put");
        let service = service(store);
        let id = PluginId::parse("demo").expect("id");
        let err = service
            .grant(&id, Capability::Java)
            .expect_err("undeclared");
        assert_eq!(err.category(), ErrorCategory::Usage);
    }

    #[test]
    fn remove_requires_disabled() {
        let store = Arc::new(MemoryStore::default());
        store.put(&record("demo", true, &[])).expect("put");
        let service = service(store);
        let id = PluginId::parse("demo").expect("id");
        assert_eq!(
            service.remove(&id).expect_err("enabled").category(),
            ErrorCategory::Conflict
        );
    }

    fn config_service_with_store(dir: &Path) -> (PluginService, PluginId, Arc<MemoryConfigStore>) {
        let id = PluginId::parse("demo").expect("id");
        std::fs::write(
            dir.join("descriptor.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema": 1,
                "plugin_id": "demo",
                "api": 1,
                "config": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "interval_secs": { "type": "integer", "minimum": 5, "default": 60 },
                        "token": { "type": "string", "x-mcsup-secret": true },
                        "qq": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "appId": { "type": "string" },
                                "clientSecret": { "type": "string", "x-mcsup-secret": true },
                                "groups": { "type": "array", "items": { "type": "string" } }
                            }
                        }
                    }
                }
            }))
            .expect("descriptor json"),
        )
        .expect("write descriptor");
        let store: Arc<dyn PluginStore> = Arc::new(MemoryStore::default());
        let mut stored = record("demo", false, &[]);
        stored.artifact_dir = dir.to_path_buf();
        store.put(&stored).expect("put");
        let config_store = Arc::new(MemoryConfigStore::default());
        let service = PluginService::new(
            Arc::clone(&store),
            Arc::clone(&config_store) as Arc<dyn PluginConfigStore>,
            None,
            None,
            None,
            Arc::new(port::plugin::NoopAuditSink),
            dir.join("plugins"),
            dir.join("plugin-data"),
            dir.join("logs"),
        );
        (service, id, config_store)
    }

    fn config_service(dir: &Path) -> (PluginService, PluginId) {
        let (service, id, _) = config_service_with_store(dir);
        (service, id)
    }

    #[test]
    fn config_get_starts_from_schema_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (service, id) = config_service(dir.path());
        let config = service.config_get(&id).expect("config");
        assert_eq!(config["revision"], 0);
        assert_eq!(config["values"]["interval_secs"], 60);
    }

    #[test]
    fn config_set_merges_validates_and_bumps_revision() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (service, id) = config_service(dir.path());
        let config = service
            .config_set(&id, &json!({ "interval_secs": 30 }))
            .expect("set");
        assert_eq!(config["revision"], 1);
        assert_eq!(config["values"]["interval_secs"], 30);
        assert_eq!(
            service.config_get(&id).expect("get")["values"]["interval_secs"],
            30
        );
    }

    /// 安装向导逐项写嵌套字段：只覆盖目标键，同级配置与已存密钥保持不变
    #[test]
    fn config_merge_keeps_nested_siblings_and_secret() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (service, id, store) = config_service_with_store(dir.path());
        service
            .config_set(
                &id,
                &json!({
                    "interval_secs": 30,
                    "qq": { "appId": "app-1", "clientSecret": "s3cret", "groups": ["g1"] }
                }),
            )
            .expect("seed");
        let config = service
            .config_merge(&id, &json!({ "qq": { "groups": ["g2"] } }))
            .expect("merge");
        // 数组整体替换，嵌套对象的其它键保留
        assert_eq!(config["values"]["qq"]["groups"], json!(["g2"]));
        assert_eq!(config["values"]["qq"]["appId"], "app-1");
        assert_eq!(config["values"]["interval_secs"], 30);
        // 读取打码；落盘值仍是原文，`***` 不会被写回
        assert_eq!(config["values"]["qq"]["clientSecret"], "***");
        assert_eq!(config["revision"], 2);
        let stored = store.get(&id).expect("get").expect("stored");
        assert_eq!(stored.values["qq"]["clientSecret"], "s3cret");
    }

    #[test]
    fn invalid_config_is_rejected_without_writing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (service, id) = config_service(dir.path());
        let err = service
            .config_set(&id, &json!({ "interval_secs": 1 }))
            .expect_err("below minimum");
        assert_eq!(err.category(), ErrorCategory::Usage);
        assert_eq!(service.config_get(&id).expect("get")["revision"], 0);
    }

    #[test]
    fn secret_config_values_are_masked_on_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (service, id) = config_service(dir.path());
        service
            .config_set(&id, &json!({ "token": "s3cret" }))
            .expect("set secret");
        let config = service.config_get(&id).expect("get");
        assert_eq!(config["values"]["token"], "***");
    }

    #[test]
    fn blocked_plugin_cannot_be_enabled() {
        let store = Arc::new(MemoryStore::default());
        store
            .put(&record("demo", false, &all_capabilities()))
            .expect("put");
        let service = service(store);
        let id = PluginId::parse("demo").expect("id");
        let blocked = service.block(&id).expect("block");
        assert!(blocked.blocked);
        assert_eq!(
            service.enable(&id).expect_err("blocked").category(),
            ErrorCategory::Conflict
        );
        let released = service.unblock(&id).expect("unblock");
        assert!(!released.blocked);
        service.enable(&id).expect("enable after unblock");
    }

    #[test]
    fn quarantined_plugin_cannot_be_enabled() {
        let store = Arc::new(MemoryStore::default());
        store
            .put(&record("demo", false, &all_capabilities()))
            .expect("put");
        let service = service(store);
        let id = PluginId::parse("demo").expect("id");
        service.quarantine(&id).expect("quarantine");
        assert_eq!(
            service.enable(&id).expect_err("quarantined").category(),
            ErrorCategory::Conflict
        );
        service.release(&id).expect("release");
        service.enable(&id).expect("enable after release");
    }

    // ---- 文件能力（API-FS） ----

    fn files(root: &Path) -> ServerFiles {
        ServerFiles::new(root, false, None).expect("files")
    }

    fn write_file(root: &Path, rel: &str, bytes: &[u8]) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent");
        }
        std::fs::write(path, bytes).expect("write");
    }

    #[test]
    fn api_fs_001_relative_read_and_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let files = files(dir.path());
        files
            .write(Path::new("world/level.dat"), b"level", false)
            .expect("write");
        assert_eq!(
            files.read(Path::new("./world/level.dat")).expect("read"),
            b"level"
        );
        assert_eq!(
            files
                .write(Path::new("world/level.dat"), b"next", false)
                .expect_err("exists")
                .code,
            port::plugin::ErrorCode::Conflict
        );
        files
            .write(Path::new("world/level.dat"), b"next", true)
            .expect("overwrite");
        assert_eq!(
            files.read(Path::new("world/level.dat")).expect("read"),
            b"next"
        );
        let root = files.list(Path::new("")).expect("list root");
        assert!(
            root.iter()
                .any(|entry| entry.path == Path::new("world") && entry.is_dir)
        );
        let world = files.list(Path::new("world")).expect("list world");
        assert_eq!(world.len(), 1);
        assert_eq!(world[0].path, Path::new("world").join("level.dat"));
        assert_eq!(world[0].size, 4);
    }

    #[test]
    fn api_fs_002_traversal_is_denied() {
        let dir = tempfile::tempdir().expect("tempdir");
        let files = files(dir.path());
        for path in ["../escape", "world/../../escape"] {
            assert_eq!(
                files
                    .read(Path::new(path))
                    .expect_err("read traversal")
                    .code,
                port::plugin::ErrorCode::Denied
            );
            assert_eq!(
                files
                    .write(Path::new(path), b"x", true)
                    .expect_err("write traversal")
                    .code,
                port::plugin::ErrorCode::Denied
            );
        }
        assert!(!dir.path().parent().expect("parent").join("escape").exists());
    }

    #[test]
    fn api_fs_003_absolute_path_is_denied() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside");
        let target = outside.path().join("secret.txt");
        std::fs::write(&target, b"secret").expect("secret");
        let files = files(dir.path());
        assert_eq!(
            files.read(&target).expect_err("absolute read").code,
            port::plugin::ErrorCode::Denied
        );
        assert_eq!(
            files
                .write(&target, b"changed", true)
                .expect_err("absolute write")
                .code,
            port::plugin::ErrorCode::Denied
        );
        assert_eq!(std::fs::read(&target).expect("unchanged"), b"secret");
    }

    #[cfg(unix)]
    #[test]
    fn api_fs_004_symlink_escape_is_denied() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret.txt"), b"secret").expect("secret");
        symlink(outside.path(), dir.path().join("link")).expect("symlink");
        let files = files(dir.path());
        assert_eq!(
            files
                .read(Path::new("link/secret.txt"))
                .expect_err("read escape")
                .code,
            port::plugin::ErrorCode::Denied
        );
        assert_eq!(
            files
                .write(Path::new("link/new/deeper.txt"), b"x", true)
                .expect_err("write escape")
                .code,
            port::plugin::ErrorCode::Denied
        );
        assert!(
            !outside.path().join("new").exists(),
            "must not create outside"
        );
        assert_eq!(
            files
                .remove(Path::new("link/secret.txt"))
                .expect_err("remove escape")
                .code,
            port::plugin::ErrorCode::Denied
        );
        assert!(outside.path().join("secret.txt").exists());

        std::fs::write(dir.path().join("real.txt"), b"inside").expect("real");
        symlink(dir.path().join("real.txt"), dir.path().join("alias.txt")).expect("alias");
        assert_eq!(
            files.read(Path::new("alias.txt")).expect("inside link"),
            b"inside"
        );
    }

    #[test]
    fn api_fs_005_file_size_limit_boundary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let files = files(dir.path());
        let exact = vec![7_u8; MAX_FILE_BYTES];
        files
            .write(Path::new("exact.bin"), &exact, false)
            .expect("exact");
        assert_eq!(
            files.read(Path::new("exact.bin")).expect("read").len(),
            exact.len()
        );
        assert_eq!(
            files
                .write(
                    Path::new("over.bin"),
                    &vec![0_u8; MAX_FILE_BYTES + 1],
                    false
                )
                .expect_err("write over limit")
                .code,
            port::plugin::ErrorCode::LimitExceeded
        );
        std::fs::write(dir.path().join("over.bin"), vec![0_u8; MAX_FILE_BYTES + 1])
            .expect("write over");
        assert_eq!(
            files
                .read(Path::new("over.bin"))
                .expect_err("read over limit")
                .code,
            port::plugin::ErrorCode::LimitExceeded
        );
    }

    #[test]
    fn api_fs_006_marker_is_reserved() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("mcsup.json"), b"{}").expect("marker");
        let files = files(dir.path());
        assert_eq!(
            files
                .write(Path::new("mcsup.json"), b"{}", true)
                .expect_err("marker write")
                .code,
            port::plugin::ErrorCode::Denied
        );
        assert_eq!(
            files
                .write(Path::new("MCSup.json"), b"{}", true)
                .expect_err("marker case")
                .code,
            port::plugin::ErrorCode::Denied
        );
        assert_eq!(
            files
                .remove(Path::new("mcsup.json"))
                .expect_err("marker remove")
                .code,
            port::plugin::ErrorCode::Denied
        );
        assert_eq!(files.read(Path::new("mcsup.json")).expect("read"), b"{}");
        files
            .write(Path::new("world/mcsup.json"), b"{}", true)
            .expect("nested marker name is not reserved");
    }

    #[test]
    fn api_fs_007_running_launch_file_conflicts() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("server.jar"), b"jar").expect("jar");
        let stopped = files(dir.path());
        stopped
            .write(Path::new("server.jar"), b"new", true)
            .expect("stopped jar is writable");

        let running =
            ServerFiles::new(dir.path(), true, Some(PathBuf::from("server.jar"))).expect("running");
        assert_eq!(
            running
                .write(Path::new("server.jar"), b"new", true)
                .expect_err("running jar write")
                .code,
            port::plugin::ErrorCode::Conflict
        );
        assert_eq!(
            running
                .write(Path::new("SERVER.JAR"), b"new", true)
                .expect_err("running jar case")
                .code,
            port::plugin::ErrorCode::Conflict
        );
        assert_eq!(
            running
                .remove(Path::new("server.jar"))
                .expect_err("running jar remove")
                .code,
            port::plugin::ErrorCode::Conflict
        );
        assert_eq!(
            running.read(Path::new("server.jar")).expect("read jar"),
            b"new"
        );
    }

    #[test]
    fn api_fs_008_windows_forms_are_denied_everywhere() {
        let dir = tempfile::tempdir().expect("tempdir");
        let files = files(dir.path());
        for path in [
            r"\\server\share\file",
            "//server/share/file",
            r"C:\evil",
            "C:/evil",
            "C:evil",
            r"world\level.dat",
            "CON",
            "world/trail.",
        ] {
            assert_eq!(
                files
                    .write(Path::new(path), b"x", true)
                    .expect_err("windows form")
                    .code,
                port::plugin::ErrorCode::Denied,
                "{path}"
            );
        }
    }

    #[test]
    fn api_fs_list_and_remove() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_file(dir.path(), "a.txt", b"a");
        std::fs::create_dir(dir.path().join("empty")).expect("empty dir");
        std::fs::create_dir(dir.path().join("full")).expect("full dir");
        std::fs::write(dir.path().join("full/b.txt"), b"b").expect("b");
        let files = files(dir.path());
        let root = files.list(Path::new(".")).expect("list");
        assert_eq!(root.len(), 3);
        assert!(
            root.iter()
                .any(|entry| entry.path == Path::new("full") && entry.is_dir)
        );
        assert_eq!(
            files.list(Path::new("a.txt")).expect_err("file list").code,
            port::plugin::ErrorCode::Conflict
        );
        assert!(files.remove(Path::new("a.txt")).expect("remove file"));
        assert!(!files.remove(Path::new("a.txt")).expect("missing"));
        assert_eq!(
            files.remove(Path::new("full")).expect_err("non-empty").code,
            port::plugin::ErrorCode::Conflict
        );
        assert!(files.remove(Path::new("empty")).expect("empty dir"));
        assert!(!dir.path().join("empty").exists());
    }

    // ---- 生命周期与备份错误映射（API-LIFE / API-BAK） ----

    #[test]
    fn api_life_errors_map_to_host_codes() {
        let conflict = |err: LifecycleError| {
            assert_eq!(
                lifecycle_host_error(&err).code,
                port::plugin::ErrorCode::Conflict
            );
        };
        conflict(LifecycleError::AlreadyRunning);
        conflict(LifecycleError::NotRunning);
        conflict(LifecycleError::EulaRequired);
        conflict(LifecycleError::NoJar {
            name: "survival".to_owned(),
        });
        assert_eq!(
            lifecycle_host_error(&LifecycleError::StopTimedOut {
                waited: Duration::from_secs(30),
            })
            .code,
            port::plugin::ErrorCode::Timeout
        );
    }

    #[test]
    fn api_bak_errors_map_to_host_codes() {
        assert_eq!(
            backup_host_error(&BackupError::SnapshotNotFound("missing".to_owned())).code,
            port::plugin::ErrorCode::NotFound
        );
        assert_eq!(
            backup_host_error(&BackupError::RestoreWhileRunning {
                name: "survival".to_owned(),
            })
            .code,
            port::plugin::ErrorCode::Conflict
        );
        assert_eq!(
            backup_host_error(&BackupError::EmptyScope).code,
            port::plugin::ErrorCode::InvalidInput
        );
    }

    #[test]
    fn api_java_errors_map_to_host_codes() {
        assert_eq!(
            java_host_error(&JavaError::NotFound("/opt/java/17".to_owned())).code,
            port::plugin::ErrorCode::NotFound
        );
        assert_eq!(
            java_host_error(&JavaError::Scan("permission denied".to_owned())).code,
            port::plugin::ErrorCode::Unavailable
        );
    }

    #[test]
    fn api_cnt_errors_map_to_host_codes() {
        assert_eq!(
            content_host_error(&ModError::Running {
                name: "survival".to_owned(),
            })
            .code,
            port::plugin::ErrorCode::Conflict
        );
        assert_eq!(
            content_host_error(&ModError::NotFound {
                selector: "jei".to_owned(),
                dir: PathBuf::from("/srv/mods"),
            })
            .code,
            port::plugin::ErrorCode::NotFound
        );
        assert_eq!(
            content_host_error(&ModError::Remote {
                name: "survival".to_owned(),
            })
            .code,
            port::plugin::ErrorCode::Unsupported
        );
        assert_eq!(
            content_host_error(&ModError::ClientOnly {
                kind: ContentKind::Shader,
            })
            .code,
            port::plugin::ErrorCode::InvalidInput
        );
    }

    /// 卸载的三种落点：默认连数据一起删、`Keep` 保留目录但删密钥、`Keep { secrets: true }` 全留
    ///
    /// 密钥是凭据：同名插件（可能是别人发布的同名包）重装后不该继承。所以
    /// 保留数据目录时，`secrets.json` 仍默认删掉。
    #[test]
    fn remove_handles_data_by_mode() {
        // (mode, secrets_left, data_left)
        let cases = [
            (RemoveData::Delete, false, false),
            (RemoveData::Keep { secrets: false }, false, true),
            (RemoveData::Keep { secrets: true }, true, true),
        ];
        for (mode, secrets_left, data_left) in cases {
            let label = format!("{mode:?}");
            let dir = tempfile::tempdir().expect("tempdir");
            let plugins_root = dir.path().join("plugins");
            let plugin_data = dir.path().join("plugin-data");
            let log_root = dir.path().join("logs");
            let id = PluginId::parse("demo").expect("id");
            let mut demo = record("demo", false, &[]);
            demo.last_state = PluginState::Disabled;
            demo.artifact_dir = plugins_root.join("demo");
            std::fs::create_dir_all(demo.artifact_dir.join("bin")).expect("artifact");
            std::fs::write(demo.artifact_dir.join("bin/plugin"), b"binary").expect("write entry");
            std::fs::create_dir_all(plugin_data.join("demo/kv")).expect("data dir");
            std::fs::write(plugin_data.join("demo/secrets.json"), b"{}").expect("secrets");
            std::fs::write(plugin_data.join("demo/kv/k.json"), b"{}").expect("kv");
            std::fs::create_dir_all(&log_root).expect("logs");
            std::fs::write(log_root.join("demo.log"), b"log").expect("log file");
            let store = Arc::new(MemoryStore::default());
            store.put(&demo).expect("put");

            let service = PluginService::new(
                store,
                Arc::new(MemoryConfigStore::default()),
                None,
                None,
                None,
                Arc::new(port::plugin::NoopAuditSink),
                plugins_root.clone(),
                plugin_data.clone(),
                log_root.clone(),
            );
            service.remove_with(&id, mode).expect("remove");

            assert_eq!(
                plugin_data.join("demo/secrets.json").exists(),
                secrets_left,
                "{label}: secrets"
            );
            assert_eq!(
                plugin_data.join("demo/kv/k.json").exists(),
                data_left,
                "{label}: kv"
            );
            assert_eq!(
                plugin_data.join("demo").exists(),
                data_left,
                "{label}: data dir"
            );
            assert!(
                !plugins_root.join("demo").exists(),
                "{label}: artifact must be gone"
            );
            assert_eq!(
                log_root.join("demo.log").exists(),
                data_left,
                "{label}: log"
            );
        }
    }

    fn all_capabilities() -> [Capability; 4] {
        [
            Capability::Events,
            Capability::StorageKv,
            Capability::ServerRead,
            Capability::ConsoleRead,
        ]
    }
}
