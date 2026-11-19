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
use crate::lifecycle::{LifecycleError, ServerLifecycleService, StartOptions};
use crate::mod_::{ApplyOptions, InstallSpec, ListOptions, ModError, ModService};
use crate::props::ServerPropsService;
use crate::registry::{ServerRegistry, ServerRegistryError};
use domain::backup::{Page, RestoreTarget, RetentionInput};
use domain::path::RelPath;
use domain::plugin::capability::{self, Capability};
use domain::plugin::descriptor::Descriptor;
use domain::plugin::error::PluginErrorCode;
use domain::plugin::manifest::Manifest;
use domain::plugin::state::{PluginEvent, PluginState};
use domain::plugin::{PluginId, PluginScope};
use domain::server::artifact::Checksum;
use domain::server::content::ContentKind;
use domain::server::layout::console_log_path;
use domain::server::meta::ServerName;
use domain::server::status::ServerStatus;
use port::plugin::{
    ArtifactOrigin, ArtifactSource, ArtifactVerifier, AuditEvent, AuditResult, AuditSink,
    BackupEntry, BackupOps, ConfigOps, ConsoleIo, ConsoleLine, ConsolePage, ContentEntry,
    ContentInstallRequest, ContentOps, FileEntry, FileOps, HostError, JavaOps, KvStore,
    MAX_CONSOLE_LINE_BYTES, MAX_CONSOLE_LINES, MAX_FILE_BYTES, NetPolicy, PluginConfigStore,
    PluginRecord, PluginRuntimeStatus, PluginStore, PluginSupervisor, SecretStore, ServerLifecycle,
    ServerRead, ServerStatusView, ServerSummary, SessionInfo, StoredConfig,
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
        }
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
        let declared = record.declared_capabilities();
        if !declared.contains(&capability) {
            return Err(PluginError::Usage(format!(
                "capability {capability} is not declared by plugin {id}"
            )));
        }
        if !capability.is_open() {
            return Err(PluginError::Usage(format!(
                "capability {capability} is not available in this version"
            )));
        }
        record.approved.insert(capability);
        if record.needs_approval() {
            // 仍有待确认项
        } else if record.last_state == PluginState::PendingApproval {
            record.last_state = PluginState::Disabled;
        }
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
        if !record.approved.remove(&capability) {
            return Err(PluginError::NotFound(format!(
                "capability {capability} is not granted to plugin {id}"
            )));
        }
        if let Some(supervisor) = &self.supervisor {
            // 会话内的授予集合同步收缩；宿主不在时忽略（下次握手自然按记录计算）。
            match supervisor.revoke(id, capability) {
                Ok(()) => {}
                Err(err) if err.code == port::plugin::ErrorCode::Unavailable => {}
                Err(err) => return Err(err.into()),
            }
        }
        // 撤销后若重新出现待确认能力，状态回到 `pending_approval`。
        if record.needs_approval() {
            record.last_state = PluginState::PendingApproval;
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
    /// 流水线：拉取 → 校验 → 解包（由来源实现）→ manifest / descriptor →
    /// 原子移动到 `plugins/<id>/<version>` → 写记录。安装期间不启动进程。
    ///
    /// # Errors
    ///
    /// 来源不可用、校验失败、manifest / descriptor 非法、同版本已存在时返回
    /// [`PluginError`]
    pub fn install(
        &self,
        origin: &ArtifactOrigin,
        expected_sha256: Option<&Checksum>,
        allow_unsigned: bool,
    ) -> Result<InstallOutcome, PluginError> {
        let source = self.source.as_ref().ok_or_else(|| {
            PluginError::Usage("no plugin artifact source is configured".to_owned())
        })?;
        let source = Arc::clone(source);
        std::fs::create_dir_all(&self.plugins_root)?;
        let temp = self.plugins_root.join(format!(
            "{TEMP_PREFIX}{}-{}",
            std::process::id(),
            now_secs()
        ));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp)?;
        let result = self.install_into(origin, expected_sha256, allow_unsigned, &source, &temp);
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&temp);
        }
        result
    }

    fn install_into(
        &self,
        origin: &ArtifactOrigin,
        expected_sha256: Option<&Checksum>,
        allow_unsigned: bool,
        source: &Arc<dyn ArtifactSource>,
        temp: &Path,
    ) -> Result<InstallOutcome, PluginError> {
        let fetched = source.fetch(origin, temp)?;
        let artifact = if let Some(verifier) = &self.verifier {
            verifier.verify(&fetched.root, expected_sha256, allow_unsigned)?
        } else {
            if !fetched.trusted && !allow_unsigned {
                return Err(PluginError::Denied(
                    "artifact is not signed; pass --allow-unsigned to install unsigned local builds"
                        .to_owned(),
                ));
            }
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
        let id = manifest.id.clone();
        let version = manifest.raw.version.clone();
        let target = self.plugins_root.join(id.as_str()).join(&version);
        if target.exists() {
            return Err(PluginError::AlreadyExists(format!(
                "{id} {version} is already installed"
            )));
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if manifest.scope == PluginScope::Server {
            return Err(PluginError::Usage(
                "server-scoped plugins require `--server <name>`".to_owned(),
            ));
        }
        // 同文件系统内 rename 才是原子的：临时目录建在 `plugins_root` 下。
        std::fs::rename(&artifact.root, &target)
            .map_err(|err| PluginError::Fs(FsError::io(&artifact.root, err)))?;
        let _ = std::fs::remove_dir_all(temp);
        // 升级：只保留新版本仍声明、且此前已确认的能力；低风险照常自动通过。
        let previous = self.store.get(&id).ok();
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
        let record = PluginRecord {
            id: id.clone(),
            version: version.clone(),
            api: manifest.raw.api,
            scope: manifest.scope,
            server: None,
            enabled: false,
            approved,
            manifest: manifest.raw.clone(),
            artifact_dir: target,
            installed_at: now,
            updated_at: now,
            last_state: PluginState::after_install(!pending.is_empty()),
            last_error: None,
            previous_version: previous.as_ref().map(|record| record.version.clone()),
            source: Some(origin_text(origin)),
            sha256: Some(artifact.sha256),
            signed: artifact.signed,
            blocked: false,
            quarantined: false,
        };
        self.store.put(&record)?;
        self.audit.record(&AuditEvent {
            at: now,
            plugin: id.to_string(),
            action: "install".to_owned(),
            capability: None,
            result: AuditResult::Allowed,
            detail: Some(format!("version {version}")),
        });
        Ok(InstallOutcome { record, pending })
    }

    /// 回滚到上一版本
    ///
    /// # Errors
    ///
    /// 没有上一版本、上一版本产物缺失时返回 [`PluginError`]
    pub fn rollback(&self, id: &PluginId) -> Result<PluginRecord, PluginError> {
        let mut record = self.record(id)?;
        let previous = record
            .previous_version
            .clone()
            .ok_or_else(|| PluginError::Conflict(format!("plugin {id} has no previous version")))?;
        let artifact = self.plugins_root.join(id.as_str()).join(&previous);
        if !artifact.is_dir() {
            return Err(PluginError::Conflict(format!(
                "artifact for {id} {previous} is missing"
            )));
        }
        let was_running = self
            .supervisor
            .as_ref()
            .and_then(|supervisor| supervisor.status(id).ok())
            .is_some_and(|status| status.session.is_some());
        if was_running && let Some(supervisor) = &self.supervisor {
            supervisor.stop(id, Duration::from_secs(10))?;
        }
        let current = record.version.clone();
        record.previous_version = Some(current);
        record.version = previous;
        record.artifact_dir = artifact;
        record.last_state = PluginState::Stopped;
        record.last_error = None;
        record.updated_at = now_secs();
        self.store.put(&record)?;
        self.audit.record(&AuditEvent {
            at: now_secs(),
            plugin: id.to_string(),
            action: "rollback".to_owned(),
            capability: None,
            result: AuditResult::Allowed,
            detail: Some(format!("active version {}", record.version)),
        });
        if was_running && record.enabled {
            self.start(id)?;
        }
        Ok(record)
    }

    /// 卸载：要求已停用且进程退出
    ///
    /// `purge` 同时删除 `plugin-data/<id>` 与日志；默认保留数据。
    ///
    /// # Errors
    ///
    /// 仍启用或进程仍在运行时返回 [`PluginError::Conflict`]
    pub fn remove(&self, id: &PluginId, purge: bool) -> Result<(), PluginError> {
        let record = self.record(id)?;
        if let Some(supervisor) = &self.supervisor
            && let Ok(status) = supervisor.status(id)
            && status.session.is_some()
        {
            return Err(PluginError::Conflict(format!(
                "plugin {id} is running; run `mcsup plugin disable {id}` first"
            )));
        }
        if record.enabled {
            return Err(PluginError::Conflict(format!(
                "plugin {id} is enabled; run `mcsup plugin disable {id}` first"
            )));
        }
        self.store.remove(id)?;
        let artifact = self.plugins_root.join(id.as_str());
        if artifact.exists() {
            std::fs::remove_dir_all(&artifact)?;
        }
        if purge {
            let data = self.plugin_data.join(id.as_str());
            if data.exists() {
                std::fs::remove_dir_all(&data)?;
            }
            let log = self.log_root.join(format!("{id}.log"));
            if log.exists() {
                std::fs::remove_file(&log)?;
            }
        }
        self.audit.record(&AuditEvent {
            at: now_secs(),
            plugin: id.to_string(),
            action: "remove".to_owned(),
            capability: None,
            result: AuditResult::Allowed,
            detail: Some(format!("purge={purge}")),
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
        let record = self.record(id)?;
        let schema = Self::schema(&record)?.ok_or_else(|| {
            HostError::unsupported(format!("plugin {id} does not declare a config schema"))
        })?;
        let patch = values
            .as_object()
            .ok_or_else(|| HostError::invalid_input("config values must be an object"))?;
        let previous = self.current(id, &schema)?;
        let mut merged = previous.values.clone();
        let Some(object) = merged.as_object_mut() else {
            return Err(HostError::internal("stored config is not an object"));
        };
        for (key, value) in patch {
            object.insert(key.clone(), value.clone());
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
        self.audit.record(&AuditEvent {
            at: now_secs(),
            plugin: id.to_string(),
            action: "config:set".to_owned(),
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

fn origin_text(origin: &ArtifactOrigin) -> String {
    match origin {
        ArtifactOrigin::Local(path) => format!("local:{}", path.display()),
        ArtifactOrigin::Url(url) => format!("url:{url}"),
        ArtifactOrigin::Git { url, rev } => match rev {
            Some(rev) => format!("git:{url}@{rev}"),
            None => format!("git:{url}"),
        },
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
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
        let text = match std::fs::read(&path) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(HostError::internal(format!("read console log: {err}"))),
        };
        // 游标是字节偏移：单调递增、可跨进程复用，且无需额外状态文件。
        let start = cursor.unwrap_or(u64::MAX);
        let mut lines = Vec::new();
        let mut offset = 0_u64;
        for line in text.split_inclusive('\n') {
            let end = offset + line.len() as u64;
            if start == u64::MAX {
                // `None`：从尾部开始，保留最后 `max_lines` 行
                lines.push(ConsoleLine {
                    cursor: end,
                    text: line.trim_end_matches(['\r', '\n']).to_owned(),
                });
                if lines.len() > max_lines {
                    lines.remove(0);
                }
            } else if end > start {
                lines.push(ConsoleLine {
                    cursor: end,
                    text: line.trim_end_matches(['\r', '\n']).to_owned(),
                });
                if lines.len() >= max_lines {
                    break;
                }
            }
            offset = end;
        }
        let next = lines.last().map_or(start.min(offset), |line| line.cursor);
        Ok(ConsolePage { lines, next })
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
        RelPath::from_path(rel).map_err(|err| HostError::denied(err.to_string()))
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
            signed: false,
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
            service.remove(&id, false).expect_err("enabled").category(),
            ErrorCategory::Conflict
        );
    }

    fn config_service(dir: &Path) -> (PluginService, PluginId) {
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
                        "token": { "type": "string", "x-mcsup-secret": true }
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
        let service = PluginService::new(
            Arc::clone(&store),
            Arc::new(MemoryConfigStore::default()),
            None,
            None,
            None,
            Arc::new(port::plugin::NoopAuditSink),
            dir.join("plugins"),
            dir.join("plugin-data"),
            dir.join("logs"),
        );
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

    fn all_capabilities() -> [Capability; 4] {
        [
            Capability::Events,
            Capability::StorageKv,
            Capability::ServerRead,
            Capability::ConsoleRead,
        ]
    }
}
